use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("session not found")]
    SessionNotFound,
    #[error("session expired")]
    SessionExpired,
    #[error("turn limit exceeded")]
    TurnLimitExceeded,
    #[error("invalid tool result: {0}")]
    InvalidToolResult(String),
    #[error("invalid plan emitted by model: {0}")]
    InvalidPlan(String),
    #[error("llm timeout")]
    LlmTimeout,
    #[error("llm unavailable: {0}")]
    LlmUnavailable(String),
    #[error("bad signals: {0}")]
    BadSignals(String),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("internal: {0}")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        use AppError::*;
        match self {
            SessionNotFound => (StatusCode::NOT_FOUND, "SESSION_NOT_FOUND"),
            SessionExpired => (StatusCode::GONE, "SESSION_EXPIRED"),
            TurnLimitExceeded => (StatusCode::CONFLICT, "TURN_LIMIT_EXCEEDED"),
            InvalidToolResult(_) => (StatusCode::BAD_REQUEST, "INVALID_TOOL_RESULT"),
            InvalidPlan(_) => (StatusCode::BAD_GATEWAY, "INVALID_PLAN"),
            LlmTimeout => (StatusCode::GATEWAY_TIMEOUT, "LLM_TIMEOUT"),
            LlmUnavailable(_) => (StatusCode::BAD_GATEWAY, "LLM_UNAVAILABLE"),
            BadSignals(_) => (StatusCode::BAD_REQUEST, "BAD_SIGNALS"),
            Db(_) | Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let body = Json(json!({
            "error": { "code": code, "message": self.to_string() }
        }));
        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
