use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppResult;
use crate::session::{self, Turn};
use crate::signals::Signals;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/sessions", post(create))
        .route("/api/sessions/{id}", get(fetch))
        .route("/api/sessions/{id}/message", post(message))
        .route("/api/sessions/{id}/tool_result", post(tool_result))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/clarify", post(clarify_answer))
        .route("/api/sessions/{id}/result", post(result))
}

#[derive(Deserialize)]
pub struct MessageReq { pub content: String }

async fn message(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MessageReq>,
) -> AppResult<Json<TurnResp>> {
    let turn = session::handle_message(&s, &id, req.content).await?;
    Ok(Json(TurnResp { turn }))
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub url: String,
    pub intention: String,
    pub signals: Signals,
}

#[derive(Serialize)]
pub struct CreateResp {
    pub session_id: String,
    pub turn: Turn,
}

async fn create(State(s): State<AppState>, Json(req): Json<CreateReq>) -> AppResult<Json<CreateResp>> {
    let (session_id, turn) = session::create(&s, req.url, req.intention, req.signals).await?;
    Ok(Json(CreateResp { session_id, turn }))
}

#[derive(Deserialize)]
pub struct ToolResultReq {
    pub results: Vec<session::ToolResultItem>,
}

#[derive(Serialize)]
pub struct TurnResp { pub turn: Turn }

async fn tool_result(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ToolResultReq>,
) -> AppResult<Json<TurnResp>> {
    let turn = session::handle_tool_result(&s, &id, req.results).await?;
    Ok(Json(TurnResp { turn }))
}

async fn cancel(State(s): State<AppState>, Path(id): Path<String>) -> AppResult<Json<Value>> {
    session::cancel(&s, &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ClarifyReq { pub answer: String }
async fn clarify_answer(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ClarifyReq>,
) -> AppResult<Json<TurnResp>> {
    let turn = session::handle_clarify_answer(&s, &id, req.answer).await?;
    Ok(Json(TurnResp { turn }))
}

async fn result(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<Value>,
) -> AppResult<Json<Value>> {
    session::record_result(&s, &id, payload).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn fetch(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(session::fetch_full(&s, &id).await?))
}
