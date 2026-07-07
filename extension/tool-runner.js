// Runs the extension-side tools the agent asks for: `execute_script` (JS in the
// live tab, via chrome.userScripts) and `screenshot` (captureVisibleTab). The
// server-side tools (list/read/write/edit over the per-origin file store) never
// reach here — the backend resolves those itself.

/// Run one tool call. Always resolves; failures come back as { error } so the
/// agent can see and react to them.
export async function runTool(tabId, call) {
  const { call_id, tool, args } = call;
  try {
    let result;
    if (tool === "execute_script") {
      result = await executeScript(tabId, args?.code ?? "");
    } else if (tool === "screenshot") {
      result = await screenshot();
    } else {
      throw new Error(`unknown extension tool: ${tool}`);
    }
    return { call_id, tool, result: result === undefined ? null : result };
  } catch (e) {
    return { call_id, tool, error: String(e?.message || e) };
  }
}

/// Run JS in the page and return whatever it returns (probe value or the
/// { kind, items } extraction). Uses chrome.userScripts so the code runs
/// outside the page CSP.
async function executeScript(tabId, code) {
  if (!chrome.userScripts) {
    throw new Error(
      "chrome.userScripts unavailable. Enable Developer Mode in " +
      'chrome://extensions and toggle "Allow User Scripts" for this extension.'
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
      throw new Error('User scripts disabled. Toggle "Allow User Scripts" on this extension.');
    }
    throw new Error(`userScripts.execute failed: ${msg}`);
  }
  const first = results && results[0];
  if (!first) throw new Error("no frames executed");
  if (first.error) throw new Error(`script error: ${first.error.message || first.error}`);
  return first.result;
}

/// Capture the visible tab as a PNG and return it base64-encoded for the agent
/// to view. Works with the extension's activeTab + host permissions.
async function screenshot() {
  const dataUrl = await chrome.tabs.captureVisibleTab({ format: "png" });
  if (!dataUrl) throw new Error("captureVisibleTab returned nothing");
  const comma = dataUrl.indexOf(",");
  const image_base64 = comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
  return { image_base64, mime: "image/png" };
}
