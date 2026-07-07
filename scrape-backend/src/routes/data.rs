//! JSON read/download routes powering the dashboard and the extension's
//! history/restore: session list, per-session artifact, artifact downloads
//! (JSON/CSV), and the per-origin file store.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::csv;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/artifact", get(session_artifact))
        .route("/api/artifacts/{file}", get(download_artifact))
        .route("/api/origins/{host}/files", get(list_origin_files))
        .route("/api/origins/{host}/files/{*path}", get(read_origin_file))
}

#[derive(Deserialize)]
pub struct ListQ {
    #[serde(default)]
    pub q: Option<String>,
}

async fn list_sessions(
    State(s): State<AppState>,
    Query(q): Query<ListQ>,
) -> AppResult<Json<Value>> {
    let like = format!("%{}%", q.q.unwrap_or_default());
    let rows: Vec<(String, String, String, String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT s.id, s.url, s.host, s.intention, s.status, s.created_at, s.updated_at,
                COALESCE(a.item_count, 0) AS item_count
         FROM sessions s
         LEFT JOIN artifacts a ON a.session_id = s.id
         WHERE s.intention LIKE ?1 OR s.url LIKE ?1 OR s.host LIKE ?1
         ORDER BY s.created_at DESC
         LIMIT 200",
    )
    .bind(&like)
    .fetch_all(s.db())
    .await?;

    let sessions: Vec<Value> = rows
        .into_iter()
        .map(|(id, url, host, intention, status, created_at, updated_at, item_count)| {
            json!({
                "session_id": id, "url": url, "host": host, "intention": intention,
                "status": status, "created_at": created_at, "updated_at": updated_at,
                "item_count": item_count,
            })
        })
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

async fn session_artifact(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    let row: Option<(String, String, i64, String, i64)> = sqlx::query_as(
        "SELECT id, kind, item_count, items_json, updated_at FROM artifacts WHERE session_id = ?1",
    )
    .bind(&id)
    .fetch_optional(s.db())
    .await?;
    let Some((artifact_id, kind, item_count, items_json, updated_at)) = row else {
        return Err(AppError::SessionNotFound);
    };
    let items: Value = serde_json::from_str(&items_json).unwrap_or(Value::Array(vec![]));
    Ok(Json(json!({
        "artifact_id": artifact_id,
        "kind": kind,
        "item_count": item_count,
        "items": items,
        "updated_at": updated_at,
    })))
}

/// `/api/artifacts/{id}.json` or `/api/artifacts/{id}.csv`.
async fn download_artifact(
    State(s): State<AppState>,
    Path(file): Path<String>,
) -> Result<Response, AppError> {
    let (id, ext) = file.rsplit_once('.').unwrap_or((file.as_str(), "json"));
    let row: Option<(String,)> =
        sqlx::query_as("SELECT items_json FROM artifacts WHERE id = ?1")
            .bind(id)
            .fetch_optional(s.db())
            .await?;
    let Some((items_json,)) = row else {
        return Err(AppError::SessionNotFound);
    };
    let items: Vec<Value> = serde_json::from_str(&items_json).unwrap_or_default();

    match ext {
        "csv" => {
            let body = csv::to_csv(&items);
            Ok((
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{id}.csv\""),
                    ),
                ],
                body,
            )
                .into_response())
        }
        _ => {
            let body = serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into());
            Ok((
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/json".to_string()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{id}.json\""),
                    ),
                ],
                body,
            )
                .into_response())
        }
    }
}

async fn list_origin_files(
    State(s): State<AppState>,
    Path(host): Path<String>,
) -> AppResult<Json<Value>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT path, length(content), updated_at FROM origin_files WHERE origin = ?1 ORDER BY path ASC",
    )
    .bind(&host)
    .fetch_all(s.db())
    .await?;
    let files: Vec<Value> = rows
        .into_iter()
        .map(|(path, size, updated_at)| json!({ "path": path, "size": size, "updated_at": updated_at }))
        .collect();
    Ok(Json(json!({ "origin": host, "files": files })))
}

async fn read_origin_file(
    State(s): State<AppState>,
    Path((host, path)): Path<(String, String)>,
) -> AppResult<Json<Value>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT content, updated_at FROM origin_files WHERE origin = ?1 AND path = ?2",
    )
    .bind(&host)
    .bind(&path)
    .fetch_optional(s.db())
    .await?;
    let Some((content, updated_at)) = row else {
        return Err(AppError::SessionNotFound);
    };
    Ok(Json(json!({ "origin": host, "path": path, "content": content, "updated_at": updated_at })))
}
