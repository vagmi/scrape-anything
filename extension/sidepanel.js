import { backend } from "./backend.js";
import { runTool } from "./tool-runner.js";
import { detach as detachDebugger } from "./page-debugger.js";

const $ = (id) => document.getElementById(id);
const STORAGE_KEY = "scrape_anything.activeSessionId";
const MAX_TOOL_ITERATIONS = 100; // client guard mirroring the backend max_turns (per request)
// Waits before re-trying an agent step when the model is down even after the
// backend's own quick retries (~1 min of waiting all told).
const LLM_OUTAGE_RETRY_DELAYS_MS = [5000, 10000, 20000, 30000];

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
  show("empty");
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
  hide("empty");
  show("transcript");
  $("composer-label").textContent = "Refine, or ask for something else:";
  $("send").textContent = "Send";
}

// ──────────────────────────── storage ───────────────────────────────────────

// The session is stored together with the tab it drives. The panel spans the
// whole window, so the user can wander off to other tabs mid-run; the tools
// must keep acting on the session's tab, not whichever one is in front.
async function saveActiveSession(id, tabId) {
  state.sessionId = id;
  state.tabId = tabId;
  await chrome.storage.local.set({ [STORAGE_KEY]: { sessionId: id, tabId } });
}
async function loadActiveSession() {
  const obj = await chrome.storage.local.get(STORAGE_KEY);
  const v = obj[STORAGE_KEY];
  if (!v) return null;
  // Pre-side-panel builds stored the bare session id.
  if (typeof v === "string") return { sessionId: v, tabId: null };
  return v;
}
async function clearActiveSession() {
  state.sessionId = null;
  state.tabId = null;
  await chrome.storage.local.remove(STORAGE_KEY);
}

// ─────────────────────── content script glue ────────────────────────────────

async function getActiveTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

async function getTab(tabId) {
  if (!tabId) return null;
  try { return await chrome.tabs.get(tabId); } catch { return null; }
}

/// The tab this session's tools run against. Keeps the stored binding while
/// that tab exists; if it was closed (or never recorded), adopts the tab the
/// user is looking at now, as long as it's a web page. Returns null otherwise.
async function resolveSessionTab() {
  let tab = await getTab(state.tabId);
  if (!tab) {
    const active = await getActiveTab().catch(() => null);
    if (!active?.url || !/^https?:\/\//.test(active.url)) return null;
    tab = active;
    if (state.sessionId) await saveActiveSession(state.sessionId, tab.id);
  }
  state.tabId = tab.id;
  return tab;
}

// ───────────────────────────── header ───────────────────────────────────────

/// Keep the header in step with the tabs: which page the session is on, and a
/// warning when the user is looking at a different tab. Called from tab events,
/// so it must be cheap and never throw.
async function renderHeader() {
  try {
    const active = await getActiveTab().catch(() => null);
    if (!state.sessionId) {
      setPageContext(active, active?.url ? `Ready on ${shortUrl(active.url)}` : "No active tab");
      hide("session-link");
      hide("tab-warning");
      return;
    }
    $("session-link").href = backend.sessionPageUrl(state.sessionId);
    show("session-link");
    const bound = await getTab(state.tabId);
    setPageContext(
      bound,
      bound ? `Session on ${shortUrl(bound.url)}` : "Session tab was closed — sending continues on this tab",
    );
    if (bound && active && bound.id !== active.id) show("tab-warning");
    else hide("tab-warning");
  } catch {}
}

function setPageContext(tab, text) {
  $("page-kind").textContent = text;
  $("page-kind").title = tab?.url || "";
  const icon = $("page-favicon");
  if (tab?.favIconUrl && /^https?:|^data:/.test(tab.favIconUrl)) {
    icon.src = tab.favIconUrl;
    show("page-favicon");
  } else {
    hide("page-favicon");
  }
}

async function showSessionTab() {
  const tab = await getTab(state.tabId);
  if (!tab) return;
  await chrome.tabs.update(tab.id, { active: true });
  await chrome.windows.update(tab.windowId, { focused: true });
}

// ───────────────────────────── driving ──────────────────────────────────────

/// Run `fn` as the one driver of this session's agent loop. Two windows can
/// each have the panel open on the same stored session; a Web Lock (shared by
/// all extension pages) makes sure only one of them runs tools and posts
/// results. The lock drops automatically when the panel closes. Handles the
/// Send/Stop button state around the run.
async function drive(fn) {
  $("send").disabled = true;
  state.running = true;
  state.stopRequested = false;
  document.body.dataset.running = "";
  $("stop").disabled = false;
  show("stop");
  try {
    const run = async () => {
      try {
        await fn();
      } catch (e) {
        if (!state.stopRequested) setStatus(e.message || String(e), "error");
      }
    };
    if (!state.sessionId) return await run(); // brand-new session: nothing to contend with
    await navigator.locks.request(`scrape-session:${state.sessionId}`, { ifAvailable: true }, async (lock) => {
      if (!lock) {
        setStatus("This session is being run from another window's panel.", "error");
        return;
      }
      await run();
    });
  } finally {
    state.running = false;
    delete document.body.dataset.running;
    hide("stop");
    $("send").disabled = false;
  }
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
  el.dataset.state = "running";

  const head = document.createElement("div");
  head.className = "tool-head";
  head.innerHTML =
    `<span class="tool-name">${toolIcon(call.tool)}<span></span></span>` +
    `<span class="tool-status" data-role="status"><span>running…</span></span>`;
  head.querySelector(".tool-name > span").textContent = call.tool;
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
  const s = el.querySelector('[data-role="status"] > span');
  if (!s) return;
  s.textContent = text;
  s.title = text;
  el.dataset.state = isError ? "error" : "ok";
}

// Lucide line icons, styled by `.icon` in sidepanel.css.
const TOOL_ICONS = {
  screenshot:
    '<path d="M14.5 4h-5L7 7H4a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-3l-2.5-3z"/><circle cx="12" cy="13" r="3"/>',
  execute_script: '<path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/>',
  navigate:
    '<circle cx="12" cy="12" r="10"/><path d="m16.24 7.76-2.12 6.36-6.36 2.12 2.12-6.36 6.36-2.12z"/>',
  page_to_markdown:
    '<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><path d="M16 13H8"/><path d="M16 17H8"/>',
};
const DEFAULT_TOOL_ICON =
  '<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>';

function toolIcon(tool) {
  return `<svg class="icon" viewBox="0 0 24 24">${TOOL_ICONS[tool] || DEFAULT_TOOL_ICON}</svg>`;
}

function scrollTranscript() {
  const t = $("scroll");
  t.scrollTop = t.scrollHeight;
}

// ─────────────────────────── the agent tool loop ────────────────────────────

/// The backend answers 502 LLM_UNAVAILABLE / 504 LLM_TIMEOUT when Gemini kept
/// failing through its own retries (a 502/503 from Google, an empty response).
function isLlmOutage(e) {
  return e?.code === "LLM_UNAVAILABLE" || e?.code === "LLM_TIMEOUT" || [502, 503, 504].includes(e?.status);
}

/// Run a backend call that advances the agent (`sendMessage`/`sendToolResult`),
/// riding out a model outage. By the time the backend reports one, our tool
/// results / user message are already stored, so recovery is just an empty
/// `tool_result` post: it re-runs `advance()` without re-running tools or
/// duplicating the message. Resolves `{ turn: null }` if the user hits Stop
/// while we wait.
async function advanceWithRecovery(request) {
  try {
    return await request();
  } catch (e) {
    if (!isLlmOutage(e) || !state.sessionId) throw e;
    let last = e;
    for (let i = 0; i < LLM_OUTAGE_RETRY_DELAYS_MS.length; i++) {
      for (let left = LLM_OUTAGE_RETRY_DELAYS_MS[i] / 1000; left > 0; left--) {
        if (state.stopRequested) return { turn: null };
        setStatus(`Model unavailable — retrying in ${left}s (attempt ${i + 1}/${LLM_OUTAGE_RETRY_DELAYS_MS.length})…`, "error");
        await new Promise((r) => setTimeout(r, 1000));
      }
      if (state.stopRequested) return { turn: null };
      setStatus("Thinking… (retrying agent step)");
      try {
        return await backend.sendToolResult(state.sessionId, []);
      } catch (e2) {
        if (!isLlmOutage(e2)) throw e2;
        last = e2;
      }
    }
    throw new Error(`Model still unavailable (${last.message}) — hit Send or reopen the panel to resume.`);
  }
}

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
  // The session's tab may have been closed mid-run; carry on in the user's
  // current tab rather than failing on a dead id.
  const tab = await resolveSessionTab();
  if (!tab) throw new Error("The session's tab is gone — open a web page and hit Send to continue.");
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
  detachDebugger(state.tabId);
    try { await backend.cancelSession(state.sessionId); } catch {}
    await refreshArtifact();
    setStatus("Stopped.", "");
    return { kind: "message", text: "Stopped.", turn_index: -1 };
  }
  setStatus("Thinking… (agent step running)");
  const { turn: next } = await advanceWithRecovery(() => backend.sendToolResult(state.sessionId, results));
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
    scrollTranscript(); // the results bar just took height from the transcript
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

  // A new session starts on the tab in front of the user; a follow-up keeps
  // going on the session's own tab, wherever the user happens to be looking.
  const tab = state.sessionId ? await resolveSessionTab() : await getActiveTab();
  if (!tab?.url || !/^https?:\/\//.test(tab.url)) {
    setStatus("Open a web page first.", "error"); return;
  }
  state.tabId = tab.id;
  $("message").value = "";

  await drive(async () => {
    let turn;
    if (!state.sessionId) {
      setStatus("Collecting page signals…");
      const signals = await collectSignals(state.tabId);
      setStatus("Starting the agent…");
      const res = await backend.createSession({ url: tab.url, intention: text, signals });
      await saveActiveSession(res.session_id, tab.id);
      setActiveMode();
      renderHeader();
      appendUserTurn(text);
      turn = res.turn;
    } else {
      appendUserTurn(text);
      setStatus("Thinking…");
      const res = await advanceWithRecovery(() => backend.sendMessage(state.sessionId, text));
      turn = res.turn;
    }
    await runToolLoop(turn);
  });
}

/// Stop button: signal the loop to stand down between steps and mark the
/// backend session cancelled so reopening the panel doesn't auto-resume it.
/// Sending another message afterwards revives the session.
async function onStop() {
  if (!state.sessionId) return;
  state.stopRequested = true;
  $("stop").disabled = true;
  setStatus("Stopping…");
  detachDebugger(state.tabId);
  try { await backend.cancelSession(state.sessionId); } catch (e) {
    setStatus(`Stop failed (${e.message || e}) — closing the panel also halts the run.`, "error");
  }
}

async function onNewSession() {
  // Stand down the current run first so its loop can't keep posting results.
  state.stopRequested = true;
  if (state.sessionId) {
    try { await backend.cancelSession(state.sessionId); } catch {}
  }
  state.running = false;
  await detachDebugger(state.tabId);
  await clearActiveSession();
  resetUiForNew();
  await renderHeader();
  setStatus("Started a new session.", "ok");
}

// ───────────────────────── restore on open ──────────────────────────────────

async function restoreSession({ sessionId, tabId }) {
  let info;
  try {
    info = await backend.fetchSession(sessionId);
  } catch {
    await clearActiveSession();
    setStatus("Previous session not found; starting fresh.", "");
    return;
  }
  state.sessionId = sessionId;
  state.tabId = tabId;
  setActiveMode();
  await renderHeader();

  // The opening intention lives on sessions.intention, not as a turn.
  if (info.intention) appendUserTurn(info.intention);

  // Tool calls the backend handed out but never got an answer for. Closing the
  // panel (or the browser) mid-step leaves these behind; without this the
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
    // Stuck case: the panel died during POST /tool_result. The backend already
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
/// waiting on against the session's tab as it is now, then carry the loop on.
/// The page may well have moved since — that's fine, the tools report where
/// they landed and the agent re-orients from there.
async function resumePendingCalls(calls) {
  if (!(await resolveSessionTab())) {
    setStatus("Session paused mid-step — open a web page to resume it.", "error");
    return;
  }
  await drive(async () => {
    setStatus("Resuming the interrupted step…");
    await runToolLoop(await runCallsAndContinue(calls));
  });
}

/// Recover an `active` session with no unanswered calls (panel died during the
/// POST, so the backend consumed our results but we never got the next turn).
/// An empty `results: []` just re-runs `advance()` server-side — no duplicate
/// tool execution — and returns whatever the agent wants next.
async function resumeStuckSession() {
  if (!(await resolveSessionTab())) {
    setStatus("Session is active but paused — open a web page and hit Send to resume.", "error");
    return;
  }
  await drive(async () => {
    setStatus("Resuming interrupted session…");
    try {
      const { turn } = await advanceWithRecovery(() => backend.sendToolResult(state.sessionId, []));
      await runToolLoop(turn);
    } catch (e) {
      throw new Error(`Resume failed (${e.message || e}) — hit Send to retry.`);
    }
  });
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

document.addEventListener("DOMContentLoaded", async () => {
  $("send").addEventListener("click", onSend);
  $("stop").addEventListener("click", onStop);
  $("new-session").addEventListener("click", onNewSession);
  $("show-session-tab").addEventListener("click", showSessionTab);
  $("dl-json").addEventListener("click", () => downloadArtifact("json"));
  $("dl-csv").addEventListener("click", () => downloadArtifact("csv"));
  $("message").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onSend(); }
  });

  // The panel outlives tab switches and navigations, so keep the header live.
  // onActivated fires for every window; renderHeader re-reads this window's
  // active tab, so events from other windows are harmless.
  chrome.tabs.onActivated.addListener(() => renderHeader());
  chrome.tabs.onUpdated.addListener((_id, info) => {
    if (info.url || info.status === "complete") renderHeader();
  });
  chrome.tabs.onRemoved.addListener(() => renderHeader());

  await renderHeader();
  const existing = await loadActiveSession();
  if (existing) await restoreSession(existing);
});
