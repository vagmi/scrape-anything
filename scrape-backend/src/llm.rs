//! Thin wrapper over `genai` for the Gemini provider. The actual agentic loop
//! (turn-by-turn calls with tool schemas) is built in `session.rs`; this module
//! exposes a configured `genai::Client`, the chosen model name, and
//! `exec_chat`, which retries the transient failures a hosted model throws
//! now and then (a 502 from the gateway, a 503 while it's overloaded).

use anyhow::{Context, Result, anyhow};
use genai::Client;
use genai::chat::{ChatOptions, ChatRequest, ChatResponse};
use genai::resolver::{AuthData, AuthResolver, Error as ResolverError};
use genai::{ModelIden, webc};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::config::Llm;

#[derive(Clone)]
pub struct LlmClient {
    pub client: Client,
    pub model: String,
    retry: RetryPolicy,
}

#[derive(Clone, Copy, Debug)]
struct RetryPolicy {
    max_retries: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl LlmClient {
    pub fn from_config(cfg: &Llm) -> Result<Self> {
        if cfg.provider != "gemini" {
            return Err(anyhow!(
                "only `gemini` provider is wired up (got `{}`)",
                cfg.provider
            ));
        }
        let api_key = std::env::var("GEMINI_API_KEY")
            .context("GEMINI_API_KEY env var must be set")?;

        let client = build_gemini_client(api_key)?;
        let retry = RetryPolicy {
            max_retries: cfg.max_retries,
            base_delay: Duration::from_millis(cfg.retry_base_delay_ms),
            max_delay: Duration::from_millis(cfg.retry_max_delay_ms),
        };
        Ok(Self { client, model: cfg.model.clone(), retry })
    }

    /// `Client::exec_chat` against the configured model, retrying transient
    /// failures with exponential backoff (honouring `Retry-After` when the
    /// provider sends one). Anything else — a 400, bad auth, a response we
    /// couldn't parse — fails straight away, since retrying won't change it.
    pub async fn exec_chat(
        &self,
        req: ChatRequest,
        opts: Option<&ChatOptions>,
    ) -> genai::Result<ChatResponse> {
        let mut attempt = 0;
        loop {
            match self.client.exec_chat(self.model.as_str(), req.clone(), opts).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    let Some(retry_after) = transient(&e) else { return Err(e) };
                    if attempt >= self.retry.max_retries {
                        return Err(e);
                    }
                    let delay = retry_after.unwrap_or_else(|| self.retry.backoff(attempt));
                    attempt += 1;
                    tracing::warn!(
                        target: "scrape_backend::llm",
                        attempt,
                        max_retries = self.retry.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        error = %first_line(&e.to_string()),
                        "transient llm failure; retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

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

fn build_gemini_client(api_key: String) -> Result<Client> {
    let resolve = move |_model: ModelIden| -> Pin<
        Box<dyn Future<Output = Result<Option<AuthData>, ResolverError>> + Send + 'static>,
    > {
        let key = api_key.clone();
        Box::pin(async move { Ok(Some(AuthData::Key(key))) })
    };
    let resolver = AuthResolver::from_resolver_async_fn(resolve);
    Ok(Client::builder().with_auth_resolver(resolver).build())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            model_iden: ModelIden::new(genai::adapter::AdapterKind::Gemini, "m"),
            body: serde_json::Value::Null,
        };
        assert!(transient(&e).is_none());
    }
}
