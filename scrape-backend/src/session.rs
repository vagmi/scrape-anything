//! Session state machine. Skeleton only — the agentic loop (LLM call,
//! tool-call emission, plan validation) lives behind `next_turn()` and
//! currently short-circuits to a `give_up` turn with a "not implemented"
//! reason. Phase 1 will fill this in.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use ulid::Ulid;

use crate::error::{AppError, AppResult};
use crate::signals::Signals;
use crate::state::AppState;

/// What the extension sees back on each step. Mirrors SPEC §5.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Turn {
    ToolCall {
        tool_call_id: String,
        tool: String,
        args: Value,
        rationale: String,
        turn_index: u32,
        tokens_used: u32,
    },
    Clarify {
        question: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        options: Option<Vec<String>>,
        turn_index: u32,
    },
    Plan {
        plan: Value,
        turn_index: u32,
        tokens_used: u32,
    },
    GiveUp {
        reason: String,
        turn_index: u32,
    },
}

/// Create a session: insert row, store signals as turn 0, run the first LLM
/// step (stubbed for now), persist + return the resulting turn.
pub async fn create(
    state: &AppState,
    url: String,
    intention: String,
    signals: Signals,
) -> AppResult<(String, Turn)> {
    if url.is_empty() {
        return Err(AppError::BadSignals("url is required".into()));
    }
    let parsed = url::host_and_path(&url)
        .map_err(|e| AppError::BadSignals(format!("invalid url: {e}")))?;
    let host = parsed.0;
    let route = parsed.1;

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

    // TODO(P1): call self::next_turn() to actually drive the LLM.
    // Skeleton: emit an immediate give_up so the endpoint round-trips.
    let turn = Turn::GiveUp {
        reason: "agentic loop not yet implemented; skeleton only".into(),
        turn_index: 1,
    };
    persist_turn(state.db(), &session_id, &turn).await?;

    Ok((session_id, turn))
}

pub async fn handle_tool_result(
    state: &AppState,
    session_id: &str,
    _tool_call_id: &str,
    result: Value,
) -> AppResult<Turn> {
    let _ = ensure_active(state.db(), session_id).await?;
    let idx = next_index(state.db(), session_id).await?;
    insert_turn(
        state.db(),
        session_id,
        idx,
        "extension",
        "tool_result",
        &result.to_string(),
    )
    .await?;

    // TODO(P1): feed the tool result back to the LLM and produce the next turn.
    let turn = Turn::GiveUp {
        reason: "agentic loop not yet implemented".into(),
        turn_index: idx + 1,
    };
    persist_turn(state.db(), session_id, &turn).await?;
    Ok(turn)
}

pub async fn handle_clarify_answer(
    state: &AppState,
    session_id: &str,
    answer: String,
) -> AppResult<Turn> {
    let _ = ensure_active(state.db(), session_id).await?;
    let idx = next_index(state.db(), session_id).await?;
    insert_turn(
        state.db(),
        session_id,
        idx,
        "user",
        "clarify_answer",
        &json!({ "answer": answer }).to_string(),
    )
    .await?;
    let turn = Turn::GiveUp {
        reason: "agentic loop not yet implemented".into(),
        turn_index: idx + 1,
    };
    persist_turn(state.db(), session_id, &turn).await?;
    Ok(turn)
}

pub async fn record_result(
    state: &AppState,
    session_id: &str,
    payload: Value,
) -> AppResult<()> {
    let _ = ensure_active(state.db(), session_id).await?;
    let idx = next_index(state.db(), session_id).await?;
    insert_turn(
        state.db(),
        session_id,
        idx,
        "extension",
        "result",
        &payload.to_string(),
    )
    .await?;
    sqlx::query("UPDATE sessions SET status = 'completed', updated_at = ?1 WHERE id = ?2")
        .bind(Utc::now().timestamp_millis())
        .bind(session_id)
        .execute(state.db())
        .await?;
    Ok(())
}

pub async fn fetch_full(
    state: &AppState,
    session_id: &str,
) -> AppResult<Value> {
    let row: Option<(String, String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT id, url, host, route, intention, created_at, updated_at FROM sessions WHERE id = ?1",
    )
    .bind(session_id)
    .fetch_optional(state.db())
    .await?;
    let Some((id, url, host, route, intention, created_at, updated_at)) = row else {
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
        "created_at": created_at,
        "updated_at": updated_at,
        "turns": turns,
    }))
}

async fn ensure_active(db: &SqlitePool, session_id: &str) -> AppResult<()> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM sessions WHERE id = ?1")
            .bind(session_id)
            .fetch_optional(db)
            .await?;
    match row {
        None => Err(AppError::SessionNotFound),
        Some((s,)) if s == "active" => Ok(()),
        Some(_) => Err(AppError::SessionExpired),
    }
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

async fn persist_turn(db: &SqlitePool, session_id: &str, turn: &Turn) -> AppResult<()> {
    let (idx, kind, payload) = match turn {
        Turn::ToolCall { turn_index, .. } => (*turn_index, "tool_call", serde_json::to_string(turn).unwrap()),
        Turn::Clarify { turn_index, .. } => (*turn_index, "clarify", serde_json::to_string(turn).unwrap()),
        Turn::Plan { turn_index, .. } => (*turn_index, "plan", serde_json::to_string(turn).unwrap()),
        Turn::GiveUp { turn_index, .. } => (*turn_index, "give_up", serde_json::to_string(turn).unwrap()),
    };
    insert_turn(db, session_id, idx, "llm", kind, &payload).await?;
    Ok(())
}

fn hash_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Tiny URL parser to avoid pulling the `url` crate just for host+path.
mod url {
    pub fn host_and_path(input: &str) -> Result<(String, String), &'static str> {
        let rest = input.strip_prefix("https://")
            .or_else(|| input.strip_prefix("http://"))
            .ok_or("scheme must be http(s)")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() { return Err("missing host"); }
        // Strip query/fragment.
        let path = path.split(['?', '#']).next().unwrap_or("/");
        Ok((authority.to_string(), path.to_string()))
    }
}
