//! Session lifecycle for the code-generation flow.
//!
//! Each session is now a one-shot: create() collects signals, asks the agent
//! for code, persists everything, returns. Cached code blobs live in `plans`
//! (table name kept for continuity — they're still keyed by host+route+intention).
//!
//! The handle_tool_result / handle_clarify_answer endpoints are retained as
//! deprecated no-ops so the older protocol shape doesn't 404 noisily.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use ulid::Ulid;

use crate::agent::{self, Envelope};
use crate::error::{AppError, AppResult};
use crate::signals::Signals;
use crate::state::AppState;

/// What the extension sees back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Turn {
    Code   { code: String, rationale: String, turn_index: u32, tokens_used: u32 },
    NoCode { reason: String, turn_index: u32 },
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

/// Telemetry from the extension after running the code. Drives cache scoring
/// and lets us mark sessions completed.
pub async fn record_result(
    state: &AppState,
    session_id: &str,
    payload: Value,
) -> AppResult<()> {
    // record_result fires after a code turn — the session has already been
    // marked completed by `advance`. We just need it to exist.
    session_must_exist(state.db(), session_id).await?;
    let idx = next_index(state.db(), session_id).await?;
    let body = payload.to_string();
    let items = payload.get("items_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let errs = payload.get("errors").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);

    tracing::info!(
        target: "scrape_backend::session",
        session_id = %session_id,
        items_count = items,
        error_count = errs,
        bytes = body.len(),
        payload = %truncate_for_log(&body, 1_500),
        "extension result received"
    );

    insert_turn(state.db(), session_id, idx, "extension", "result", &body).await?;
    sqlx::query("UPDATE sessions SET status = 'completed', updated_at = ?1 WHERE id = ?2")
        .bind(Utc::now().timestamp_millis())
        .bind(session_id)
        .execute(state.db())
        .await?;
    Ok(())
}

/// Append a follow-up user message and run the next agent step. This is the
/// multi-turn refinement path: the session must exist but doesn't need to be
/// in `active` status (completed and gave_up sessions can be revived by
/// continuing the conversation).
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
    // Re-open if we previously marked completed/gave_up.
    mark_status(state.db(), session_id, "active").await?;

    let idx = next_index(state.db(), session_id).await?;
    let payload = json!({ "content": trimmed }).to_string();
    tracing::info!(
        target: "scrape_backend::session",
        session_id = %session_id,
        turn_index = idx,
        content = %truncate_for_log(trimmed, 400),
        "extension user message received"
    );
    insert_turn(state.db(), session_id, idx, "user", "message", &payload).await?;
    advance(state, session_id).await
}

/// Deprecated. The old agentic loop with tool calls is gone; the endpoint
/// stays so old extension builds get a clear 410 instead of a 404.
pub async fn handle_tool_result(
    _state: &AppState,
    _session_id: &str,
    _tool_call_id: &str,
    _result: Value,
) -> AppResult<Turn> {
    Err(AppError::SessionExpired)
}

/// Deprecated, see `handle_tool_result`.
pub async fn handle_clarify_answer(
    _state: &AppState,
    _session_id: &str,
    _answer: String,
) -> AppResult<Turn> {
    Err(AppError::SessionExpired)
}

async fn session_must_exist(db: &SqlitePool, session_id: &str) -> AppResult<()> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM sessions WHERE id = ?1")
            .bind(session_id)
            .fetch_optional(db)
            .await?;
    if row.is_none() { Err(AppError::SessionNotFound) } else { Ok(()) }
}

pub async fn fetch_full(state: &AppState, session_id: &str) -> AppResult<Value> {
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

// ───────────────────── core driver: one LLM step per call ───────────────────

async fn advance(state: &AppState, session_id: &str) -> AppResult<Turn> {
    let env = agent::step(state, session_id).await?;
    let idx = next_index(state.db(), session_id).await?;
    let turn = match env {
        Envelope::Code { code, rationale } => Turn::Code {
            code,
            rationale,
            turn_index: idx,
            tokens_used: 0,
        },
        Envelope::NoCode { reason } => Turn::NoCode { reason, turn_index: idx },
    };
    persist_turn(state.db(), session_id, &turn).await?;
    tracing::debug!(
        target: "scrape_backend::session",
        session_id = %session_id,
        turn_index = idx,
        kind = turn_kind_str(&turn),
        "persisted turn"
    );

    match &turn {
        Turn::Code { .. } => {
            // Cache the code blob keyed on host+route_pattern+intention.
            cache_code(state, session_id, &turn).await.ok();
            mark_status(state.db(), session_id, "completed").await?;
            tracing::info!(target: "scrape_backend::session", %session_id, "session emitted code");
        }
        Turn::NoCode { reason, .. } => {
            mark_status(state.db(), session_id, "gave_up").await?;
            tracing::info!(target: "scrape_backend::session", %session_id, reason = %reason, "session no_code");
        }
    }
    Ok(turn)
}

fn turn_kind_str(t: &Turn) -> &'static str {
    match t {
        Turn::Code   { .. } => "code",
        Turn::NoCode { .. } => "no_code",
    }
}

// ───────────────────────────── code cache ───────────────────────────────────

async fn cache_code(state: &AppState, session_id: &str, turn: &Turn) -> AppResult<()> {
    let (code, rationale) = match turn {
        Turn::Code { code, rationale, .. } => (code.clone(), rationale.clone()),
        _ => return Ok(()),
    };
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT host, route, intention_hash FROM sessions WHERE id = ?1",
    )
    .bind(session_id)
    .fetch_optional(state.db())
    .await?;
    let Some((host, route, intention_hash)) = row else { return Ok(()); };
    let pattern = normalize_route(&host, &route, &state.config().routes);
    let now = Utc::now().timestamp_millis();
    let id = Ulid::new().to_string();
    // Reuse the plans table; plan_json now stores { code, rationale }.
    let plan_json = json!({ "code": code, "rationale": rationale }).to_string();

    sqlx::query(
        "INSERT INTO plans (id, host, route_pattern, intention_hash, plan_json, rationale,
                            success_count, failure_count, invalidated, created_at, last_used_at, source_session)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 0, ?7, ?7, ?8)
         ON CONFLICT(host, route_pattern, intention_hash) DO UPDATE SET
             plan_json    = excluded.plan_json,
             rationale    = excluded.rationale,
             last_used_at = excluded.last_used_at,
             invalidated  = 0,
             source_session = excluded.source_session",
    )
    .bind(&id)
    .bind(&host)
    .bind(&pattern)
    .bind(&intention_hash)
    .bind(&plan_json)
    .bind(&rationale)
    .bind(now)
    .bind(session_id)
    .execute(state.db())
    .await?;
    Ok(())
}

fn normalize_route(host: &str, route: &str, host_rules: &[crate::config::HostRoutes]) -> String {
    let lower = route.trim_end_matches('/').to_lowercase();
    let lower = if lower.is_empty() { "/".to_string() } else { lower };

    if let Some(rules) = host_rules.iter().find(|r| r.host == host) {
        for p in &rules.patterns {
            if let Ok(re) = regex::Regex::new(&p.match_re) {
                if re.is_match(&lower) {
                    return re.replace_all(&lower, p.replace.as_str()).into_owned();
                }
            }
        }
    }
    let parts: Vec<String> = lower
        .split('/')
        .map(|seg| {
            if seg.len() >= 16 && seg.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                ":id".to_string()
            } else if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()) {
                ":n".to_string()
            } else {
                seg.to_string()
            }
        })
        .collect();
    parts.join("/")
}

// ─────────────────────────── small DB helpers ───────────────────────────────

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
        Turn::Code   { turn_index, .. } => (*turn_index, "code",    serde_json::to_string(turn).unwrap()),
        Turn::NoCode { turn_index, .. } => (*turn_index, "no_code", serde_json::to_string(turn).unwrap()),
    };
    insert_turn(db, session_id, idx, "llm", kind, &payload).await?;
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
    let rest = input.strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .ok_or("scheme must be http(s)")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() { return Err("missing host"); }
    let path = path.split(['?', '#']).next().unwrap_or("/");
    Ok((authority.to_string(), path.to_string()))
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…[+{} chars]", &s[..max], s.len() - max) }
}
