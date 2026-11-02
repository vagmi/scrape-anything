//! Function-calling agent.
//!
//! One LLM step per `step()` call: replay the session's turn history as a chat
//! (with the tool catalog attached), send it through the multi-provider client
//! (`crate::llm`, which fails over between providers), and return the model's
//! text and/or tool calls. The *loop* — resolving server-side tools inline and
//! handing extension-side tools back over HTTP — lives in `session.rs`.
//!
//! Tools (see `crate::tools`):
//!   - server-side: the file store / workspace tools, `run_command`, `save_result`.
//!   - extension-side: `execute_script`, `screenshot`, … (run in the live tab).
//!
//! The history is provider-neutral, except that an assistant turn's reasoning
//! and thought signatures are only meaningful to the provider that produced
//! them (DeepSeek wants its own `reasoning_content` back on tool-call turns,
//! Gemini 3 its own signatures, Anthropic its own signed thinking), so they are
//! replayed to that provider and dropped for any other.
//!
//! The agent's persistent memory is the per-origin file store; the first user
//! message lists what's already there so repeat runs on a site reuse prior work.

use genai::chat::{ChatMessage, ChatRequest, ContentPart, MessageContent, ToolCall, ToolResponse};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::tools;

/// One assistant response: free text and/or a batch of tool calls, and which
/// provider/model produced it (a session can fail over mid-run).
#[derive(Debug, Clone)]
pub struct Step {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// The model's reasoning, kept to replay to the same provider.
    pub reasoning: Option<String>,
    pub provider: String,
    pub model: String,
}

/// Drive one LLM step for `session_id`.
pub async fn step(state: &AppState, session_id: &str) -> AppResult<Step> {
    let db = state.db();
    let session = load_session(db, session_id).await?;
    let history = History::load(db, session_id, &session).await?;
    let catalog = tools::catalog(state.sandbox().available());

    tracing::debug!(target: "scrape_backend::agent", session_id = %session_id, "calling llm (function-calling)");

    // Retries transient failures and fails over across providers itself,
    // rendering the history for whichever provider it is trying; an error here
    // means every configured provider failed this step.
    let answer = state
        .inner
        .llm
        .chat(|provider| ChatRequest::from_messages(history.messages_for(provider)).with_tools(catalog.clone()))
        .await
        .map_err(AppError::LlmUnavailable)?;

    let text = answer.response.first_text().map(|s| s.to_string());
    let reasoning = answer.response.reasoning_content.clone().filter(|r| !r.trim().is_empty());
    let tool_calls = answer.response.into_tool_calls();

    tracing::info!(
        target: "scrape_backend::agent",
        session_id = %session_id,
        provider = %answer.provider,
        model = %answer.model,
        tool_calls = tool_calls.len(),
        tools = %tool_calls.iter().map(|c| c.fn_name.as_str()).collect::<Vec<_>>().join(","),
        has_text = text.is_some(),
        "assistant step"
    );

    Ok(Step { text, tool_calls, reasoning, provider: answer.provider, model: answer.model })
}

// ───────────────────────── session/signals loaders ──────────────────────────

pub struct SessionRow {
    pub url: String,
    pub host: String,
    pub intention: String,
}

async fn load_session(db: &SqlitePool, id: &str) -> AppResult<SessionRow> {
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT url, host, intention FROM sessions WHERE id = ?1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    match row {
        None => Err(AppError::SessionNotFound),
        Some((url, host, intention)) => Ok(SessionRow { url, host, intention }),
    }
}

/// The persisted turn log, loaded once per step and rendered into a chat for
/// each provider the step is tried on.
struct History {
    /// The opening user message: URL + intention + signals + memory file list.
    opening: String,
    /// `(role, kind, payload)` in turn order.
    turns: Vec<(String, String, Value)>,
}

impl History {
    async fn load(db: &SqlitePool, session_id: &str, session: &SessionRow) -> AppResult<Self> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT role, kind, payload FROM turns
             WHERE session_id = ?1 ORDER BY turn_index ASC",
        )
        .bind(session_id)
        .fetch_all(db)
        .await?;

        let signals = rows
            .iter()
            .find(|(r, k, _)| r == "extension" && k == "signals")
            .map(|(_, _, p)| p.clone())
            .unwrap_or_else(|| "{}".into());

        let file_list = origin_file_list(db, &session.host).await?;

        let opening = format!(
            "Page URL: {url}\n\nIntention:\n{intent}\n\n\
             Files already in your memory for `{host}`:\n{files}\n\n\
             Page signals (JSON):\n{signals}",
            url = session.url,
            intent = session.intention,
            host = session.host,
            files = file_list,
            signals = truncate(&signals, 80_000),
        );
        let turns = rows
            .into_iter()
            .map(|(role, kind, payload)| {
                let v = serde_json::from_str(&payload).unwrap_or(Value::String(payload));
                (role, kind, v)
            })
            .collect();
        Ok(Self { opening, turns })
    }

    /// The chat for `provider`. Turns replay in order:
    ///   - user/message        → user text
    ///   - user/attachment     → a note on where the uploads are
    ///   - llm/assistant        → assistant text and/or ToolCalls
    ///   - tool/tool_response   → ToolResponse (+ an image user message for screenshots)
    fn messages_for(&self, provider: &str) -> Vec<ChatMessage> {
        let mut out: Vec<ChatMessage> = Vec::new();
        out.push(ChatMessage::system(SYSTEM_PROMPT.to_string()));
        out.push(ChatMessage::user(self.opening.clone()));

        for (role, kind, v) in &self.turns {
            match (role.as_str(), kind.as_str()) {
                ("user", "message") => {
                    let content = match v.get("content").and_then(|c| c.as_str()) {
                        Some(c) => c.to_string(),
                        None => v.as_str().unwrap_or_default().to_string(),
                    };
                    out.push(ChatMessage::user(content));
                }
                ("user", "attachment") => out.push(ChatMessage::user(attachment_note(v))),
                ("llm", "assistant") => {
                    let own = v.get("provider").and_then(|p| p.as_str()) == Some(provider);
                    let mut calls = parse_stored_tool_calls(v);
                    if !calls.is_empty() {
                        // Assistant tool-call turn. (Any incidental text alongside
                        // tool calls is dropped on replay — not needed for context.)
                        let mut parts: Vec<ContentPart> = Vec::new();
                        if own {
                            if let Some(r) = v.get("reasoning").and_then(|r| r.as_str()) {
                                parts.push(ContentPart::ReasoningContent(r.to_string()));
                            }
                        } else {
                            for call in &mut calls {
                                call.thought_signatures = None;
                            }
                        }
                        parts.extend(calls.into_iter().map(ContentPart::ToolCall));
                        out.push(ChatMessage::assistant(MessageContent::from_parts(parts)));
                    } else if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
                        out.push(ChatMessage::assistant(text.to_string()));
                    }
                }
                ("tool", "tool_response") => {
                    let call_id = v.get("call_id").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                    let mut resp = ToolResponse::new(call_id, content);
                    // Gemini names a functionResponse by its function, not its id.
                    if let Some(fn_name) = v.get("fn_name").and_then(|f| f.as_str()).filter(|f| !f.is_empty()) {
                        resp = resp.with_fn_name(fn_name);
                    }
                    out.push(ChatMessage::from(resp));

                    // Screenshots ride in as a following user image message, since a
                    // functionResponse carries only text.
                    if let Some(img) = v.get("image_base64").and_then(|c| c.as_str()) {
                        let mime = v.get("mime").and_then(|c| c.as_str()).unwrap_or("image/png");
                        out.push(ChatMessage::user(MessageContent::from_parts(vec![
                            ContentPart::from_text("Screenshot of the current page:"),
                            ContentPart::from_binary_base64(mime.to_string(), img.to_string(), None),
                        ])));
                    }
                }
                // signals (already in the opening message) and anything else are skipped.
                _ => {}
            }
        }
        out
    }
}

/// Reconstruct `Vec<ToolCall>` from a stored assistant payload's `tool_calls`.
fn parse_stored_tool_calls(v: &Value) -> Vec<ToolCall> {
    v.get("tool_calls")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    let call_id = c.get("call_id")?.as_str()?.to_string();
                    let fn_name = c.get("fn_name")?.as_str()?.to_string();
                    let fn_arguments = c.get("fn_arguments").cloned().unwrap_or(Value::Null);
                    // Gemini 3 requires a call's thought signature back on replay.
                    let thought_signatures = c
                        .get("thought_signatures")
                        .and_then(|s| serde_json::from_value::<Vec<String>>(s.clone()).ok());
                    Some(ToolCall { call_id, fn_name, fn_arguments, thought_signatures })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The note the model sees for a `user/attachment` turn: where each upload is
/// and its preview, so a first parsing script can be written without probing.
fn attachment_note(v: &Value) -> String {
    let files = v.get("files").and_then(|f| f.as_array()).cloned().unwrap_or_default();
    let mut note = String::from(
        "The user attached files. They are in your session workspace — `work/uploads/...` in the file \
         tools, `/work/uploads/...` inside run_command. Analyze them with scripts in the sandbox \
         (see skills/file-analysis.md) rather than reading them whole.\n",
    );
    for f in &files {
        let path = f.get("path").and_then(|p| p.as_str()).unwrap_or("?");
        let size = f.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        let preview = f.get("preview").map(|p| p.to_string()).unwrap_or_default();
        // Previews are already bounded (a few clipped rows) by `workspace::preview`.
        note.push_str(&format!("\n- {path} ({size} bytes)\n  preview: {preview}"));
    }
    note
}

/// Compact listing of the origin's memory files for the opening prompt.
async fn origin_file_list(db: &SqlitePool, host: &str) -> AppResult<String> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT path, length(content) FROM origin_files WHERE origin = ?1 ORDER BY path ASC",
    )
    .bind(host)
    .fetch_all(db)
    .await?;
    if rows.is_empty() {
        return Ok("(none yet — this is the first run on this site)".to_string());
    }
    Ok(rows
        .into_iter()
        .map(|(path, size)| format!("- {path} ({size} bytes)"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…[+{} chars]", &s[..max], s.len() - max) }
}

// ──────────────────────────── system prompt ────────────────────────────────

const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn history() -> History {
        let call = |id: &str, sig: Option<&str>| {
            let mut c = json!({ "call_id": id, "fn_name": "run_command", "fn_arguments": { "command": "ls" } });
            if let Some(sig) = sig {
                c["thought_signatures"] = json!([sig]);
            }
            c
        };
        let turns = vec![
            ("llm", "assistant", json!({ "provider": "gemini", "tool_calls": [call("g1", Some("gsig"))] })),
            ("tool", "tool_response", json!({ "call_id": "g1", "fn_name": "run_command", "content": "{}" })),
            ("llm", "assistant", json!({ "provider": "deepseek", "reasoning": "think", "tool_calls": [call("d1", None)] })),
            ("tool", "tool_response", json!({ "call_id": "d1", "fn_name": "run_command", "content": "{}" })),
        ];
        History {
            opening: "hi".into(),
            turns: turns.into_iter().map(|(r, k, v)| (r.to_string(), k.to_string(), v)).collect(),
        }
    }

    /// Reasoning and signatures go back only to the provider that made them.
    #[test]
    fn replays_reasoning_and_signatures_only_to_their_provider() {
        let h = history();
        let assistant_turns = |provider: &str| -> Vec<MessageContent> {
            h.messages_for(provider)
                .into_iter()
                .filter(|m| !m.content.tool_calls().is_empty())
                .map(|m| m.content)
                .collect()
        };

        let gemini = assistant_turns("gemini");
        assert!(gemini[0].tool_calls()[0].thought_signatures.is_some());
        assert!(gemini[1].reasoning_contents().is_empty());

        let deepseek = assistant_turns("deepseek");
        assert!(deepseek[0].tool_calls()[0].thought_signatures.is_none());
        assert_eq!(deepseek[1].reasoning_contents(), ["think"]);

        let other = assistant_turns("anthropic");
        assert!(other[0].tool_calls()[0].thought_signatures.is_none());
        assert!(other[1].reasoning_contents().is_empty());

        // Tool responses carry their function name (Gemini needs it).
        let resp = h.messages_for("gemini").into_iter().find_map(|m| m.content.tool_responses().first().map(|r| (*r).clone()));
        assert_eq!(resp.unwrap().fn_name.as_deref(), Some("run_command"));
    }
}
