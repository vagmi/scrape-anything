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

/// One-shot: read signals + intention from the session, ask Gemini, parse out
/// the ```js-action fence. Persistence is the caller's job.
pub async fn step(state: &AppState, session_id: &str) -> AppResult<Envelope> {
    let db = state.db();
    let session = load_session(db, session_id).await?;
    let signals = load_signals_payload(db, session_id).await?;

    let system = SYSTEM_PROMPT.to_string();
    let user = format!(
        "Page URL: {url}\n\nUser intention:\n{intent}\n\nPage signals (JSON):\n{signals}",
        url = session.url,
        intent = session.intention,
        signals = truncate(&signals, 80_000),
    );

    let messages = vec![ChatMessage::system(system), ChatMessage::user(user)];
    let req = ChatRequest::from_messages(messages);
    let opts = ChatOptions::default();

    tracing::debug!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        model = %state.inner.llm.model,
        signals_chars = signals.len(),
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
        Some((_, _, s)) if s != "active" => Err(AppError::SessionExpired),
        Some((url, intention, _)) => Ok(SessionRow { url, intention }),
    }
}

/// Pull the signals turn (turn 0, role=extension, kind=signals) as a JSON string.
async fn load_signals_payload(db: &SqlitePool, session_id: &str) -> AppResult<String> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT payload FROM turns
         WHERE session_id = ?1 AND role = 'extension' AND kind = 'signals'
         ORDER BY turn_index ASC LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(p,)| p).unwrap_or_else(|| "{}".into()))
}

// ──────────────────────────── system prompt ────────────────────────────────

const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
