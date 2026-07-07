import { backend } from "./backend.js";
import { runTool } from "./tool-runner.js";

const $ = (id) => document.getElementById(id);
const STORAGE_KEY = "scrape_anything.activeSessionId";
const MAX_TOOL_ITERATIONS = 60; // client guard mirroring the backend max_turns

const state = {
  tabId: null,
  sessionId: null,
  contentScriptsInjected: false,
  artifactId: null,
  toolEls: new Map(), // call_id -> tool bubble element
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
  state.toolEls.clear();
  $("transcript").innerHTML = "";
  hide("transcript");
  hide("results");
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

async function ensureSignalsScript(tabId) {
  if (state.contentScriptsInjected) return;
  await chrome.scripting.executeScript({ target: { tabId }, files: ["dom-signals.js"] });
  state.contentScriptsInjected = true;
}

async function collectSignals(tabId) {
  await ensureSignalsScript(tabId);
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

/// A tool-call bubble. `call` is { call_id, tool, args }.
function appendToolCallBubble(call) {
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
    : JSON.stringify(call.args ?? {}, null, 2);
  if (detailText && detailText !== "{}") {
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = call.tool === "execute_script" ? "code" : "args";
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
  return "🔧";
}

function scrollTranscript() {
  const t = $("transcript");
  t.scrollTop = t.scrollHeight;
}

// ─────────────────────────── the agent tool loop ────────────────────────────

/// Given a turn from the backend, run any tools it asks for and post the
/// results back, looping until the agent returns a terminal message.
async function runToolLoop(turn) {
  let current = turn;
  let guard = 0;
  while (current && current.kind === "tool_call") {
    if (++guard > MAX_TOOL_ITERATIONS) {
      setStatus("Too many tool steps; stopping.", "error");
      return;
    }
    const results = [];
    for (const call of current.tool_calls) {
      appendToolCallBubble(call);
      setStatus(`Running ${call.tool}…`);
      const r = await runTool(state.tabId, call);
      setToolStatus(call.call_id, describeResult(call.tool, r), !!r.error);
      results.push(r);
    }
    const { turn: next } = await backend.sendToolResult(state.sessionId, results);
    current = next;
  }

  if (current && current.kind === "message") {
    appendAssistantMessage(current.text);
  }
  await refreshArtifact();
  setStatus("Done.", "ok");
}

function describeResult(tool, r) {
  if (r.error) return `error: ${r.error}`;
  if (tool === "screenshot") return "captured";
  if (tool === "execute_script") {
    const items = r.result?.items;
    if (Array.isArray(items)) return `${items.length} item${items.length === 1 ? "" : "s"}`;
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
  const text = $("message").value.trim();
  if (!text) { setStatus("Type something first.", "error"); return; }
  const tab = await getActiveTab();
  if (!tab?.url || !/^https?:\/\//.test(tab.url)) {
    setStatus("Open a web page first.", "error"); return;
  }
  state.tabId = tab.id;

  $("send").disabled = true;
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
  } catch (e) {
    setStatus(e.message || String(e), "error");
  } finally {
    $("send").disabled = false;
  }
}

async function onNewSession() {
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

  for (const t of info.turns) {
    if (t.role === "extension") continue; // signals / result telemetry
    if (t.role === "user" && t.kind === "message") {
      appendUserTurn(t.payload?.content || "");
    } else if (t.role === "llm" && t.kind === "assistant") {
      const p = t.payload || {};
      const calls = p.tool_calls || [];
      if (calls.length) {
        for (const c of calls) {
          appendToolCallBubble({ call_id: c.call_id, tool: c.fn_name, args: c.fn_arguments });
        }
      } else if (p.text) {
        appendAssistantMessage(p.text);
      }
    } else if (t.role === "tool" && t.kind === "tool_response") {
      const p = t.payload || {};
      const { text, err } = restoreToolSummary(p);
      setToolStatus(p.call_id, text, err);
    }
  }
  scrollTranscript();
  await refreshArtifact();
  setStatus(`Restored session.`, "");
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
  try { const u = new URL(url); return u.host + u.pathname; } catch { return url; }
}

async function detectPage() {
  const tab = await getActiveTab();
  if (!tab?.url) { $("page-kind").textContent = "(no active tab)"; return; }
  $("page-kind").textContent = `Ready on ${shortUrl(tab.url)}`;
}

document.addEventListener("DOMContentLoaded", async () => {
  $("send").addEventListener("click", onSend);
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
