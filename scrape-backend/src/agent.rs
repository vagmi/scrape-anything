//! Function-calling agent.
//!
//! One LLM step per `step()` call: replay the session's turn history into a
//! Gemini chat (with the tool catalog attached), call `exec_chat`, and return
//! the model's text and/or tool calls. The *loop* — resolving server-side tools
//! inline and handing extension-side tools back over HTTP — lives in
//! `session.rs`.
//!
//! Tools (see `crate::tools`):
//!   - server-side: list/read/write/edit against the per-origin file store.
//!   - extension-side: `execute_script`, `screenshot` (run in the live tab).
//!
//! The agent's persistent memory is the per-origin file store; the first user
//! message lists what's already there so repeat runs on a site reuse prior work.

use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ContentPart, MessageContent, ToolCall, ToolResponse,
};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::tools;

/// One assistant response: free text and/or a batch of tool calls.
#[derive(Debug, Clone)]
pub struct Step {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// Drive one LLM step for `session_id`.
pub async fn step(state: &AppState, session_id: &str) -> AppResult<Step> {
    let db = state.db();
    let session = load_session(db, session_id).await?;
    let messages = build_messages(db, session_id, &session).await?;

    let req = ChatRequest::from_messages(messages).with_tools(tools::catalog());
    let opts = ChatOptions::default();

    tracing::debug!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        model = %state.inner.llm.model,
        "calling gemini (function-calling)"
    );

    let llm = &state.inner.llm;
    let resp = llm
        .client
        .exec_chat(llm.model.as_str(), req, Some(&opts))
        .await
        .map_err(|e| AppError::LlmUnavailable(e.to_string()))?;

    let text = resp.first_text().map(|s| s.to_string());
    let tool_calls = resp.into_tool_calls();

    tracing::info!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        tool_calls = tool_calls.len(),
        tools = %tool_calls.iter().map(|c| c.fn_name.as_str()).collect::<Vec<_>>().join(","),
        has_text = text.is_some(),
        "assistant step"
    );

    if text.is_none() && tool_calls.is_empty() {
        return Err(AppError::LlmUnavailable("empty response".into()));
    }
    Ok(Step { text, tool_calls })
}

// ───────────────────────── session/signals loaders ──────────────────────────

pub struct SessionRow {
    pub url: String,
    pub host: String,
    pub intention: String,
}

async fn load_session(db: &SqlitePool, id: &str) -> AppResult<SessionRow> {
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT url, host, intention FROM sessions WHERE id = ?1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    match row {
        None => Err(AppError::SessionNotFound),
        Some((url, host, intention)) => Ok(SessionRow { url, host, intention }),
    }
}

/// Convert the persisted turn log into a Gemini chat history.
///
/// First user message bundles URL + intention + signals + the origin's memory
/// file list. Then turns replay in order:
///   - user/message        → user text
///   - llm/assistant        → assistant text and/or ToolCalls
///   - tool/tool_response   → ToolResponse (+ an image user message for screenshots)
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

    let file_list = origin_file_list(db, &session.host).await?;

    out.push(ChatMessage::user(format!(
        "Page URL: {url}\n\nIntention:\n{intent}\n\n\
         Files already in your memory for `{host}`:\n{files}\n\n\
         Page signals (JSON):\n{signals}",
        url = session.url,
        intent = session.intention,
        host = session.host,
        files = file_list,
        signals = truncate(&signals, 80_000),
    )));

    for (_, role, kind, payload) in &rows {
        match (role.as_str(), kind.as_str()) {
            ("user", "message") => {
                let v: Value = serde_json::from_str(payload).unwrap_or_default();
                let content = v.get("content").and_then(|c| c.as_str()).unwrap_or(payload.as_str());
                out.push(ChatMessage::user(content.to_string()));
            }
            ("llm", "assistant") => {
                let v: Value = serde_json::from_str(payload).unwrap_or_default();
                let calls = parse_stored_tool_calls(&v);
                if !calls.is_empty() {
                    // Assistant tool-call turn. (Any incidental text alongside
                    // tool calls is dropped on replay — not needed for context.)
                    out.push(ChatMessage::from(calls));
                } else if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                    out.push(ChatMessage::assistant(text.to_string()));
                }
            }
            ("tool", "tool_response") => {
                let v: Value = serde_json::from_str(payload).unwrap_or_default();
                let call_id = v.get("call_id").and_then(|c| c.as_str()).unwrap_or("").to_string();
                let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                out.push(ChatMessage::from(ToolResponse::new(call_id, content)));

                // Screenshots ride in as a following user image message, since a
                // functionResponse carries only text.
                if let Some(img) = v.get("image_base64").and_then(|c| c.as_str()) {
                    let mime = v.get("mime").and_then(|c| c.as_str()).unwrap_or("image/png");
                    out.push(ChatMessage::user(MessageContent::from_parts(vec![
                        ContentPart::from_text("Screenshot of the current page:"),
                        ContentPart::from_binary_base64(mime.to_string(), img.to_string(), None),
                    ])));
                }
            }
            // signals (already in first message) and anything else are skipped.
            _ => {}
        }
    }

    Ok(out)
}

/// Reconstruct `Vec<ToolCall>` from a stored assistant payload's `tool_calls`.
fn parse_stored_tool_calls(v: &Value) -> Vec<ToolCall> {
    v.get("tool_calls")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let call_id = c.get("call_id")?.as_str()?.to_string();
                    let fn_name = c.get("fn_name")?.as_str()?.to_string();
                    let fn_arguments = c.get("fn_arguments").cloned().unwrap_or(Value::Null);
                    Some(ToolCall { call_id, fn_name, fn_arguments, thought_signatures: None })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Compact listing of the origin's memory files for the opening prompt.
async fn origin_file_list(db: &SqlitePool, host: &str) -> AppResult<String> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT path, length(content) FROM origin_files WHERE origin = ?1 ORDER BY path ASC",
    )
    .bind(host)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok("(none yet — this is the first run on this site)".to_string());
    }
    Ok(rows
        .into_iter()
        .map(|(path, size)| format!("- {path} ({size} bytes)"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…[+{} chars]", &s[..max], s.len() - max) }
}

// ──────────────────────────── system prompt ────────────────────────────────

const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");
