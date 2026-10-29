//! File uploads and the session workspace over HTTP.
//!
//! `POST /api/uploads` stages files before a session exists (the side panel
//! creates the session on the first send); the returned upload ids are passed
//! as `attachments` on create/message, which moves them into the session's
//! workspace. The workspace (uploads + whatever the agent wrote) is listed and
//! downloadable for the dashboard.

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use ulid::Ulid;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::workspace;

pub fn router(max_upload_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/api/uploads", post(upload).layer(DefaultBodyLimit::max(max_upload_bytes)))
        .route("/api/sessions/{id}/files", get(list_files))
        .route("/api/sessions/{id}/files/{*path}", get(download_file))
}

/// Multipart upload; every part with a filename becomes one staged file.
/// Returns `{ uploads: [{ upload_id, name, size }] }`.
async fn upload(State(s): State<AppState>, mut form: Multipart) -> AppResult<Json<Value>> {
    let mut uploads = Vec::new();
    while let Some(mut field) = form.next_field().await.map_err(bad_upload)? {
        let Some(file_name) = field.file_name() else { continue };
        let name = workspace::sanitize_filename(file_name);
        let upload_id = Ulid::new().to_string();
        let dir = workspace::staging_dir(s.sandbox().data_dir(), &upload_id).map_err(AppError::BadSignals)?;
        tokio::fs::create_dir_all(&dir).await.map_err(anyhow::Error::from)?;

        // Stream to disk chunk by chunk; uploads can be large.
        let mut file = tokio::fs::File::create(dir.join(&name)).await.map_err(anyhow::Error::from)?;
        let mut size = 0u64;
        while let Some(chunk) = field.chunk().await.map_err(bad_upload)? {
            size += chunk.len() as u64;
            file.write_all(&chunk).await.map_err(anyhow::Error::from)?;
        }
        file.flush().await.map_err(anyhow::Error::from)?;
        tracing::info!(%upload_id, %name, size, "file staged");
        uploads.push(json!({ "upload_id": upload_id, "name": name, "size": size }));
    }
    if uploads.is_empty() {
        return Err(AppError::BadSignals("no files in the upload".into()));
    }
    Ok(Json(json!({ "uploads": uploads })))
}

fn bad_upload(e: axum::extract::multipart::MultipartError) -> AppError {
    AppError::BadSignals(format!("upload failed: {e}"))
}

async fn list_files(State(s): State<AppState>, Path(id): Path<String>) -> AppResult<Json<Value>> {
    session_exists(&s, &id).await?;
    let files: Vec<Value> = s
        .sandbox()
        .workspace(&id)
        .list("")
        .into_iter()
        .map(|(path, size)| json!({ "path": path, "size": size }))
        .collect();
    Ok(Json(json!({ "files": files })))
}

/// `GET /api/sessions/{id}/files/uploads/leads.csv` — the path is relative to
/// the workspace (with or without the `work/` prefix).
async fn download_file(
    State(s): State<AppState>,
    Path((id, path)): Path<(String, String)>,
) -> Result<Response, AppError> {
    session_exists(&s, &id).await?;
    let virt = if workspace::is_workspace_path(&path) { path } else { format!("{}{path}", workspace::PREFIX) };
    let full = s.sandbox().workspace(&id).resolve(&virt).map_err(AppError::BadSignals)?;
    let Ok(bytes) = tokio::fs::read(&full).await else {
        return Ok((StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "FILE_NOT_FOUND", "message": virt } })))
            .into_response());
    };
    let name = full.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type(&name).to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        Body::from(bytes),
    )
        .into_response())
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("json") => "application/json",
        Some("csv") => "text/csv; charset=utf-8",
        Some("jsonl" | "ndjson") => "application/x-ndjson",
        Some("txt" | "md" | "py" | "js" | "ts" | "sql" | "log") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn session_exists(s: &AppState, id: &str) -> AppResult<()> {
    let row: Option<(String,)> = sqlx::query_as("SELECT id FROM sessions WHERE id = ?1")
        .bind(id)
        .fetch_optional(s.db())
        .await?;
    row.map(|_| ()).ok_or(AppError::SessionNotFound)
}
