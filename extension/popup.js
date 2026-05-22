const $ = (id) => document.getElementById(id);

const state = {
  result: null,
};

function setStatus(msg, kind = "") {
  const el = $("status");
  el.textContent = msg || "";
  el.className = kind;
}

function renderPreview(result) {
  const preview = $("preview");
  preview.classList.remove("hidden");
  $("preview-count").textContent =
    `${result.count} ${result.kind === "event" ? "event" : "items"}`;
  const body = JSON.stringify(result, null, 2);
  $("preview-body").textContent =
    body.length > 4000 ? body.slice(0, 4000) + "\n… (truncated in preview)" : body;
  $("download-json").disabled = result.count === 0;
  $("download-csv").disabled = result.count === 0;
}

async function getActiveTab() {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  return tab;
}

async function detectPage() {
  const tab = await getActiveTab();
  if (!tab?.url || !/^https:\/\/lu\.ma\//.test(tab.url)) {
    $("page-kind").textContent = "Not a lu.ma page — open an event or listing first.";
    $("scrape").disabled = true;
    return;
  }
  $("page-kind").textContent = `Ready on ${new URL(tab.url).pathname}`;
}

async function runScrape() {
  setStatus("Scraping…");
  $("scrape").disabled = true;
  try {
    const tab = await getActiveTab();
    const [{ result } = {}] = await chrome.scripting.executeScript({
      target: { tabId: tab.id },
      files: ["content.js"],
    });
    if (!result) throw new Error("No result returned from page.");
    state.result = result;
    renderPreview(result);
    setStatus(`Scraped ${result.count} item(s).`, "ok");
  } catch (e) {
    setStatus(e.message || String(e), "error");
  } finally {
    $("scrape").disabled = false;
  }
}

function toCsv(items) {
  if (!items.length) return "";
  const keys = Array.from(
    items.reduce((set, item) => {
      Object.keys(item).forEach((k) => set.add(k));
      return set;
    }, new Set())
  );
  const escape = (v) => {
    if (v == null) return "";
    const s = typeof v === "string" ? v : JSON.stringify(v);
    if (/[",\n\r]/.test(s)) return '"' + s.replace(/"/g, '""') + '"';
    return s;
  };
  const lines = [keys.join(",")];
  for (const item of items) lines.push(keys.map((k) => escape(item[k])).join(","));
  return lines.join("\n");
}

function timestamp() {
  return new Date().toISOString().replace(/[:.]/g, "-");
}

function download(content, filename, mime) {
  const blob = new Blob([content], { type: mime });
  const url = URL.createObjectURL(blob);
  chrome.downloads.download({ url, filename, saveAs: true }, () => {
    // The blob URL is reclaimed once the download starts; revoke after a beat.
    setTimeout(() => URL.revokeObjectURL(url), 5000);
  });
}

function downloadJson() {
  if (!state.result) return;
  download(
    JSON.stringify(state.result, null, 2),
    `luma-${state.result.kind}-${timestamp()}.json`,
    "application/json"
  );
}

function downloadCsv() {
  if (!state.result) return;
  download(
    toCsv(state.result.items),
    `luma-${state.result.kind}-${timestamp()}.csv`,
    "text/csv"
  );
}

async function copyJson() {
  if (!state.result) return;
  try {
    await navigator.clipboard.writeText(JSON.stringify(state.result, null, 2));
    setStatus("Copied to clipboard.", "ok");
  } catch (e) {
    setStatus("Copy failed: " + e.message, "error");
  }
}

document.addEventListener("DOMContentLoaded", () => {
  detectPage();
  $("scrape").addEventListener("click", runScrape);
  $("download-json").addEventListener("click", downloadJson);
  $("download-csv").addEventListener("click", downloadCsv);
  $("copy").addEventListener("click", copyJson);
});
