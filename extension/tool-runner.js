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
      result = await withPageReady(tabId, () => executeScript(tabId, args?.code ?? ""));
    } else if (tool === "screenshot") {
      result = await withPageReady(tabId, () => screenshot());
    } else if (tool === "page_to_markdown") {
      result = await withPageReady(tabId, () => pageHtml(tabId));
    } else {
      throw new Error(`unknown extension tool: ${tool}`);
    }
    return { call_id, tool, result: result === undefined ? null : result };
  } catch (e) {
    return { call_id, tool, error: String(e?.message || e) };
  }
}

// A navigation (link click, redirect, or a load the agent's own script kicked
// off) can leave the tab mid-load or its frame torn down, so tool calls fire
// against a context that no longer exists. These are the errors that means.
const TRANSIENT_NAV_RE =
  /frames? (was|were) removed|no frame with id|frame with id .* was removed|execution context was destroyed|the tab was (closed|discarded)|cannot access contents|no frames executed|receiving end does not exist|target closed/i;

/// Run an extension-side tool only once the tab has finished loading. On a
/// transient navigation error, wait for the (likely new) page to settle and
/// retry once — this is what keeps a session alive across in-page navigation.
async function withPageReady(tabId, fn) {
  await waitForTabReady(tabId);
  try {
    return await fn();
  } catch (e) {
    const msg = e?.message || String(e);
    if (TRANSIENT_NAV_RE.test(msg)) {
      await waitForTabReady(tabId);
      return await fn();
    }
    throw e;
  }
}

/// Resolve once the tab is loaded and on a page scripts can run in. Guards the
/// common failure: acting on a tab that navigated to a 404/error page or is
/// still loading. Never rejects on timeout — it lets the actual tool call
/// surface a precise error instead.
async function waitForTabReady(tabId, timeoutMs = 15000) {
  let tab;
  try {
    tab = await chrome.tabs.get(tabId);
  } catch {
    throw new Error("the page is gone (tab closed) — reopen it and try again.");
  }
  assertScriptableUrl(tab.url);

  if (tab.status !== "complete") {
    await new Promise((resolve) => {
      const done = () => {
        clearTimeout(timer);
        chrome.tabs.onUpdated.removeListener(listener);
        resolve();
      };
      const timer = setTimeout(done, timeoutMs);
      function listener(updatedId, info) {
        if (updatedId === tabId && info.status === "complete") done();
      }
      chrome.tabs.onUpdated.addListener(listener);
    });
    // The load may have redirected to somewhere unscriptable.
    try {
      const t2 = await chrome.tabs.get(tabId);
      assertScriptableUrl(t2.url);
    } catch { /* the tool call will surface a clear error */ }
  }
}

/// Reject non-web pages up front with an actionable message, instead of letting
/// the browser throw an opaque error deep in userScripts.execute.
function assertScriptableUrl(url) {
  if (!url) return; // unknown (no host permission yet) — let the call proceed
  if (/^https?:\/\//.test(url)) return;
  if (/^chrome-error:\/\//.test(url)) {
    throw new Error(
      "the page failed to load (network error or the link is dead), so there's " +
      "nothing to scrape here. Navigate to a working page and try again."
    );
  }
  const scheme = url.split(":")[0];
  throw new Error(`the tab is on a ${scheme}: page that scripts can't run in; open an http(s) page.`);
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

/// Grab the live page's full serialized HTML. Returned to the backend, which
/// runs pandoc over it to hand the agent clean Markdown. Uses the same
/// chrome.userScripts path as executeScript so it reads the post-render DOM
/// outside the page CSP.
async function pageHtml(tabId) {
  const out = await executeScript(
    tabId,
    "return { html: document.documentElement.outerHTML, url: location.href };"
  );
  if (!out || typeof out.html !== "string") throw new Error("could not read page HTML");
  return { html: out.html, url: out.url };
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
