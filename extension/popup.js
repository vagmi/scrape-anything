import { backend } from "./backend.js";
import { runTool } from "./tool-runner.js";

const $ = (id) => document.getElementById(id);
const STORAGE_KEY = "scrape_anything.activeSessionId";
const MAX_TOOL_ITERATIONS = 60; // client guard mirroring the backend max_turns

const state = {
  tabId: null,
  sessionId: null,
  artifactId: null,
  toolEls: new Map(), // call_id -> tool bubble element
  running: false, // an agent loop is driving tools right now
  stopRequested: false, // user hit Stop; loop checks this between steps
};

// ─────────────────────────── UI helpers ─────────────────────────────────────

function setStatus(msg, kind = "") {
  const el = $("status");
  el.textContent = msg || "";
  el.className = kind;
}
function hide(id) { $(id).classList.add("hidden"); }
function show(id) { $(id).classList.remove("hidden"); }

function resetUiForNew() {
  state.sessionId = null;
  state.artifactId = null;
  state.running = false;
  state.stopRequested = false;
  state.toolEls.clear();
  $("transcript").innerHTML = "";
  hide("transcript");
  hide("results");
  hide("stop");
  $("stop").disabled = false;
  $("send").disabled = false;
  $("dl-json").disabled = true;
  $("dl-csv").disabled = true;
  $("composer-label").textContent = "What do you want extracted?";
  $("message").value = "";
  $("send").textContent = "Scrape";
  setStatus("");
}

function setActiveMode() {
  show("transcript");
  $("composer-label").textContent = "Refine, or ask for something else:";
  $("send").textContent = "Send";
}

// ──────────────────────────── storage ───────────────────────────────────────

async function saveActiveSession(id) {
  state.sessionId = id;
  await chrome.storage.local.set({ [STORAGE_KEY]: id });
}
async function loadActiveSession() {
  const obj = await chrome.storage.local.get(STORAGE_KEY);
  return obj[STORAGE_KEY] || null;
}
async function clearActiveSession() {
  state.sessionId = null;
  await chrome.storage.local.remove(STORAGE_KEY);
}

// ─────────────────────── content script glue ────────────────────────────────

async function getActiveTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

// Injected fresh every time: a navigation wipes the content script out of the
// page, and the agent moves between pages inside a single session, so a cached
// "already injected" flag would go stale the moment it navigated.
async function injectSignalsScript(tabId) {
  await chrome.scripting.executeScript({ target: { tabId }, files: ["dom-signals.js"] });
}

async function collectSignals(tabId) {
  await injectSignalsScript(tabId);
  const [{ result } = {}] = await chrome.scripting.executeScript({
    target: { tabId },
    func: () => window.__scrape.signals(),
  });
  if (!result) throw new Error("dom-signals.js returned no result");
  return result;
}

// ────────────────────────── transcript rendering ────────────────────────────

function appendUserTurn(content) {
  const el = document.createElement("div");
  el.className = "turn user";
  el.textContent = content;
  $("transcript").appendChild(el);
  scrollTranscript();
}

function appendAssistantMessage(text) {
  if (!text) return;
  const el = document.createElement("div");
  el.className = "turn assistant";
  el.textContent = text;
  $("transcript").appendChild(el);
  scrollTranscript();
}

/// A tool-call bubble. `call` is { call_id, tool, args }. Returns the existing
/// bubble if this call already has one — a resumed session re-runs calls that
/// were rendered while restoring the transcript.
function appendToolCallBubble(call) {
  const existing = state.toolEls.get(call.call_id);
  if (existing) return existing;
  const el = document.createElement("div");
  el.className = "turn tool";
  el.dataset.callId = call.call_id;

  const head = document.createElement("div");
  head.className = "tool-head";
  head.innerHTML =
    `<span class="tool-name">${toolIcon(call.tool)} ${call.tool}</span>` +
    `<span class="tool-status" data-role="status">running…</span>`;
  el.appendChild(head);

  const detailText = call.tool === "execute_script"
    ? (call.args?.code ?? "")
    : call.tool === "navigate"
      ? (call.args?.url ?? "")
      : JSON.stringify(call.args ?? {}, null, 2);
  if (detailText && detailText !== "{}") {
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent =
      call.tool === "execute_script" ? "code" : call.tool === "navigate" ? "url" : "args";
    const pre = document.createElement("pre");
    pre.textContent = detailText;
    details.appendChild(summary);
    details.appendChild(pre);
    el.appendChild(details);
  }

  $("transcript").appendChild(el);
  state.toolEls.set(call.call_id, el);
  scrollTranscript();
  return el;
}

function setToolStatus(callId, text, isError) {
  const el = state.toolEls.get(callId);
  if (!el) return;
  const s = el.querySelector('[data-role="status"]');
  if (!s) return;
  s.textContent = text;
  s.classList.toggle("error", !!isError);
}

function toolIcon(tool) {
  if (tool === "screenshot") return "📷";
  if (tool === "execute_script") return "⚙️";
  if (tool === "navigate") return "🧭";
  if (tool === "page_to_markdown") return "📄";
  return "🔧";
}

function scrollTranscript() {
  const t = $("transcript");
  t.scrollTop = t.scrollHeight;
}

// ─────────────────────────── the agent tool loop ────────────────────────────

/// Given a turn from the backend, run any tools it asks for and post the
/// results back, looping until the agent returns a terminal message.
/// Checks `state.stopRequested` between steps so the Stop button breaks out
/// promptly; the backend session is marked cancelled separately.
async function runToolLoop(turn) {
  let current = turn;
  let guard = 0;
  while (current && current.kind === "tool_call" && !state.stopRequested) {
    if (++guard > MAX_TOOL_ITERATIONS) {
      setStatus("Too many tool steps; stopping.", "error");
      return;
    }
    current = await runCallsAndContinue(current.tool_calls);
  }

  if (state.stopRequested) {
    await refreshArtifact();
    return;
  }
  if (current && current.kind === "message") {
    appendAssistantMessage(current.text);
  }
  await refreshArtifact();
  setStatus("Done.", "ok");
}

/// Run one batch of extension-side tool calls, post the results, and return the
/// backend's next turn. Tools can navigate the tab; `state.tabId` stays fixed
/// and the session on the backend carries straight on into the new page.
async function runCallsAndContinue(calls) {
  // If the popup was restored without a tab (e.g. after a restart), follow the
  // user's current tab rather than failing on a stale/null id.
  if (!state.tabId) {
    const tab = await getActiveTab().catch(() => null);
    if (tab?.id) state.tabId = tab.id;
  }
  if (!state.tabId) throw new Error("No active tab — open a web page first.");
  const results = [];
  for (const call of calls) {
    if (state.stopRequested) break;
    appendToolCallBubble(call);
    setStatus(`Running ${call.tool}…`);
    const r = await runTool(state.tabId, call, (m) => {
      if (!state.stopRequested) setStatus(`${call.tool}: ${m}`);
    });
    setToolStatus(call.call_id, describeResult(call.tool, r), !!r.error);
    results.push(r);
  }
  if (state.stopRequested) {
    // Don't post partial results — that would advance the agent loop. Just
    // tell the backend to stand down (best-effort; an in-flight request may
    // re-mark the session active, so cancel lands last and wins).
    setStatus("Stopping…");
    try { await backend.cancelSession(state.sessionId); } catch {}
    await refreshArtifact();
    setStatus("Stopped.", "");
    return { kind: "message", text: "Stopped.", turn_index: -1 };
  }
  setStatus("Thinking… (agent step running)");
  const { turn: next } = await backend.sendToolResult(state.sessionId, results);
  if (state.stopRequested) {
    try { await backend.cancelSession(state.sessionId); } catch {}
    await refreshArtifact();
    setStatus("Stopped.", "");
    return { kind: "message", text: "Stopped.", turn_index: -1 };
  }
  return next;
}

function describeResult(tool, r) {
  if (r.error) return `error: ${r.error}`;
  if (r.result?.navigated) return `page navigated → ${shortUrl(r.result.to)}`;
  if (tool === "navigate") return `→ ${shortUrl(r.result?.url)}`;
  if (tool === "screenshot") return "captured";
  if (tool === "execute_script") {
    const items = r.result?.items;
    if (Array.isArray(items)) {
      const n = `${items.length} item${items.length === 1 ? "" : "s"}`;
      return r.result.append ? `+${n}` : n;
    }
    return "ran";
  }
  return "done";
}

// ────────────────────────── results / downloads ─────────────────────────────

async function refreshArtifact() {
  try {
    const a = await backend.fetchArtifact(state.sessionId);
    state.artifactId = a.artifact_id;
    const noun = a.kind === "event" ? "event" : "item";
    $("results-count").textContent =
      `${a.item_count} ${noun}${a.item_count === 1 ? "" : "s"} extracted`;
    $("dl-json").disabled = !a.item_count;
    $("dl-csv").disabled = !a.item_count;
    show("results");
  } catch {
    // No artifact for this session yet — leave the results area hidden.
  }
}

function downloadArtifact(format) {
  if (!state.artifactId) { setStatus("No result to download yet.", "error"); return; }
  chrome.downloads.download({ url: backend.artifactUrl(state.artifactId, format), saveAs: true });
}

// ─────────────────────────── handlers ──────────────────────────────────────

async function onSend() {
  if (state.running) return;
  const text = $("message").value.trim();
  if (!text) { setStatus("Type something first.", "error"); return; }
  const tab = await getActiveTab();
  if (!tab?.url || !/^https?:\/\//.test(tab.url)) {
    setStatus("Open a web page first.", "error"); return;
  }
  state.tabId = tab.id;

  $("send").disabled = true;
  state.running = true;
  state.stopRequested = false;
  $("stop").disabled = false;
  show("stop");
  $("message").value = "";

  try {
    let turn;
    if (!state.sessionId) {
      setStatus("Collecting page signals…");
      const signals = await collectSignals(state.tabId);
      setStatus("Starting the agent…");
      const res = await backend.createSession({ url: tab.url, intention: text, signals });
      await saveActiveSession(res.session_id);
      setActiveMode();
      appendUserTurn(text);
      turn = res.turn;
    } else {
      appendUserTurn(text);
      setStatus("Thinking…");
      const res = await backend.sendMessage(state.sessionId, text);
      turn = res.turn;
    }
    await runToolLoop(turn);
    if (!state.stopRequested && state.sessionId) {
      // runToolLoop reports Done itself; nothing to do here.
    }
  } catch (e) {
    if (!state.stopRequested) setStatus(e.message || String(e), "error");
  } finally {
    state.running = false;
    hide("stop");
    $("send").disabled = false;
  }
}

/// Stop button: signal the loop to stand down between steps and mark the
/// backend session cancelled so reopening the popup doesn't auto-resume it.
/// Sending another message afterwards revives the session.
async function onStop() {
  if (!state.sessionId) return;
  state.stopRequested = true;
  $("stop").disabled = true;
  setStatus("Stopping…");
  try { await backend.cancelSession(state.sessionId); } catch (e) {
    setStatus(`Stop failed (${e.message || e}) — closing the popup also halts the run.`, "error");
  }
}

async function onNewSession() {
  // Stand down the current run first so its loop can't keep posting results.
  state.stopRequested = true;
  if (state.sessionId) {
    try { await backend.cancelSession(state.sessionId); } catch {}
  }
  state.running = false;
  await clearActiveSession();
  resetUiForNew();
  await detectPage();
  setStatus("Started a new session.", "ok");
}

// ───────────────────────── restore on open ──────────────────────────────────

async function restoreSession(sessionId) {
  let info;
  try {
    info = await backend.fetchSession(sessionId);
  } catch {
    await clearActiveSession();
    setStatus("Previous session not found; starting fresh.", "");
    return;
  }
  state.sessionId = sessionId;
  setActiveMode();
  $("page-kind").textContent = `Session on ${shortUrl(info.url)}`;

  // The opening intention lives on sessions.intention, not as a turn.
  if (info.intention) appendUserTurn(info.intention);

  // Tool calls the backend handed out but never got an answer for. The popup
  // closes whenever it loses focus, which can happen mid-step; without this the
  // session would sit waiting forever on a call nobody is going to run.
  const unanswered = new Map();

  for (const t of info.turns) {
    if (t.role === "extension") continue; // signals / result telemetry
    if (t.role === "user" && t.kind === "message") {
      appendUserTurn(t.payload?.content || "");
    } else if (t.role === "llm" && t.kind === "assistant") {
      const p = t.payload || {};
      const calls = p.tool_calls || [];
      if (calls.length) {
        for (const c of calls) {
          const call = { call_id: c.call_id, tool: c.fn_name, args: c.fn_arguments };
          appendToolCallBubble(call);
          unanswered.set(c.call_id, call);
        }
      } else if (p.text) {
        appendAssistantMessage(p.text);
      }
    } else if (t.role === "tool" && t.kind === "tool_response") {
      const p = t.payload || {};
      const { text, err } = restoreToolSummary(p);
      setToolStatus(p.call_id, text, err);
      unanswered.delete(p.call_id);
    }
  }
  scrollTranscript();
  await refreshArtifact();

  const pending = [...unanswered.values()];
  if (info.status === "cancelled") {
    setStatus("Stopped.", "");
  } else if (info.status === "active" && pending.length) {
    await resumePendingCalls(pending);
  } else if (info.status === "active" && !pending.length) {
    // Stuck case: the popup died during POST /tool_result. The backend already
    // stored our tool_response and advanced (or was cancelled mid-LLM), so
    // there is no unanswered ToolCall to re-run — but the session is still
    // active with no driver. Kicking with an empty result re-runs advance()
    // and hands us the next turn instead of sitting frozen on "Restored".
    await resumeStuckSession();
  } else {
    setStatus("Restored session.", "");
  }
}

/// Finish a step that was interrupted: re-run the tools the backend is still
/// waiting on against the tab as it is now, then carry the loop on. The page may
/// well have moved since — that's fine, the tools report where they landed and
/// the agent re-orients from there.
async function resumePendingCalls(calls) {
  const tab = await getActiveTab();
  if (!tab?.url || !/^https?:\/\//.test(tab.url)) {
    setStatus("Session paused mid-step — open a web page to resume it.", "error");
    return;
  }
  state.tabId = tab.id;
  $("send").disabled = true;
  state.running = true;
  state.stopRequested = false;
  $("stop").disabled = false;
  show("stop");
  try {
    setStatus("Resuming the interrupted step…");
    await runToolLoop(await runCallsAndContinue(calls));
  } catch (e) {
    if (!state.stopRequested) setStatus(e.message || String(e), "error");
  } finally {
    state.running = false;
    hide("stop");
    $("send").disabled = false;
  }
}

/// Recover an `active` session with no unanswered calls (popup died during the
/// POST, so the backend consumed our results but we never got the next turn).
/// An empty `results: []` just re-runs `advance()` server-side — no duplicate
/// tool execution — and returns whatever the agent wants next.
async function resumeStuckSession() {
  const tab = await getActiveTab().catch(() => null);
  if (tab?.id) state.tabId = tab.id;
  if (!tab?.url || !/^https?:\/\//.test(tab.url)) {
    setStatus("Session is active but paused — open a web page and hit Send to resume.", "error");
    return;
  }
  $("send").disabled = true;
  state.running = true;
  state.stopRequested = false;
  $("stop").disabled = false;
  show("stop");
  try {
    setStatus("Resuming interrupted session…");
    const { turn } = await backend.sendToolResult(state.sessionId, []);
    await runToolLoop(turn);
  } catch (e) {
    if (!state.stopRequested) setStatus(`Resume failed (${e.message || e}) — hit Send to retry.`, "error");
  } finally {
    state.running = false;
    hide("stop");
    $("send").disabled = false;
  }
}

function restoreToolSummary(p) {
  if (p.image_base64) return { text: "captured", err: false };
  let v;
  try { v = JSON.parse(p.content); } catch { v = null; }
  if (v && v.error !== undefined) {
    const msg = typeof v.error === "string" ? v.error : JSON.stringify(v.error);
    return { text: `error: ${msg}`, err: true };
  }
  if (v && Array.isArray(v.items)) return { text: `${v.items.length} items`, err: false };
  return { text: "ran", err: false };
}

function shortUrl(url) {
  if (!url) return "";
  try { const u = new URL(url); return u.host + u.pathname; } catch { return url; }
}

async function detectPage() {
  const tab = await getActiveTab();
  if (!tab?.url) { $("page-kind").textContent = "(no active tab)"; return; }
  $("page-kind").textContent = `Ready on ${shortUrl(tab.url)}`;
}

document.addEventListener("DOMContentLoaded", async () => {
  $("send").addEventListener("click", onSend);
  $("stop").addEventListener("click", onStop);
  $("new-session").addEventListener("click", onNewSession);
  $("dl-json").addEventListener("click", () => downloadArtifact("json"));
  $("dl-csv").addEventListener("click", () => downloadArtifact("csv"));
  $("message").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onSend(); }
  });

  await detectPage();
  const existing = await loadActiveSession();
  if (existing) await restoreSession(existing);
});
