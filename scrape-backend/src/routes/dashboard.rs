//! Server-rendered HTML fragments for the dashboard (Vite static shell +
//! htmx). Each route returns a `maud` fragment that htmx swaps into the page.
//! The static shell (index.html + compiled Tailwind CSS + htmx) is served
//! separately by a `ServeDir` fallback wired in `routes::build`.

use axum::{
    extract::{Path, Query, State},
    response::Html,
    routing::get,
    Router,
};
use chrono::DateTime;
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::Value;

use crate::error::AppResult;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ui/sessions", get(sessions_fragment))
        .route("/ui/sessions/{id}", get(session_detail))
        .route("/ui/origins/{host}/files", get(origin_files))
        .route("/ui/origins/{host}/file", get(origin_file)) // ?path=
}

fn fmt_ts(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[derive(Deserialize)]
pub struct ListQ {
    #[serde(default)]
    pub q: Option<String>,
}

async fn sessions_fragment(
    State(s): State<AppState>,
    Query(q): Query<ListQ>,
) -> AppResult<Html<String>> {
    let like = format!("%{}%", q.q.unwrap_or_default());
    let rows: Vec<(String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT s.id, s.host, s.intention, s.status, s.created_at,
                COALESCE(a.item_count, 0)
         FROM sessions s
         LEFT JOIN artifacts a ON a.session_id = s.id
         WHERE s.intention LIKE ?1 OR s.url LIKE ?1 OR s.host LIKE ?1
         ORDER BY s.created_at DESC
         LIMIT 200",
    )
    .bind(&like)
    .fetch_all(s.db())
    .await?;

    let markup = html! {
        @if rows.is_empty() {
            p class="text-sm text-gray-500 p-3" { "No sessions yet." }
        } @else {
            @for (id, host, intention, status, created_at, item_count) in &rows {
                div
                    class="session-card cursor-pointer rounded-lg border border-gray-200 dark:border-gray-700 p-3 mb-2 hover:border-indigo-400 hover:bg-indigo-50/40 dark:hover:bg-indigo-500/10"
                    hx-get={ "/ui/sessions/" (id) }
                    hx-target="#detail"
                    hx-swap="innerHTML"
                {
                    div class="font-medium text-gray-900 dark:text-gray-100 line-clamp-2" { (intention) }
                    div class="mt-1 flex items-center gap-2 text-xs text-gray-500" {
                        span class="font-mono" { (host) }
                        span { "·" }
                        span { (status_badge(status)) }
                        span { "·" }
                        span { (item_count) " items" }
                        span class="ml-auto" { (fmt_ts(*created_at)) }
                    }
                }
            }
        }
    };
    Ok(Html(markup.into_string()))
}

fn status_badge(status: &str) -> Markup {
    let cls = match status {
        "completed" => "text-green-600",
        "gave_up" => "text-red-600",
        "active" => "text-amber-600",
        _ => "text-gray-500",
    };
    html! { span class=(cls) { (status) } }
}

async fn session_detail(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let session = crate::session::fetch_full(&s, &id).await?;

    let host = session.get("host").and_then(|v| v.as_str()).unwrap_or("");
    let url = session.get("url").and_then(|v| v.as_str()).unwrap_or("");
    let intention = session.get("intention").and_then(|v| v.as_str()).unwrap_or("");
    let status = session.get("status").and_then(|v| v.as_str()).unwrap_or("");
    let turns = session.get("turns").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    // Artifact (if any) for download links.
    let artifact: Option<(String, i64, String)> = sqlx::query_as(
        "SELECT id, item_count, kind FROM artifacts WHERE session_id = ?1",
    )
    .bind(&id)
    .fetch_optional(s.db())
    .await?;

    let markup = html! {
        div class="mb-4" {
            h2 class="text-lg font-semibold text-gray-900 dark:text-gray-100" { (intention) }
            div class="mt-1 flex items-center gap-2 text-xs text-gray-500" {
                a class="font-mono hover:underline" href=(url) target="_blank" { (url) }
                span { "·" }
                span { (status_badge(status)) }
            }
        }

        @if let Some((art_id, item_count, kind)) = &artifact {
            div class="mb-4 flex items-center gap-3 rounded-lg border border-gray-200 dark:border-gray-700 p-3" {
                span class="text-sm text-gray-700 dark:text-gray-300" { (item_count) " " (kind) " rows" }
                a class="btn" href={ "/api/artifacts/" (art_id) ".json" } { "JSON" }
                a class="btn" href={ "/api/artifacts/" (art_id) ".csv" } { "CSV" }
            }
        }

        div class="flex flex-col gap-2" {
            @for turn in &turns {
                (render_turn(turn))
            }
        }

        div class="mt-5 pt-4 border-t border-gray-200 dark:border-gray-700" {
            button
                class="btn"
                hx-get={ "/ui/origins/" (host) "/files" }
                hx-target="#detail"
                hx-swap="innerHTML"
            { "View memory files for " span class="font-mono" { (host) } }
        }
    };
    Ok(Html(markup.into_string()))
}

fn render_turn(turn: &Value) -> Markup {
    let role = turn.get("role").and_then(|v| v.as_str()).unwrap_or("");
    let kind = turn.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let payload = turn.get("payload").cloned().unwrap_or(Value::Null);

    match (role, kind) {
        ("user", "message") => {
            let content = payload.get("content").and_then(|v| v.as_str()).unwrap_or("");
            bubble("you", "border-indigo-200 bg-indigo-50/40 dark:bg-indigo-500/10", html! { (content) })
        }
        ("llm", "assistant") => {
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let calls = payload.get("tool_calls").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if !calls.is_empty() {
                bubble("assistant · tools", "border-gray-200 dark:border-gray-700", html! {
                    @for c in &calls {
                        (render_tool_call(c))
                    }
                })
            } else if !text.is_empty() {
                bubble("assistant", "border-gray-200 dark:border-gray-700", html! { (text) })
            } else {
                html! {}
            }
        }
        ("tool", "tool_response") => {
            let fn_name = payload.get("fn_name").and_then(|v| v.as_str()).unwrap_or("tool");
            let content = payload.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let is_img = payload.get("image_base64").is_some();
            bubble("result", "border-gray-200 dark:border-gray-700 bg-gray-50 dark:bg-gray-800/40", html! {
                div class="font-mono text-xs text-gray-500 mb-1" { (fn_name) }
                @if is_img {
                    span class="text-sm text-gray-600 dark:text-gray-300" { "📷 screenshot" }
                } @else {
                    pre class="m-0 text-xs whitespace-pre-wrap break-words max-h-40 overflow-auto" { (truncate(content, 4000)) }
                }
            })
        }
        _ => html! {},
    }
}

fn render_tool_call(call: &Value) -> Markup {
    let fn_name = call.get("fn_name").and_then(|v| v.as_str()).unwrap_or("");
    let args = call.get("fn_arguments").cloned().unwrap_or(Value::Null);
    let code = args.get("code").and_then(|v| v.as_str());
    html! {
        div class="mb-2" {
            div class="font-mono text-xs text-indigo-600 dark:text-indigo-400" { "⚙️ " (fn_name) }
            @if let Some(code) = code {
                details class="mt-1" {
                    summary class="text-xs text-gray-500 cursor-pointer" { "code" }
                    pre class="m-0 mt-1 text-xs whitespace-pre-wrap break-words max-h-56 overflow-auto rounded bg-gray-50 dark:bg-gray-800/60 p-2" { (code) }
                }
            } @else if !args.is_null() {
                pre class="m-0 mt-1 text-xs whitespace-pre-wrap break-words rounded bg-gray-50 dark:bg-gray-800/60 p-2" { (args.to_string()) }
            }
        }
    }
}

fn bubble(label: &str, border: &str, body: Markup) -> Markup {
    html! {
        div class={ "rounded-lg border p-3 " (border) } {
            div class="text-xs text-gray-400 mb-1" { (label) }
            div class="text-sm text-gray-800 dark:text-gray-200 whitespace-pre-wrap break-words" { (body) }
        }
    }
}

async fn origin_files(
    State(s): State<AppState>,
    Path(host): Path<String>,
) -> AppResult<Html<String>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT path, length(content), updated_at FROM origin_files WHERE origin = ?1 ORDER BY path ASC",
    )
    .bind(&host)
    .fetch_all(s.db())
    .await?;

    let markup = html! {
        div class="mb-3 flex items-center gap-2" {
            h2 class="text-lg font-semibold text-gray-900 dark:text-gray-100" { "Memory · " span class="font-mono" { (host) } }
        }
        @if rows.is_empty() {
            p class="text-sm text-gray-500" { "No files stored for this site yet." }
        } @else {
            div class="flex flex-col gap-1" {
                @for (path, size, updated_at) in &rows {
                    div
                        class="session-card cursor-pointer rounded-lg border border-gray-200 dark:border-gray-700 p-2.5 hover:border-indigo-400"
                        hx-get={ "/ui/origins/" (host) "/file?path=" (urlencode(path)) }
                        hx-target="#detail"
                        hx-swap="innerHTML"
                    {
                        div class="flex items-center gap-2 text-sm" {
                            span class="font-mono text-gray-800 dark:text-gray-200" { (path) }
                            span class="ml-auto text-xs text-gray-400" { (size) " B · " (fmt_ts(*updated_at)) }
                        }
                    }
                }
            }
        }
    };
    Ok(Html(markup.into_string()))
}

#[derive(Deserialize)]
pub struct FileQ {
    pub path: String,
}

async fn origin_file(
    State(s): State<AppState>,
    Path(host): Path<String>,
    Query(q): Query<FileQ>,
) -> AppResult<Html<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT content FROM origin_files WHERE origin = ?1 AND path = ?2",
    )
    .bind(&host)
    .bind(&q.path)
    .fetch_optional(s.db())
    .await?;
    let content = row.map(|r| r.0).unwrap_or_else(|| "(not found)".into());

    let markup = html! {
        div class="mb-3 flex items-center gap-2" {
            button
                class="btn"
                hx-get={ "/ui/origins/" (host) "/files" }
                hx-target="#detail"
                hx-swap="innerHTML"
            { "← files" }
            span class="font-mono text-sm text-gray-700 dark:text-gray-300" { (q.path) }
        }
        pre class="m-0 text-xs whitespace-pre-wrap break-words rounded bg-gray-50 dark:bg-gray-800/60 p-3 overflow-auto" { (content) }
    };
    Ok(Html(markup.into_string()))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…[+{} chars]", &s[..max], s.len() - max)
    }
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}
