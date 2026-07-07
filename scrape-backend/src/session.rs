//! Session lifecycle for the function-calling agent.
//!
//! A session is a conversation: an intention + a starting page, then a loop of
//! LLM steps. Each step the model may call tools. Server-side tools
//! (list/read/write/edit over the per-origin file store) are resolved inline in
//! `advance()`; extension-side tools (`execute_script`, `screenshot`) are handed
//! back to the extension, which runs them in the live tab and POSTs the results
//! to `/tool_result`, resuming the loop. The session ends when the model
//! replies with plain text (no tool calls).
//!
//! Extracted rows (an `execute_script` result shaped `{ kind, items }`) are
//! upserted into the `artifacts` table and serve the JSON/CSV downloads.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use ulid::Ulid;

use crate::agent;
use crate::error::{AppError, AppResult};
use crate::signals::Signals;
use crate::state::AppState;
use crate::tools;

/// A pending extension-side tool call handed to the extension to execute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCall {
    pub call_id: String,
    pub tool: String,
    pub args: Value,
}

/// What the extension sees back from create/message/tool_result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Turn {
    /// The extension must run these tools and reply via `/tool_result`.
    ToolCall { tool_calls: Vec<PendingCall>, turn_index: u32 },
    /// Terminal: the agent's closing message. No more tools to run.
    Message { text: String, turn_index: u32 },
}

/// One tool result posted back by the extension.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolResultItem {
    pub call_id: String,
    /// The tool that was run (so we can label the stored response).
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<Value>,
}

pub async fn create(
    state: &AppState,
    url: String,
    intention: String,
    signals: Signals,
) -> AppResult<(String, Turn)> {
    if url.is_empty() {
        return Err(AppError::BadSignals("url is required".into()));
    }
    let (host, route) = url_host_and_path(&url)
        .map_err(|e| AppError::BadSignals(format!("invalid url: {e}")))?;

    let session_id = Ulid::new().to_string();
    let now = Utc::now().timestamp_millis();
    let intention_hash = hash_hex(&intention);

    let signals_json = serde_json::to_string(&signals).map_err(anyhow::Error::from)?;

    sqlx::query(
        "INSERT INTO sessions (id, url, host, route, intention, intention_hash, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?7)",
    )
    .bind(&session_id)
    .bind(&url)
    .bind(&host)
    .bind(&route)
    .bind(&intention)
    .bind(&intention_hash)
    .bind(now)
    .execute(state.db())
    .await?;

    insert_turn(state.db(), &session_id, 0, "extension", "signals", &signals_json).await?;

    let turn = advance(state, &session_id).await?;
    Ok((session_id, turn))
}

/// Append a follow-up user message and resume the loop. Revives completed
/// sessions (continuing the conversation refines the result).
pub async fn handle_message(
    state: &AppState,
    session_id: &str,
    content: String,
) -> AppResult<Turn> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(AppError::BadSignals("message content is empty".into()));
    }
    session_must_exist(state.db(), session_id).await?;
    mark_status(state.db(), session_id, "active").await?;

    let idx = next_index(state.db(), session_id).await?;
    let payload = json!({ "content": trimmed }).to_string();
    tracing::info!(
        target: "scrape_backend::session",
        session_id = %session_id,
        turn_index = idx,
        content = %truncate_for_log(trimmed, 400),
        "user message"
    );
    insert_turn(state.db(), session_id, idx, "user", "message", &payload).await?;
    advance(state, session_id).await
}

/// Results from the extension for the tools it just ran. Persists a
/// tool_response per call, upserts any `{kind,items}` artifact, then resumes
/// the loop and returns the next turn.
pub async fn handle_tool_result(
    state: &AppState,
    session_id: &str,
    results: Vec<ToolResultItem>,
) -> AppResult<Turn> {
    session_must_exist(state.db(), session_id).await?;

    for item in &results {
        let fn_name = item.tool.clone().unwrap_or_default();
        let (content, image, mime) = if let Some(err) = &item.error {
            (json!({ "error": err }).to_string(), None, None)
        } else if let Some(img) = item.result.get("image_base64").and_then(|v| v.as_str()) {
            let mime = item
                .result
                .get("mime")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            (
                "Screenshot captured; the image follows.".to_string(),
                Some(img.to_string()),
                Some(mime),
            )
        } else {
            (item.result.to_string(), None, None)
        };

        tracing::info!(
            target: "scrape_backend::session",
            session_id = %session_id,
            tool = %fn_name,
            call_id = %item.call_id,
            is_error = item.error.is_some(),
            "tool result"
        );

        persist_tool_response(
            state.db(),
            session_id,
            &fn_name,
            &item.call_id,
            &content,
            image.as_deref(),
            mime.as_deref(),
        )
        .await?;

        maybe_store_artifact(state.db(), session_id, &item.result).await?;
    }

    advance(state, session_id).await
}

/// Deprecated. The old clarify protocol is gone; keep the endpoint returning a
/// clear 410 rather than a 404 for old extension builds.
pub async fn handle_clarify_answer(
    _state: &AppState,
    _session_id: &str,
    _answer: String,
) -> AppResult<Turn> {
    Err(AppError::SessionExpired)
}

/// Optional post-run telemetry from the extension (kept for compatibility).
pub async fn record_result(state: &AppState, session_id: &str, payload: Value) -> AppResult<()> {
    session_must_exist(state.db(), session_id).await?;
    let idx = next_index(state.db(), session_id).await?;
    insert_turn(state.db(), session_id, idx, "extension", "result", &payload.to_string()).await?;
    Ok(())
}

async fn session_must_exist(db: &SqlitePool, session_id: &str) -> AppResult<()> {
    let row: Option<(String,)> = sqlx::query_as("SELECT id FROM sessions WHERE id = ?1")
        .bind(session_id)
        .fetch_optional(db)
        .await?;
    if row.is_none() { Err(AppError::SessionNotFound) } else { Ok(()) }
}

pub async fn fetch_full(state: &AppState, session_id: &str) -> AppResult<Value> {
    let row: Option<(String, String, String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT id, url, host, route, intention, status, created_at, updated_at FROM sessions WHERE id = ?1",
    )
    .bind(session_id)
    .fetch_optional(state.db())
    .await?;
    let Some((id, url, host, route, intention, status, created_at, updated_at)) = row else {
        return Err(AppError::SessionNotFound);
    };

    let turns: Vec<(i64, String, String, String, i64)> = sqlx::query_as(
        "SELECT turn_index, role, kind, payload, created_at FROM turns
         WHERE session_id = ?1 ORDER BY turn_index ASC",
    )
    .bind(session_id)
    .fetch_all(state.db())
    .await?;

    let turns: Vec<Value> = turns
        .into_iter()
        .map(|(idx, role, kind, payload, ts)| {
            json!({
                "turn_index": idx,
                "role": role,
                "kind": kind,
                "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                "created_at": ts,
            })
        })
        .collect();

    Ok(json!({
        "session_id": id,
        "url": url,
        "host": host,
        "route": route,
        "intention": intention,
        "status": status,
        "created_at": created_at,
        "updated_at": updated_at,
        "turns": turns,
    }))
}

// ───────────────────── core driver: the agent loop ──────────────────────────

async fn advance(state: &AppState, session_id: &str) -> AppResult<Turn> {
    let db = state.db();
    let host = session_host(db, session_id).await?;
    let max_turns = state.config().llm.max_turns;

    loop {
        // Hard cap on the number of LLM steps in one session.
        let steps = count_assistant_turns(db, session_id).await?;
        if steps >= max_turns {
            let idx = persist_assistant_text(
                db,
                session_id,
                "Stopped: reached the maximum number of steps for this session.",
            )
            .await?;
            mark_status(db, session_id, "completed").await?;
            return Ok(Turn::Message {
                text: "Stopped: reached the maximum number of steps for this session.".into(),
                turn_index: idx,
            });
        }

        let step = agent::step(state, session_id).await?;
        let idx = persist_assistant(db, session_id, &step).await?;

        if step.tool_calls.is_empty() {
            mark_status(db, session_id, "completed").await?;
            tracing::info!(target: "scrape_backend::session", %session_id, "session completed (final message)");
            return Ok(Turn::Message {
                text: step.text.unwrap_or_default(),
                turn_index: idx,
            });
        }

        // Resolve server-side tools inline; collect extension-side tools.
        let mut pending: Vec<PendingCall> = Vec::new();
        for call in &step.tool_calls {
            if tools::is_server_side(&call.fn_name) {
                let resp = tools::run_server_tool(db, &host, call).await;
                persist_tool_response(db, session_id, &call.fn_name, &resp.call_id, &resp.content, None, None)
                    .await?;
            } else {
                pending.push(PendingCall {
                    call_id: call.call_id.clone(),
                    tool: call.fn_name.clone(),
                    args: call.fn_arguments.clone(),
                });
            }
        }

        if pending.is_empty() {
            // All calls were server-side — ask the model again immediately.
            continue;
        }

        mark_status(db, session_id, "active").await?;
        return Ok(Turn::ToolCall { tool_calls: pending, turn_index: idx });
    }
}

// ───────────────────────────── artifacts ────────────────────────────────────

/// If a tool result looks like an extraction (`{ kind, items:[…] }` with a
/// non-empty array), upsert it as the session's artifact (latest wins).
async fn maybe_store_artifact(db: &SqlitePool, session_id: &str, result: &Value) -> AppResult<()> {
    let Some(items) = result.get("items").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    if items.is_empty() {
        return Ok(());
    }
    let kind = result.get("kind").and_then(|v| v.as_str()).unwrap_or("listing").to_string();
    let items_json = serde_json::to_string(items).unwrap_or_else(|_| "[]".into());
    let now = Utc::now().timestamp_millis();
    let id = Ulid::new().to_string();

    sqlx::query(
        "INSERT INTO artifacts (id, session_id, kind, item_count, items_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
             kind       = excluded.kind,
             item_count = excluded.item_count,
             items_json = excluded.items_json,
             updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(session_id)
    .bind(&kind)
    .bind(items.len() as i64)
    .bind(&items_json)
    .bind(now)
    .execute(db)
    .await?;
    tracing::info!(target: "scrape_backend::session", %session_id, items = items.len(), kind = %kind, "artifact stored");
    Ok(())
}

// ─────────────────────────── small DB helpers ───────────────────────────────

async fn session_host(db: &SqlitePool, session_id: &str) -> AppResult<String> {
    let row: Option<(String,)> = sqlx::query_as("SELECT host FROM sessions WHERE id = ?1")
        .bind(session_id)
        .fetch_optional(db)
        .await?;
    row.map(|r| r.0).ok_or(AppError::SessionNotFound)
}

async fn count_assistant_turns(db: &SqlitePool, session_id: &str) -> AppResult<u32> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM turns WHERE session_id = ?1 AND role = 'llm' AND kind = 'assistant'",
    )
    .bind(session_id)
    .fetch_one(db)
    .await?;
    Ok(row.0 as u32)
}

async fn persist_assistant(db: &SqlitePool, session_id: &str, step: &agent::Step) -> AppResult<u32> {
    let idx = next_index(db, session_id).await?;
    let stored_calls: Vec<Value> = step
        .tool_calls
        .iter()
        .map(|c| json!({ "call_id": c.call_id, "fn_name": c.fn_name, "fn_arguments": c.fn_arguments }))
        .collect();
    let payload = json!({ "text": step.text, "tool_calls": stored_calls }).to_string();
    insert_turn(db, session_id, idx, "llm", "assistant", &payload).await?;
    Ok(idx)
}

async fn persist_assistant_text(db: &SqlitePool, session_id: &str, text: &str) -> AppResult<u32> {
    let idx = next_index(db, session_id).await?;
    let payload = json!({ "text": text, "tool_calls": [] }).to_string();
    insert_turn(db, session_id, idx, "llm", "assistant", &payload).await?;
    Ok(idx)
}

async fn persist_tool_response(
    db: &SqlitePool,
    session_id: &str,
    fn_name: &str,
    call_id: &str,
    content: &str,
    image_base64: Option<&str>,
    mime: Option<&str>,
) -> AppResult<()> {
    let idx = next_index(db, session_id).await?;
    let mut obj = json!({ "call_id": call_id, "fn_name": fn_name, "content": content });
    if let Some(img) = image_base64 {
        obj["image_base64"] = json!(img);
        obj["mime"] = json!(mime.unwrap_or("image/png"));
    }
    insert_turn(db, session_id, idx, "tool", "tool_response", &obj.to_string()).await?;
    Ok(())
}

async fn next_index(db: &SqlitePool, session_id: &str) -> AppResult<u32> {
    let row: (Option<i64>,) =
        sqlx::query_as("SELECT MAX(turn_index) FROM turns WHERE session_id = ?1")
            .bind(session_id)
            .fetch_one(db)
            .await?;
    Ok(row.0.map(|n| n as u32 + 1).unwrap_or(0))
}

async fn insert_turn(
    db: &SqlitePool,
    session_id: &str,
    idx: u32,
    role: &str,
    kind: &str,
    payload: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO turns (id, session_id, turn_index, role, kind, payload, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(Ulid::new().to_string())
    .bind(session_id)
    .bind(idx as i64)
    .bind(role)
    .bind(kind)
    .bind(payload)
    .bind(Utc::now().timestamp_millis())
    .execute(db)
    .await?;
    Ok(())
}

async fn mark_status(db: &SqlitePool, session_id: &str, status: &str) -> AppResult<()> {
    sqlx::query("UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(status)
        .bind(Utc::now().timestamp_millis())
        .bind(session_id)
        .execute(db)
        .await?;
    Ok(())
}

fn hash_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

fn url_host_and_path(input: &str) -> Result<(String, String), &'static str> {
    let rest = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .ok_or("scheme must be http(s)")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("missing host");
    }
    let path = path.split(['?', '#']).next().unwrap_or("/");
    Ok((authority.to_string(), path.to_string()))
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…[+{} chars]", &s[..max], s.len() - max) }
}
