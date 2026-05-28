import { backend } from "./backend.js";

const $ = (id) => document.getElementById(id);
const STORAGE_KEY = "scrape_anything.activeSessionId";

const state = {
  tabId: null,
  sessionId: null,
  contentScriptsInjected: false,
  // Per-turn live state, keyed by turn_index. Lets us re-render
  // download buttons and run-again actions without rebuilding the DOM.
  lastResults: new Map(),  // turn_index -> { kind, items }
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
  state.lastResults.clear();
  $("transcript").innerHTML = "";
  hide("transcript");
  $("composer-label").textContent = "What do you want extracted?";
  $("message").value = "";
  $("send").textContent = "Plan & run";
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
  await chrome.scripting.executeScript({
    target: { tabId }, files: ["dom-signals.js"],
  });
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

async function runCode(tabId, code) {
  if (!chrome.userScripts) {
    throw new Error(
      "chrome.userScripts unavailable. Enable Developer Mode in " +
      "chrome://extensions and toggle \"Allow User Scripts\" for this extension."
    );
  }
  const wrapped = `(async function(){\n${code}\n})()`;
  let results;
  try {
    results = await chrome.userScripts.execute({
      target: { tabId },
      js: [{ code: wrapped }],
      injectImmediately: true,
    });
  } catch (e) {
    const msg = e?.message || String(e);
    if (/not allowed|developer mode|user script/i.test(msg)) {
      throw new Error(
        "User scripts disabled. Toggle \"Allow User Scripts\" on this extension."
      );
    }
    throw new Error(`userScripts.execute failed: ${msg}`);
  }
  const first = results && results[0];
  if (!first) throw new Error("no frames executed");
  if (first.error) throw new Error(`code error: ${first.error.message || first.error}`);
  const out = first.result;
  if (!out || typeof out !== "object") {
    throw new Error("code did not return an object with { kind, items }");
  }
  return {
    kind: out.kind === "event" ? "event" : "listing",
    items: Array.isArray(out.items) ? out.items : [],
  };
}

// ────────────────────────── transcript rendering ────────────────────────────

function appendUserTurn(content) {
  const el = document.createElement("div");
  el.className = "turn user";
  el.textContent = content;
  $("transcript").appendChild(el);
  scrollTranscript();
}

function appendAssistantTurn(turn) {
  const el = document.createElement("div");
  el.className = "turn assistant";
  el.dataset.turnIndex = String(turn.turn_index);

  if (turn.kind === "no_code") {
    const reason = document.createElement("p");
    reason.className = "rationale";
    reason.textContent = turn.reason;
    el.appendChild(reason);
    $("transcript").appendChild(el);
    scrollTranscript();
    return el;
  }

  if (turn.rationale) {
    const r = document.createElement("p");
    r.className = "rationale";
    r.textContent = turn.rationale;
    el.appendChild(r);
  }
  const details = document.createElement("details");
  const summary = document.createElement("summary");
  summary.textContent = "code";
  const pre = document.createElement("pre");
  pre.textContent = turn.code;
  details.appendChild(summary);
  details.appendChild(pre);
  el.appendChild(details);

  // Result row populated after runCode completes.
  const resultRow = document.createElement("div");
  resultRow.className = "result-line";
  resultRow.dataset.role = "result";
  resultRow.innerHTML =
    '<strong>—</strong>' +
    '<span class="actions">' +
      '<button data-action="run">Run</button>' +
      '<button data-action="download-json" disabled>JSON</button>' +
      '<button data-action="download-csv"  disabled>CSV</button>' +
    '</span>';
  el.appendChild(resultRow);

  $("transcript").appendChild(el);
  scrollTranscript();
  return el;
}

function updateResultRow(turnEl, out) {
  const row = turnEl.querySelector('[data-role="result"]');
  if (!row) return;
  row.querySelector("strong").textContent =
    `${out.items.length} ${out.kind === "event" ? "event" : "item"}${out.items.length === 1 ? "" : "s"}`;
  row.querySelector('[data-action="download-json"]').disabled = !out.items.length;
  row.querySelector('[data-action="download-csv"]').disabled  = !out.items.length;
}

function scrollTranscript() {
  const t = $("transcript");
  t.scrollTop = t.scrollHeight;
}

// ────────────────────────────── downloads ──────────────────────────────────

function toCsv(items) {
  if (!items.length) return "";
  const keys = Array.from(items.reduce((set, it) => {
    Object.keys(it).forEach((k) => set.add(k)); return set;
  }, new Set()));
  const esc = (v) => {
    if (v == null) return "";
    const s = typeof v === "string" ? v : JSON.stringify(v);
    return /[",\n\r]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  return [keys.join(",")]
    .concat(items.map((it) => keys.map((k) => esc(it[k])).join(",")))
    .join("\n");
}

function ts() { return new Date().toISOString().replace(/[:.]/g, "-"); }

function download(content, filename, mime) {
  const blob = new Blob([content], { type: mime });
  const url = URL.createObjectURL(blob);
  chrome.downloads.download({ url, filename, saveAs: true }, () => {
    setTimeout(() => URL.revokeObjectURL(url), 5000);
  });
}

// ───────────────────────── core code-execution path ─────────────────────────

/// Given a freshly rendered assistant turn, run its code in the active tab,
/// update the result row, store the items, and POST telemetry.
async function executeTurn(turnEl, turn) {
  setStatus("Running code…");
  try {
    const out = await runCode(state.tabId, turn.code);
    state.lastResults.set(turn.turn_index, out);
    updateResultRow(turnEl, out);
    await backend.recordResult(state.sessionId, {
      items_count: out.items.length,
      errors: [],
      sample_items: out.items.slice(0, 3),
    });
    setStatus(`${out.items.length} item(s).`, "ok");
  } catch (e) {
    setStatus(e.message || String(e), "error");
    await backend.recordResult(state.sessionId, {
      items_count: 0,
      errors: [String(e.message || e)],
    });
  }
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
    if (!state.sessionId) {
      // First message: create the session with collected signals.
      setStatus("Collecting page signals…");
      const signals = await collectSignals(state.tabId);
      setStatus("Asking the planner for code…");
      const { session_id, turn } = await backend.createSession({
        url: tab.url, intention: text, signals,
      });
      await saveActiveSession(session_id);
      setActiveMode();
      appendUserTurn(text);
      const turnEl = appendAssistantTurn(turn);
      if (turn.kind === "code") await executeTurn(turnEl, turn);
    } else {
      // Follow-up message: refine the existing session.
      appendUserTurn(text);
      setStatus("Asking for a revision…");
      const { turn } = await backend.sendMessage(state.sessionId, text);
      const turnEl = appendAssistantTurn(turn);
      if (turn.kind === "code") await executeTurn(turnEl, turn);
    }
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

// Transcript event delegation: Run / JSON / CSV per code turn.
async function onTranscriptClick(e) {
  const btn = e.target.closest("button[data-action]");
  if (!btn) return;
  const turnEl = btn.closest(".turn.assistant");
  if (!turnEl) return;
  const turnIndex = Number(turnEl.dataset.turnIndex);
  const action = btn.dataset.action;

  if (action === "run") {
    const tab = await getActiveTab();
    if (!tab) { setStatus("No active tab.", "error"); return; }
    state.tabId = tab.id;
    state.contentScriptsInjected = false; // tab might have changed
    const code = turnEl.querySelector("pre")?.textContent;
    if (!code) { setStatus("Code missing.", "error"); return; }
    await executeTurn(turnEl, { code, turn_index: turnIndex });
    return;
  }
  const out = state.lastResults.get(turnIndex);
  if (!out) { setStatus("Run the code first.", "error"); return; }
  if (action === "download-json") {
    download(JSON.stringify(out, null, 2), `scrape-${ts()}.json`, "application/json");
  } else if (action === "download-csv") {
    download(toCsv(out.items), `scrape-${ts()}.csv`, "text/csv");
  }
}

// ───────────────────────── restore on open ──────────────────────────────────

async function restoreSession(sessionId) {
  let info;
  try { info = await backend.fetchSession(sessionId); }
  catch (e) {
    // Backend doesn't know this session anymore (db wiped, etc).
    await clearActiveSession();
    setStatus("Previous session not found; starting fresh.", "");
    return;
  }
  state.sessionId = sessionId;
  setActiveMode();
  $("page-kind").textContent = `Session on ${shortUrl(info.url)}`;

  // The initial intention isn't persisted as a turn — it lives in
  // sessions.intention. Always render it first so the transcript starts with
  // the user's opening prompt, regardless of how many follow-ups there are.
  if (info.intention) appendUserTurn(info.intention);

  for (const t of info.turns) {
    if (t.role === "extension" && t.kind === "signals") continue;
    if (t.role === "extension" && t.kind === "result") continue; // shown via dedicated UI on the prior code turn
    if (t.role === "user" && t.kind === "message") {
      const content = t.payload?.content || "";
      appendUserTurn(content);
    } else if (t.role === "llm" && t.kind === "code") {
      const p = t.payload || {};
      appendAssistantTurn({
        kind: "code",
        turn_index: t.turn_index,
        code: p.code || "",
        rationale: p.rationale || "",
      });
    } else if (t.role === "llm" && t.kind === "no_code") {
      const p = t.payload || {};
      appendAssistantTurn({
        kind: "no_code",
        turn_index: t.turn_index,
        reason: p.reason || "",
      });
    }
  }
  scrollTranscript();
  setStatus(`Restored session with ${$("transcript").children.length} turn(s).`, "");
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
  $("transcript").addEventListener("click", onTranscriptClick);
  $("message").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); onSend(); }
  });

  await detectPage();
  const existing = await loadActiveSession();
  if (existing) await restoreSession(existing);
});
