//! Code-generation agent.
//!
//! Asks Gemini to write a JavaScript function body that extracts the user's
//! intention from the live page, wrapped in a ```js-action fenced block. The
//! extension runs the code via `new Function(code)()` and returns the result.
//!
//! Why not structured output / tool-calling: the model is much better at
//! emitting code than at filling discriminated-union schemas, and code is
//! more expressive than the Plan DSL ever was. Trade-off: cached "plans"
//! become code blobs you read top-to-bottom rather than per-field selectors.
//! For local YOLO use this is fine; revisit if/when we ship this multi-user.

use futures::StreamExt;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Debug, Clone)]
pub enum Envelope {
    /// Model produced a usable js-action block.
    Code { code: String, rationale: String },
    /// No code fence found in the response — surface the response as a reason.
    NoCode { reason: String },
}

/// Drive one LLM step for `session_id`. Builds the chat history from every
/// persisted turn — initial signals + intention, prior code emissions, prior
/// extraction results, follow-up user messages — so refinements are coherent
/// across many back-and-forths in the same session.
pub async fn step(state: &AppState, session_id: &str) -> AppResult<Envelope> {
    let db = state.db();
    let session = load_session(db, session_id).await?;
    let messages = build_messages(db, session_id, &session).await?;
    let req = ChatRequest::from_messages(messages);
    let opts = ChatOptions::default();

    tracing::debug!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        model = %state.inner.llm.model,
        "calling gemini for code"
    );

    let raw = call_llm(state, req, &opts).await?;
    tracing::debug!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        chars = raw.len(),
        raw = %truncate(&raw, 6_000),
        "gemini raw response"
    );

    let env = parse(&raw);
    tracing::info!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        envelope = %describe(&env),
        "parsed envelope"
    );
    Ok(env)
}

async fn call_llm(state: &AppState, req: ChatRequest, opts: &ChatOptions) -> AppResult<String> {
    let llm = &state.inner.llm;
    let stream_res = llm
        .client
        .exec_chat_stream(&llm.model, req, Some(opts))
        .await
        .map_err(|e| AppError::LlmUnavailable(e.to_string()))?;

    let mut stream = stream_res.stream;
    let mut buf = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatStreamEvent::Chunk(c)) => buf.push_str(&c.content),
            Ok(ChatStreamEvent::End(_)) => break,
            Err(e) => return Err(AppError::LlmUnavailable(e.to_string())),
            _ => {}
        }
    }
    if buf.trim().is_empty() {
        return Err(AppError::LlmUnavailable("empty response".into()));
    }
    Ok(buf)
}

/// Find the first fenced block whose info-string is `js-action` (or, as a
/// fallback, plain `js`/`javascript`). Everything outside the fence is treated
/// as the rationale.
fn parse(raw: &str) -> Envelope {
    let fences = ["js-action", "javascript-action", "js", "javascript"];
    for tag in fences {
        if let Some((code, rationale)) = extract_fence(raw, tag) {
            return Envelope::Code { code, rationale };
        }
    }
    Envelope::NoCode { reason: raw.trim().to_string() }
}

/// Extracts the first ```<tag> ... ``` block. Tag-match is case-insensitive
/// and ignores trailing chars on the info string (so ```js-action attrs``` is OK).
fn extract_fence(raw: &str, tag: &str) -> Option<(String, String)> {
    let needle_low = format!("```{}", tag.to_lowercase());
    let raw_low = raw.to_lowercase();
    let start = raw_low.find(&needle_low)?;
    // Skip past the opening ``` line.
    let after_open = &raw[start..];
    let first_nl = after_open.find('\n')?;
    let body_start = start + first_nl + 1;
    let body_slice = &raw[body_start..];
    let end_rel = body_slice.find("```")?;
    let code = body_slice[..end_rel].trim_end().to_string();
    let rationale = {
        let before = &raw[..start];
        let after = &raw[body_start + end_rel + 3..];
        format!("{}{}", before.trim(), after.trim()).trim().to_string()
    };
    Some((code, rationale))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…[+{} chars]", &s[..max], s.len() - max) }
}

fn describe(e: &Envelope) -> String {
    match e {
        Envelope::Code { code, rationale } => format!(
            "code (rationale={:?}) code={}",
            truncate(rationale, 200),
            truncate(code, 2_000)
        ),
        Envelope::NoCode { reason } => format!("no_code reason={}", truncate(reason, 600)),
    }
}

// ───────────────────────── session/signals loaders ──────────────────────────

struct SessionRow { url: String, intention: String }

async fn load_session(db: &SqlitePool, id: &str) -> AppResult<SessionRow> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT url, intention, status FROM sessions WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?;
    match row {
        None => Err(AppError::SessionNotFound),
        Some((url, intention, _)) => Ok(SessionRow { url, intention }),
    }
}

/// Convert the persisted turn log into a Gemini chat history.
///
/// First user message bundles URL + initial intention + signals (the static
/// page context). After that we replay turns in order:
///   - llm/code        → assistant("rationale\n```js-action\n<code>\n```")
///   - llm/no_code     → assistant("<reason>")
///   - extension/result → user("<extraction summary + sample items>")
///   - user/message    → user("<refinement>")
///
/// Signals and intention are *not* repeated on follow-up turns; the model
/// already saw them at the top of the conversation.
async fn build_messages(
    db: &SqlitePool,
    session_id: &str,
    session: &SessionRow,
) -> AppResult<Vec<ChatMessage>> {
    let mut out: Vec<ChatMessage> = Vec::new();
    out.push(ChatMessage::system(SYSTEM_PROMPT.to_string()));

    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT turn_index, role, kind, payload FROM turns
         WHERE session_id = ?1 ORDER BY turn_index ASC",
    )
    .bind(session_id)
    .fetch_all(db)
    .await?;

    let signals = rows
        .iter()
        .find(|(_, r, k, _)| r == "extension" && k == "signals")
        .map(|(_, _, _, p)| p.clone())
        .unwrap_or_else(|| "{}".into());

    out.push(ChatMessage::user(format!(
        "Page URL: {url}\n\nInitial intention:\n{intent}\n\nPage signals (JSON):\n{signals}",
        url = session.url,
        intent = session.intention,
        signals = truncate(&signals, 80_000),
    )));

    for (_, role, kind, payload) in &rows {
        match (role.as_str(), kind.as_str()) {
            ("llm", "code") => {
                // Persisted Turn::Code; reconstruct the assistant message
                // that originally produced it (rationale + js-action fence).
                let v: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
                let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("");
                let rationale = v.get("rationale").and_then(|c| c.as_str()).unwrap_or("");
                let text = format!("{rationale}\n\n```js-action\n{code}\n```");
                out.push(ChatMessage::assistant(text));
            }
            ("llm", "no_code") => {
                let v: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
                let reason = v.get("reason").and_then(|c| c.as_str()).unwrap_or("");
                out.push(ChatMessage::assistant(format!("(no code) {reason}")));
            }
            ("extension", "result") => {
                let summary = summarize_result(payload);
                out.push(ChatMessage::user(format!(
                    "<extraction_result>\n{summary}\n</extraction_result>"
                )));
            }
            ("user", "message") => {
                let v: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
                let content = v.get("content").and_then(|c| c.as_str()).unwrap_or(payload.as_str());
                out.push(ChatMessage::user(content.to_string()));
            }
            // signals + everything else (older "tool_call", "validation_error",
            // "clarify_answer", "result" telemetry shapes) are skipped — the
            // signals are already in the first user message and the other
            // kinds aren't part of the active codegen flow anymore.
            _ => {}
        }
    }

    Ok(out)
}

/// Produce a compact text summary of an `/api/sessions/:id/result` payload.
/// The model needs to know how many items came back and what they looked
/// like so it can refine its next code.
fn summarize_result(payload_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(payload_json) {
        Ok(v) => v,
        Err(_) => return truncate(payload_json, 1_500),
    };
    let items = v.get("items_count").and_then(|x| x.as_u64()).unwrap_or(0);
    let errors = v.get("errors")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(String::from))
                .collect::<Vec<_>>()
                .join("; ")
        })
        .unwrap_or_default();
    let sample = v.get("sample_items").cloned().unwrap_or(serde_json::Value::Null);
    let sample_str = if sample.is_null() {
        String::new()
    } else {
        format!("\nSample items: {}", truncate(&sample.to_string(), 2_000))
    };
    let err_line = if errors.is_empty() {
        String::new()
    } else {
        format!("\nErrors: {}", truncate(&errors, 1_000))
    };
    format!("Code returned {items} item(s).{err_line}{sample_str}")
}

// ──────────────────────────── system prompt ────────────────────────────────

const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
