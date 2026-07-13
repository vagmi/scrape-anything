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
use regex::RegexBuilder;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::edit_ops::string_replace;

// Tool name constants (also the wire names the extension dispatches on).
pub const LIST_FILES: &str = "list_files";
pub const READ_FILE: &str = "read_file";
pub const WRITE_FILE: &str = "write_file";
pub const EDIT_FILE: &str = "edit_file";
pub const SEARCH_FILE: &str = "search_file";
pub const CONCAT_FILES: &str = "concat_files";
pub const EXECUTE_SCRIPT: &str = "execute_script";
pub const SCREENSHOT: &str = "screenshot";
pub const PAGE_TO_MARKDOWN: &str = "page_to_markdown";

/// Tools that the backend resolves itself (against `origin_files`) without
/// handing control back to the extension.
pub fn is_server_side(name: &str) -> bool {
    matches!(
        name,
        LIST_FILES | READ_FILE | WRITE_FILE | EDIT_FILE | SEARCH_FILE | CONCAT_FILES
    )
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
        Tool::new(SEARCH_FILE)
            .with_description(
                "Search your stored files with a regular expression and get back matching line numbers \
                 (ripgrep-style: path:line:text). Use this on large files — especially page dumps saved by \
                 page_to_markdown — to locate the sections you care about, then `read_file` those line \
                 ranges instead of reading the whole file. Omit `path` to search every file for this site.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression to search for." },
                    "path": { "type": "string", "description": "Limit the search to this file (optional; default: all files)." },
                    "ignore_case": { "type": "boolean", "description": "Case-insensitive match (default false)." },
                    "max_matches": { "type": "integer", "minimum": 1, "description": "Cap on returned matches (default 50)." }
                },
                "required": ["pattern"]
            })),
        Tool::new(CONCAT_FILES)
            .with_description(
                "Concatenate the contents of several stored files (in the given order) and write the result \
                 to `dest`. Use it to combine page dumps or partial results into one file — e.g. append a \
                 newly saved dump onto a running collection. `dest` may be one of the sources (that file is \
                 read before it is overwritten), so `sources:[\"all.md\",\"new.md\"], dest:\"all.md\"` appends \
                 new.md onto all.md.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {
                    "sources": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "Paths to concatenate, in order."
                    },
                    "dest": { "type": "string", "description": "Path to write the combined content to." },
                    "separator": { "type": "string", "description": "Text inserted between files (default: a newline)." }
                },
                "required": ["sources", "dest"]
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
        Tool::new(PAGE_TO_MARKDOWN)
            .with_description(
                "Dump the live page's full HTML, convert it to clean Markdown (via pandoc), and SAVE it to a \
                 file in your per-site store. Returns a small pointer `{ saved_to, bytes, lines }` — NOT the \
                 content — so you don't burn tokens on the whole page. Then use `search_file` to find the \
                 sections you need and `read_file` (with `line`/`limit`) to read just those ranges. Good for \
                 text-heavy pages (articles, docs, event details). For structured extraction into rows, still \
                 finish with `execute_script` returning `{ kind, items }`.",
            )
            .with_schema(json!({
                "type": "object",
                "properties": {}
            })),
    ]
}

/// Convert a full HTML document to Markdown by shelling out to `pandoc`.
/// Extension-side tools produce the HTML in the live tab; the conversion runs
/// here on the server (the extension can't shell out). Errors are returned as
/// `Err(String)` so callers can encode them for the model to see.
pub async fn html_to_markdown(html: &str) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let mut child = Command::new("pandoc")
        .args([
            "--from=html",
            // gfm minus raw_html: drop tags pandoc can't map so the output stays
            // readable Markdown instead of a wall of leftover HTML.
            "--to=gfm-raw_html",
            "--wrap=none",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch pandoc (is it installed?): {e}"))?;

    // Stream the HTML in on stdin, then collect stdout. Take the handle so the
    // pipe closes (EOF) before we await the output, avoiding a deadlock.
    let mut stdin = child.stdin.take().ok_or("failed to open pandoc stdin")?;
    let html_owned = html.to_string();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(html_owned.as_bytes()).await;
        // stdin dropped here → EOF to pandoc.
    });

    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
    let _ = writer.await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("pandoc exited with {}: {}", output.status, stderr.trim()));
    }
    let markdown = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(markdown)
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
        SEARCH_FILE => search_file(db, origin, &call.fn_arguments).await,
        CONCAT_FILES => concat_files(db, origin, &call.fn_arguments).await,
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
    write_origin_file(db, origin, path, content).await?;
    Ok(json!({ "ok": true, "path": path, "bytes": content.len() }))
}

/// Upsert a file into the per-origin store. Shared by `write_file` and by
/// server-side producers like the `page_to_markdown` dump path.
pub async fn write_origin_file(
    db: &SqlitePool,
    origin: &str,
    path: &str,
    content: &str,
) -> Result<(), String> {
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
    Ok(())
}

/// Regex search over the origin's stored files. Returns ripgrep-style matches
/// (`{ path, line, text }`) so the model can then `read_file` the right window
/// instead of loading a whole large file (e.g. a page dump) into context.
async fn search_file(db: &SqlitePool, origin: &str, args: &Value) -> Result<Value, String> {
    let pattern = args.get("pattern").and_then(|v| v.as_str()).ok_or("`pattern` is required")?;
    let path = args.get("path").and_then(|v| v.as_str());
    let ignore_case = args.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false);
    let max_matches = args.get("max_matches").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

    let re = RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;

    let rows: Vec<(String, String)> = match path {
        Some(p) => sqlx::query_as("SELECT path, content FROM origin_files WHERE origin = ?1 AND path = ?2")
            .bind(origin)
            .bind(p)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?,
        None => sqlx::query_as("SELECT path, content FROM origin_files WHERE origin = ?1 ORDER BY path ASC")
            .bind(origin)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?,
    };
    if let Some(p) = path {
        if rows.is_empty() {
            return Err(format!("no such file: {p}"));
        }
    }

    let mut matches: Vec<Value> = Vec::new();
    let mut truncated = false;
    'outer: for (fpath, content) in &rows {
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                if matches.len() >= max_matches {
                    truncated = true;
                    break 'outer;
                }
                // Clip long lines on a char boundary so matches stay compact.
                let text: String = line.chars().take(300).collect();
                let text = if line.chars().count() > 300 { format!("{text}…") } else { text };
                matches.push(json!({ "path": fpath, "line": i + 1, "text": text }));
            }
        }
    }

    Ok(json!({ "matches": matches, "count": matches.len(), "truncated": truncated }))
}

/// Concatenate `sources` (in order) and write the result to `dest`. All sources
/// are read up front, so `dest` may itself be one of the sources (append).
async fn concat_files(db: &SqlitePool, origin: &str, args: &Value) -> Result<Value, String> {
    let sources = args.get("sources").and_then(|v| v.as_array()).ok_or("`sources` is required")?;
    if sources.is_empty() {
        return Err("`sources` must list at least one path".into());
    }
    let dest = args.get("dest").and_then(|v| v.as_str()).ok_or("`dest` is required")?;
    let separator = args.get("separator").and_then(|v| v.as_str()).unwrap_or("\n");

    let mut parts: Vec<String> = Vec::with_capacity(sources.len());
    for src in sources {
        let path = src.as_str().ok_or("each source must be a string path")?;
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
        parts.push(content);
    }

    let combined = parts.join(separator);
    write_origin_file(db, origin, dest, &combined).await?;
    Ok(json!({ "ok": true, "dest": dest, "bytes": combined.len(), "sources": sources }))
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
