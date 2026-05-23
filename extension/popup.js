import { backend } from "./backend.js";

const $ = (id) => document.getElementById(id);

const state = {
  tabId: null,
  sessionId: null,
  lastResult: null,
  lastCode: null,
  lastRationale: null,
  contentScriptsInjected: false,
};

// ─────────────────────────── UI helpers ─────────────────────────────────────

function setStatus(msg, kind = "") {
  const el = $("status");
  el.textContent = msg || "";
  el.className = kind;
}
function hide(id) { $(id).classList.add("hidden"); }
function show(id) { $(id).classList.remove("hidden"); }

function reset() {
  state.sessionId = null;
  state.lastResult = null;
  state.lastCode = null;
  state.lastRationale = null;
  hide("rationale");
  hide("code-wrap");
  hide("result");
  $("download-json").disabled = true;
  $("download-csv").disabled = true;
  $("copy").disabled = true;
  $("result-body").textContent = "";
  setStatus("");
}

// ─────────────────────── content script glue ────────────────────────────────

async function getActiveTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

async function ensureSignalsScript(tabId) {
  if (state.contentScriptsInjected) return;
  await chrome.scripting.executeScript({
    target: { tabId },
    files: ["dom-signals.js"],
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

// Runs the LLM-generated code via chrome.userScripts. We can't use
// chrome.scripting.executeScript with `new Function(...)` because strict-CSP
// sites (like lu.ma) block runtime code generation, and MV3 forbids
// `unsafe-eval` in extension pages too. userScripts has its own context that
// is exempt from page CSP by default.
//
// CAVEAT(safety): the userScripts world bypasses CSP. We're trusting the
// backend → LLM chain. For local YOLO use that's fine. The user has to
// explicitly enable "Allow user scripts" on the extension card, which is a
// reasonable consent step.
async function runCode(tabId, code) {
  if (!chrome.userScripts) {
    throw new Error(
      "chrome.userScripts unavailable. Chrome 120+ is required. " +
      "If on a supported Chrome: enable Developer Mode in chrome://extensions and " +
      "toggle \"Allow user scripts\" on this extension's card."
    );
  }

  // Wrap the LLM body in an IIFE so a top-level `return` produces a value.
  // userScripts.execute uses the script's last evaluated expression as the
  // result for each frame.
  const wrapped = `(function(){\n${code}\n})()`;

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
        "User scripts are disabled. Open chrome://extensions, turn on " +
        "Developer Mode, then toggle \"Allow user scripts\" on this extension's card."
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
  const items = Array.isArray(out.items) ? out.items : [];
  const kind = out.kind === "event" ? "event" : "listing";
  return { kind, items };
}

// ─────────────────────────────── render ─────────────────────────────────────

function renderRationale(text) {
  const el = $("rationale");
  if (!text) { hide("rationale"); return; }
  el.textContent = text;
  show("rationale");
}

function renderCode(code) {
  $("code-body").textContent = code;
  show("code-wrap");
}

function renderResult(kind, items) {
  show("result");
  $("result-count").textContent = `${items.length} ${kind === "event" ? "event" : "item"}${items.length === 1 ? "" : "s"}`;
  const body = JSON.stringify({ kind, items }, null, 2);
  $("result-body").textContent = body.length > 5000 ? body.slice(0, 5000) + "\n…(truncated)" : body;
  const have = items.length > 0;
  $("download-json").disabled = !have;
  $("download-csv").disabled = !have;
  $("copy").disabled = !have;
}

// ─────────────────────────────── downloads ─────────────────────────────────

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

// ─────────────────────────────── handlers ──────────────────────────────────

async function onRun() {
  reset();
  const intention = $("intention").value.trim();
  if (!intention) { setStatus("Enter an intention first.", "error"); return; }
  const tab = await getActiveTab();
  if (!tab?.url || !/^https?:\/\//.test(tab.url)) {
    setStatus("Open a web page first.", "error"); return;
  }
  state.tabId = tab.id;
  state.contentScriptsInjected = false;
  $("run").disabled = true;
  setStatus("Collecting page signals…");

  try {
    const signals = await collectSignals(state.tabId);
    setStatus("Asking the planner for code…");
    const { session_id, turn } = await backend.createSession({
      url: tab.url, intention, signals,
    });
    state.sessionId = session_id;

    if (turn.kind === "no_code") {
      renderRationale(turn.reason);
      setStatus("Model declined to generate code.", "error");
      return;
    }
    if (turn.kind !== "code") {
      throw new Error(`unexpected turn kind: ${turn.kind}`);
    }
    state.lastCode = turn.code;
    state.lastRationale = turn.rationale;
    renderRationale(turn.rationale);
    renderCode(turn.code);

    setStatus("Running code in the tab…");
    const out = await runCode(state.tabId, turn.code);
    state.lastResult = out;
    renderResult(out.kind, out.items);

    await backend.recordResult(session_id, {
      items_count: out.items.length,
      errors: [],
    });
    setStatus(`Done. ${out.items.length} item(s).`, "ok");
  } catch (e) {
    setStatus(e.message || String(e), "error");
  } finally {
    $("run").disabled = false;
  }
}

async function detectPage() {
  const tab = await getActiveTab();
  if (!tab?.url) { $("page-kind").textContent = "(no active tab)"; return; }
  try {
    const u = new URL(tab.url);
    $("page-kind").textContent = `Ready on ${u.host}${u.pathname}`;
  } catch {
    $("page-kind").textContent = "(unsupported page)";
  }
}

document.addEventListener("DOMContentLoaded", () => {
  detectPage();
  $("run").addEventListener("click", onRun);
  $("download-json").addEventListener("click", () => {
    if (!state.lastResult) return;
    download(
      JSON.stringify({
        rationale: state.lastRationale,
        code: state.lastCode,
        kind: state.lastResult.kind,
        items: state.lastResult.items,
      }, null, 2),
      `scrape-${ts()}.json`,
      "application/json",
    );
  });
  $("download-csv").addEventListener("click", () => {
    if (!state.lastResult) return;
    download(toCsv(state.lastResult.items), `scrape-${ts()}.csv`, "text/csv");
  });
  $("copy").addEventListener("click", async () => {
    if (!state.lastResult) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(state.lastResult.items, null, 2));
      setStatus("Copied items.", "ok");
    } catch (e) { setStatus("Copy failed: " + e.message, "error"); }
  });
});
