//! Tool catalog for the function-calling agent.
//!
//! Six tools, two classes:
//!   - **Server-side** (`list_files`, `read_file`, `write_file`, `edit_file`) —
//!     resolved here against the per-origin SQLite file store (`origin_files`),
//!     no extension round-trip. This is the agent's persistent memory.
//!   - **Extension-side** (`execute_script`, `screenshot`) — run in the live tab
//!     by the extension; `agent.rs` / `session.rs` route these back over HTTP.
//!
//! The file-tool argument names and `edit_file`'s exact-once semantics mirror
//! the slop coding agent (`references/slop/slop-agent/src/tools/`); the edit
//! core is in `crate::edit_ops`.

use chrono::Utc;
use genai::chat::{Tool, ToolCall, ToolResponse};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::edit_ops::string_replace;

// Tool name constants (also the wire names the extension dispatches on).
pub const LIST_FILES: &str = "list_files";
pub const READ_FILE: &str = "read_file";
pub const WRITE_FILE: &str = "write_file";
pub const EDIT_FILE: &str = "edit_file";
pub const EXECUTE_SCRIPT: &str = "execute_script";
pub const SCREENSHOT: &str = "screenshot";

/// Tools that the backend resolves itself (against `origin_files`) without
/// handing control back to the extension.
pub fn is_server_side(name: &str) -> bool {
    matches!(name, LIST_FILES | READ_FILE | WRITE_FILE | EDIT_FILE)
}

/// The full tool set advertised to Gemini on every request.
pub fn catalog() -> Vec<Tool> {
    vec![
        Tool::new(LIST_FILES)
            .with_description(
                "List files in your persistent per-site memory store. Optionally filter by a path prefix. \
                 Use this at the start of a task to see what you (or a prior run) already saved for this site.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "description": "Only list paths starting with this prefix." }
                }
            })),
        Tool::new(READ_FILE)
            .with_description(
                "Read a file from your persistent per-site memory store (e.g. a scraper you saved earlier).",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path of the file to read." },
                    "line": { "type": "integer", "minimum": 1, "description": "1-based line to start from (optional)." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Max lines to read (optional)." }
                },
                "required": ["path"]
            })),
        Tool::new(WRITE_FILE)
            .with_description(
                "Write a file to your persistent per-site memory store. Overwrites if it exists. \
                 Save reusable scrapers, selectors, and notes here so future runs on this site are faster.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path of the file to write." },
                    "content": { "type": "string", "description": "Full file contents." }
                },
                "required": ["path", "content"]
            })),
        Tool::new(EDIT_FILE)
            .with_description(
                "Replace `before` with `after` in a stored file. `before` must occur exactly once; \
                 on ambiguity or absence the file is left untouched and an error with context is returned.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path of the file to edit." },
                    "before": { "type": "string", "description": "Exact text to find (must occur exactly once)." },
                    "after": { "type": "string", "description": "Replacement text (may be empty to delete)." }
                },
                "required": ["path", "before", "after"]
            })),
        Tool::new(EXECUTE_SCRIPT)
            .with_description(
                "Run JavaScript in the live page and get its return value back. The code is the body of an \
                 async function with access to document/window/location. Use it to probe structure and to \
                 perform the final extraction — end with `return { kind, items }` where kind is \"listing\" or \
                 \"event\" and items is an array of plain objects. The last such result becomes the saved artifact.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "JavaScript function body to execute in the page." }
                },
                "required": ["code"]
            })),
        Tool::new(SCREENSHOT)
            .with_description(
                "Capture a screenshot of the visible page. The image is returned to you so you can see the \
                 layout when the DOM/JSON structure is unclear. Use sparingly.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {}
            })),
    ]
}

/// Resolve a server-side tool call against the origin's file store. Always
/// returns a `ToolResponse` (errors are encoded as `{ "error": ... }` content
/// so the model can read and self-correct).
pub async fn run_server_tool(db: &SqlitePool, origin: &str, call: &ToolCall) -> ToolResponse {
    let result = match call.fn_name.as_str() {
        LIST_FILES => list_files(db, origin, &call.fn_arguments).await,
        READ_FILE => read_file(db, origin, &call.fn_arguments).await,
        WRITE_FILE => write_file(db, origin, &call.fn_arguments).await,
        EDIT_FILE => edit_file(db, origin, &call.fn_arguments).await,
        other => Err(format!("unknown server tool: {other}")),
    };
    let content = match result {
        Ok(v) => v.to_string(),
        Err(msg) => json!({ "error": msg }).to_string(),
    };
    ToolResponse::new(call.call_id.clone(), content)
}

async fn list_files(db: &SqlitePool, origin: &str, args: &Value) -> Result<Value, String> {
    let prefix = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
    let like = format!("{}%", prefix);
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT path, length(content) AS size, updated_at FROM origin_files
         WHERE origin = ?1 AND path LIKE ?2 ORDER BY path ASC",
    )
    .bind(origin)
    .bind(like)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;

    let files: Vec<Value> = rows
        .into_iter()
        .map(|(path, size, updated_at)| json!({ "path": path, "size": size, "updated_at": updated_at }))
        .collect();
    Ok(json!({ "files": files }))
}

async fn read_file(db: &SqlitePool, origin: &str, args: &Value) -> Result<Value, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("`path` is required")?;
    let row: Option<(String,)> =
        sqlx::query_as("SELECT content FROM origin_files WHERE origin = ?1 AND path = ?2")
            .bind(origin)
            .bind(path)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;
    let Some((content,)) = row else {
        return Err(format!("no such file: {path}"));
    };

    let line = args.get("line").and_then(|v| v.as_u64());
    let limit = args.get("limit").and_then(|v| v.as_u64());
    if line.is_none() && limit.is_none() {
        return Ok(json!({ "path": path, "content": content }));
    }
    // Line-window read (1-based start).
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = line.map(|n| (n as usize).saturating_sub(1)).unwrap_or(0).min(total);
    let end = match limit {
        Some(l) => (start + l as usize).min(total),
        None => total,
    };
    let window = lines[start..end].join("\n");
    Ok(json!({
        "path": path,
        "content": window,
        "start_line": start + 1,
        "total_lines": total,
    }))
}

async fn write_file(db: &SqlitePool, origin: &str, args: &Value) -> Result<Value, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("`path` is required")?;
    let content = args.get("content").and_then(|v| v.as_str()).ok_or("`content` is required")?;
    let now = Utc::now().timestamp_millis();
    sqlx::query(
        "INSERT INTO origin_files (origin, path, content, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(origin, path) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
    )
    .bind(origin)
    .bind(path)
    .bind(content)
    .bind(now)
    .execute(db)
    .await
    .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true, "path": path, "bytes": content.len() }))
}

async fn edit_file(db: &SqlitePool, origin: &str, args: &Value) -> Result<Value, String> {
    let path = args.get("path").and_then(|v| v.as_str()).ok_or("`path` is required")?;
    let before = args.get("before").and_then(|v| v.as_str()).ok_or("`before` is required")?;
    let after = args.get("after").and_then(|v| v.as_str()).ok_or("`after` is required")?;

    let row: Option<(String,)> =
        sqlx::query_as("SELECT content FROM origin_files WHERE origin = ?1 AND path = ?2")
            .bind(origin)
            .bind(path)
            .fetch_optional(db)
            .await
            .map_err(|e| e.to_string())?;
    let Some((content,)) = row else {
        return Err(format!("no such file: {path}"));
    };

    // Exact-once search-and-replace; leaves the file untouched on ambiguity.
    let new_content = string_replace(&content, before, after)?;
    let now = Utc::now().timestamp_millis();
    sqlx::query("UPDATE origin_files SET content = ?1, updated_at = ?2 WHERE origin = ?3 AND path = ?4")
        .bind(&new_content)
        .bind(now)
        .bind(origin)
        .bind(path)
        .execute(db)
        .await
        .map_err(|e| e.to_string())?;

    let old_lines = before.lines().count();
    let new_lines = after.lines().count();
    Ok(json!({ "ok": true, "path": path, "message": format!("Edited {path} ({old_lines} lines -> {new_lines} lines)") }))
}
