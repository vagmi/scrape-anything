//! Multi-provider LLM client over `genai`, with failover. The agentic loop
//! (turn-by-turn calls with tool schemas) is built in `session.rs`; this module
//! turns `[[llm.providers]]` into configured `genai::Client`s and exposes
//! `chat`, which:
//!
//!   - retries the transient failures a hosted model throws now and then (a 502
//!     from the gateway, a 503 while it's overloaded, a request that hangs past
//!     `request_timeout_secs`) with backoff, on the same provider;
//!   - fails over to the next provider when one stays down, rejects the request,
//!     or answers with nothing at all (Gemini sometimes returns an empty
//!     candidate — no text, no tool calls — again and again);
//!   - puts a provider that failed over into a cooldown, so the following steps
//!     go straight to the next one instead of waiting on it again.
//!
//! The session history is provider-neutral (`agent::build_messages`), so a
//! session can switch providers mid-run. `for_provider` adapts the replayed
//! history to the target: Gemini's synthetic tool-call ids (`call#fn#0`, which
//! repeat every turn and contain `#`) and its thought signatures mean nothing to
//! other providers, so for them ids are rewritten and signatures dropped.

use anyhow::{Context, Result, anyhow};
use genai::adapter::AdapterKind;
use genai::chat::{ChatOptions, ChatRequest, ChatResponse, ContentPart, MessageContent, ReasoningEffort};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ModelMapper, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget, webc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::{Llm, LlmProvider};

pub struct LlmClient {
    providers: Vec<Provider>,
    retry: RetryPolicy,
    request_timeout: Duration,
    cooldown: Duration,
    /// Per provider (same index): skip it until this instant.
    down_until: Mutex<Vec<Option<Instant>>>,
}

struct Provider {
    name: String,
    model: String,
    adapter: AdapterKind,
    client: Client,
    options: ChatOptions,
}

/// A successful step: the response and who produced it.
pub struct Answer {
    pub response: ChatResponse,
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Copy, Debug)]
struct RetryPolicy {
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
}

/// Why one provider's attempt failed.
enum Failure {
    /// Worth retrying on the same provider (`Some` = the provider's Retry-After).
    Transient(String, Option<Duration>),
    /// Answered with neither text nor tool calls.
    Empty,
    /// Retrying won't help here (bad request, auth, unparseable response) —
    /// but another provider may well succeed.
    Permanent(String),
}

impl LlmClient {
    /// Build a client per configured provider. A provider whose API key env var
    /// is unset is skipped with a warning, so the config can list fallbacks the
    /// local machine has no key for; at least one must be usable.
    pub fn from_config(cfg: &Llm) -> Result<Self> {
        let mut providers = Vec::new();
        for p in cfg.provider_list() {
            let name = p.name.clone().unwrap_or_else(|| p.kind.clone());
            match build_provider(&p) {
                Ok(provider) => {
                    tracing::info!(provider = %name, model = %p.model, "llm provider ready");
                    providers.push(provider);
                }
                Err(e) => tracing::warn!(provider = %name, model = %p.model, "llm provider skipped: {e:#}"),
            }
        }
        if providers.is_empty() {
            return Err(anyhow!(
                "no usable llm provider — configure [[llm.providers]] and set at least one's API key env var"
            ));
        }
        let n = providers.len();
        Ok(Self {
            providers,
            retry: RetryPolicy {
                max_retries: cfg.max_retries,
                base_delay: Duration::from_millis(cfg.retry_base_delay_ms),
                max_delay: Duration::from_millis(cfg.retry_max_delay_ms),
            },
            request_timeout: Duration::from_secs(cfg.request_timeout_secs),
            cooldown: Duration::from_secs(cfg.failover_cooldown_secs),
            down_until: Mutex::new(vec![None; n]),
        })
    }

    /// `provider/model` of each configured provider, in priority order.
    pub fn describe(&self) -> String {
        self.providers.iter().map(|p| format!("{}/{}", p.name, p.model)).collect::<Vec<_>>().join(", ")
    }

    /// Run one chat step, failing over across providers. `request_for` builds
    /// the request for a provider (by its configured name), since parts of the
    /// replayed history belong to one provider only. The error, if every
    /// provider failed, names each one's last failure.
    pub async fn chat(&self, request_for: impl Fn(&str) -> ChatRequest) -> Result<Answer, String> {
        let mut failures: Vec<String> = Vec::new();
        for idx in self.attempt_order() {
            let p = &self.providers[idx];
            let req = for_provider(request_for(&p.name), p.adapter);
            match self.chat_with_retries(p, req).await {
                Ok(response) => {
                    self.down_until.lock().unwrap()[idx] = None;
                    return Ok(Answer { response, provider: p.name.clone(), model: p.model.clone() });
                }
                Err(reason) => {
                    tracing::warn!(
                        target: "scrape_backend::llm",
                        provider = %p.name,
                        model = %p.model,
                        cooldown_secs = self.cooldown.as_secs(),
                        "provider failed ({}); failing over",
                        first_line(&reason)
                    );
                    self.down_until.lock().unwrap()[idx] = Some(Instant::now() + self.cooldown);
                    failures.push(format!("{}: {reason}", p.name));
                }
            }
        }
        Err(failures.join("; "))
    }

    /// Providers not in cooldown first (in priority order), then the cooling
    /// ones — if everything is down we still try rather than fail outright.
    fn attempt_order(&self) -> Vec<usize> {
        let now = Instant::now();
        let down = self.down_until.lock().unwrap();
        let (mut up, cooling): (Vec<usize>, Vec<usize>) =
            (0..self.providers.len()).partition(|&i| down[i].is_none_or(|t| t <= now));
        up.extend(cooling);
        up
    }

    async fn chat_with_retries(&self, p: &Provider, req: ChatRequest) -> Result<ChatResponse, String> {
        let mut attempt = 0;
        // One extra go for an empty answer before giving up on the provider.
        let mut empty_retries = 1;
        loop {
            let failure = match self.attempt(p, req.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(f) => f,
            };
            let (reason, retry_after) = match failure {
                Failure::Permanent(msg) => return Err(msg),
                Failure::Empty if empty_retries == 0 => return Err("empty response".into()),
                Failure::Empty => {
                    empty_retries -= 1;
                    ("empty response".to_string(), None)
                }
                Failure::Transient(_, _) if attempt >= self.retry.max_retries => {
                    let Failure::Transient(msg, _) = failure else { unreachable!() };
                    return Err(msg);
                }
                Failure::Transient(msg, retry_after) => (msg, retry_after),
            };
            let delay = retry_after.unwrap_or_else(|| self.retry.backoff(attempt));
            attempt += 1;
            tracing::warn!(
                target: "scrape_backend::llm",
                provider = %p.name,
                attempt,
                max_retries = self.retry.max_retries,
                delay_ms = delay.as_millis() as u64,
                error = %first_line(&reason),
                "transient llm failure; retrying"
            );
            tokio::time::sleep(delay).await;
        }
    }

    async fn attempt(&self, p: &Provider, req: ChatRequest) -> Result<ChatResponse, Failure> {
        let call = p.client.exec_chat(p.model.as_str(), req, Some(&p.options));
        let resp = match tokio::time::timeout(self.request_timeout, call).await {
            Err(_) => {
                return Err(Failure::Transient(
                    format!("no response within {}s", self.request_timeout.as_secs()),
                    None,
                ));
            }
            Ok(Err(e)) => {
                let msg = e.to_string();
                return Err(match transient(&e) {
                    Some(retry_after) => Failure::Transient(msg, retry_after),
                    None => Failure::Permanent(msg),
                });
            }
            Ok(Ok(resp)) => resp,
        };
        let has_text = resp.first_text().is_some_and(|t| !t.trim().is_empty());
        if !has_text && resp.content.tool_calls().is_empty() {
            return Err(Failure::Empty);
        }
        Ok(resp)
    }
}

// ─────────────────────────── provider wiring ────────────────────────────────

/// Wire format, default endpoint (`None` = genai's built-in one) and default
/// key env var for each supported `kind`. Endpoints keep the trailing `/`: the
/// adapter joins its path (`chat/completions`, `responses`) onto it.
fn kind_defaults(kind: &str) -> Result<(AdapterKind, Option<&'static str>, Option<&'static str>)> {
    Ok(match kind {
        "gemini" => (AdapterKind::Gemini, None, Some("GEMINI_API_KEY")),
        "anthropic" => (AdapterKind::Anthropic, None, Some("ANTHROPIC_API_KEY")),
        // The Responses API: OpenAI's current surface for its reasoning models.
        "openai" => (AdapterKind::OpenAIResp, None, Some("OPENAI_API_KEY")),
        // Meta Model API (Muse Spark) speaks the OpenAI Responses protocol.
        "meta" => (AdapterKind::OpenAIResp, Some("https://api.meta.ai/v1/"), Some("META_API_KEY")),
        "deepseek" => (AdapterKind::OpenAI, Some("https://api.deepseek.com/v1/"), Some("DEEPSEEK_API_KEY")),
        // genai's Mistral adapter, not plain OpenAI: Mistral rejects unknown
        // request fields and returns reasoning as `thinking` chunks.
        "mistral" => (AdapterKind::Mistral, Some("https://api.mistral.ai/v1/"), Some("MISTRAL_API_KEY")),
        "openrouter" => (AdapterKind::OpenAI, Some("https://openrouter.ai/api/v1/"), Some("OPENROUTER_API_KEY")),
        "xai" => (AdapterKind::Xai, None, Some("XAI_API_KEY")),
        "groq" => (AdapterKind::Groq, None, Some("GROQ_API_KEY")),
        // Local servers: chat completions, no key needed.
        "ollama" => (AdapterKind::OpenAI, Some("http://localhost:11434/v1/"), None),
        "openai_compatible" => (AdapterKind::OpenAI, None, None),
        other => return Err(anyhow!("unknown provider kind `{other}`")),
    })
}

fn build_provider(p: &LlmProvider) -> Result<Provider> {
    let (adapter, default_endpoint, default_key_env) = kind_defaults(&p.kind)?;

    let key_env = p.api_key_env.as_deref().or(default_key_env);
    let api_key = match key_env {
        Some(var) => std::env::var(var).with_context(|| format!("{var} is not set"))?,
        // vLLM without --api-key, llama-server and Ollama ignore the bearer.
        None => "no-key-needed".to_string(),
    };
    let endpoint = match (&p.base_url, default_endpoint) {
        (Some(url), _) => Some(if url.ends_with('/') { url.clone() } else { format!("{url}/") }),
        (None, Some(url)) => Some(url.to_string()),
        (None, None) if p.kind == "openai_compatible" => {
            return Err(anyhow!("`openai_compatible` needs a base_url"));
        }
        (None, None) => None,
    };

    // Route every model name to the wire format we chose, rather than one genai
    // infers from the name (`muse-spark-1.3`, `Qwen/Qwen3-32B` would otherwise
    // fall through to Ollama; some Gemini names to the Interactions API).
    let model_mapper = ModelMapper::from_mapper_fn(move |model_iden: ModelIden| {
        Ok(ModelIden::new(adapter, model_iden.model_name))
    });
    let mut builder = Client::builder().with_model_mapper(model_mapper);
    builder = match endpoint {
        Some(endpoint) => {
            let target_resolver = ServiceTargetResolver::from_resolver_fn(move |target: ServiceTarget| {
                Ok(ServiceTarget {
                    endpoint: Endpoint::from_owned(endpoint.clone()),
                    auth: AuthData::Key(api_key.clone()),
                    model: target.model,
                })
            });
            builder.with_service_target_resolver(target_resolver)
        }
        None => {
            let auth_resolver =
                AuthResolver::from_resolver_fn(move |_model: ModelIden| Ok(Some(AuthData::Key(api_key.clone()))));
            builder.with_auth_resolver(auth_resolver)
        }
    };

    let mut options = ChatOptions::default();
    if let Some(effort) = &p.reasoning_effort {
        options.reasoning_effort = Some(parse_effort(effort)?);
    }

    Ok(Provider {
        name: p.name.clone().unwrap_or_else(|| p.kind.clone()),
        model: p.model.clone(),
        adapter,
        client: builder.build()?,
        options,
    })
}

fn parse_effort(s: &str) -> Result<ReasoningEffort> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "zero" | "none" => ReasoningEffort::Zero,
        "minimal" => ReasoningEffort::Minimal,
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        "max" => ReasoningEffort::Max,
        other => return Err(anyhow!("unknown reasoning_effort `{other}`")),
    })
}

/// Adapt tool-call ids to `adapter`. Gemini takes them as they are: its adapter
/// recognises its own synthetic ids, passes foreign ids through, and stands in a
/// placeholder for a missing thought signature. Everyone else gets ids rewritten
/// to `call_<n>` (unique across the request, and within Anthropic's
/// `[A-Za-z0-9_-]`), each response mapped to its call's new id. (Which turns
/// keep reasoning and signatures is decided by provenance, in `agent.rs`.)
fn for_provider(mut req: ChatRequest, adapter: AdapterKind) -> ChatRequest {
    if adapter == AdapterKind::Gemini {
        return req;
    }
    // Original id → rewritten id. Gemini's synthetic ids repeat every turn, so
    // a later call overwrites the mapping; responses always follow their call.
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut next = 0usize;
    for msg in &mut req.messages {
        let parts = std::mem::take(&mut msg.content).into_parts();
        let parts: Vec<ContentPart> = parts
            .into_iter()
            .map(|part| match part {
                ContentPart::ToolCall(mut call) => {
                    let id = format!("call_{next}");
                    next += 1;
                    ids.insert(call.call_id.clone(), id.clone());
                    call.call_id = id;
                    ContentPart::ToolCall(call)
                }
                ContentPart::ToolResponse(mut resp) => {
                    resp.call_id = ids.get(&resp.call_id).cloned().unwrap_or_else(|| sanitize_id(&resp.call_id));
                    ContentPart::ToolResponse(resp)
                }
                other => other,
            })
            .collect();
        msg.content = MessageContent::from_parts(parts);
    }
    req
}

fn sanitize_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

// ─────────────────────────────── retries ────────────────────────────────────

impl RetryPolicy {
    /// base · 2^attempt, capped, with up to 25% jitter so concurrent sessions
    /// don't all hammer the provider again in lockstep.
    fn backoff(&self, attempt: u32) -> Duration {
        let exp = self.base_delay.saturating_mul(1u32 << attempt.min(16));
        let capped = exp.min(self.max_delay);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let jitter = capped.mul_f64((nanos % 250) as f64 / 1000.0);
        capped + jitter
    }
}

/// `Some(retry_after)` if `e` is worth retrying, where `retry_after` is the
/// provider's own `Retry-After` hint (if it gave a usable one). `None` means a
/// permanent failure.
fn transient(e: &genai::Error) -> Option<Option<Duration>> {
    let webc_error = match e {
        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => webc_error,
        genai::Error::HttpError { status, .. } => {
            return retryable_status(status.as_u16()).then_some(None);
        }
        _ => return None,
    };
    match webc_error {
        webc::Error::ResponseFailedStatus { status, headers, .. } => {
            if !retryable_status(status.as_u16()) {
                return None;
            }
            let retry_after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                // Don't let a provider park a session for minutes; the
                // extension gives up on the request long before that.
                .map(|secs| Duration::from_secs(secs.min(30)));
            Some(retry_after)
        }
        // Connection reset, DNS hiccup, timeout — the request never got a
        // real answer, so trying again is safe.
        webc::Error::Reqwest(re) => {
            (re.is_timeout() || re.is_connect() || re.is_request() || re.is_body()).then_some(None)
        }
        _ => None,
    }
}

/// 408 Request Timeout, 429 Too Many Requests, and the 5xx family that means
/// "the model/gateway is having a moment" rather than "your request is wrong".
fn retryable_status(code: u16) -> bool {
    matches!(code, 408 | 429 | 500 | 502 | 503 | 504)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{ChatMessage, ToolCall, ToolResponse};
    use serde_json::json;

    #[test]
    fn retries_gateway_and_overload_statuses_only() {
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(retryable_status(code), "{code} should retry");
        }
        for code in [400, 401, 403, 404, 422, 501] {
            assert!(!retryable_status(code), "{code} should not retry");
        }
    }

    #[test]
    fn backoff_doubles_and_caps_with_bounded_jitter() {
        let p = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_millis(8000),
        };
        for (attempt, base) in [(0, 1000), (1, 2000), (2, 4000), (3, 8000), (4, 8000), (40, 8000)] {
            let d = p.backoff(attempt).as_millis() as u64;
            assert!(d >= base && d <= base + base / 4, "attempt {attempt}: {d}ms");
        }
    }

    #[test]
    fn permanent_genai_errors_are_not_retried() {
        let e = genai::Error::ChatResponse {
            model_iden: ModelIden::new(AdapterKind::Gemini, "m"),
            body: serde_json::Value::Null,
        };
        assert!(transient(&e).is_none());
    }

    fn gemini_call(n: usize) -> ToolCall {
        ToolCall {
            call_id: format!("call#execute_script#{n}"),
            fn_name: "execute_script".into(),
            fn_arguments: json!({ "code": "return 1" }),
            thought_signatures: Some(vec!["sig".into()]),
        }
    }

    /// Two Gemini turns that both used `call#execute_script#0`: for Anthropic
    /// et al. each call gets its own clean id and each response follows it.
    #[test]
    fn rewrites_gemini_ids_for_other_providers() {
        let req = ChatRequest::from_messages(vec![
            ChatMessage::from(vec![gemini_call(0)]),
            ChatMessage::from(ToolResponse::new("call#execute_script#0", "one")),
            ChatMessage::from(vec![gemini_call(0)]),
            ChatMessage::from(ToolResponse::new("call#execute_script#0", "two")),
        ]);

        let out = for_provider(req.clone(), AdapterKind::Anthropic);
        let calls: Vec<_> = out.messages.iter().flat_map(|m| m.content.tool_calls()).collect();
        let resps: Vec<_> = out.messages.iter().flat_map(|m| m.content.tool_responses()).collect();
        assert_eq!(calls.iter().map(|c| c.call_id.as_str()).collect::<Vec<_>>(), ["call_0", "call_1"]);
        assert_eq!(resps.iter().map(|r| r.call_id.as_str()).collect::<Vec<_>>(), ["call_0", "call_1"]);

        // Gemini gets the ids untouched (its adapter handles its own ids).
        let same = for_provider(req, AdapterKind::Gemini);
        assert_eq!(same.messages[0].content.tool_calls()[0].call_id, "call#execute_script#0");
    }

    #[test]
    fn every_kind_has_defaults_and_unknown_kinds_fail() {
        for kind in [
            "gemini", "anthropic", "openai", "meta", "deepseek", "mistral", "openrouter", "xai", "groq", "ollama",
            "openai_compatible",
        ] {
            assert!(kind_defaults(kind).is_ok(), "{kind}");
        }
        assert!(kind_defaults("nope").is_err());
        let p = LlmProvider {
            kind: "openai_compatible".into(),
            model: "m".into(),
            name: None,
            api_key_env: None,
            base_url: None,
            reasoning_effort: None,
        };
        assert!(build_provider(&p).is_err(), "openai_compatible needs a base_url");
        assert!(parse_effort("Low").is_ok() && parse_effort("lots").is_err());
    }
}
