// Runs the extension-side tools the agent asks for: `execute_script` (JS in the
// live tab, via chrome.userScripts), `navigate` (move the tab and wait for the
// load), and `screenshot` (captureVisibleTab of the session tab). The server-side tools
// (list/read/write/edit over the per-origin file store) never reach here — the
// backend resolves those itself.
//
// The scrape session lives on the backend, so it spans as many pages as the
// agent wants to visit. Only the *page* context is per-page: a navigation tears
// down whatever JS was running in the tab. Everything below exists to keep that
// distinction honest — wait out navigations rather than failing on them, and
// when one does kill a script, tell the agent exactly that instead of handing it
// a silent `null`.

/// Run one tool call. Always resolves; failures come back as { error } so the
/// agent can see and react to them. `onProgress` receives human-readable
/// status updates for long waits (navigate / tab settle / LLM round-trip).
/// A 60s overall cap guarantees we never hang the panel transcript forever.
export async function runTool(tabId, call, onProgress) {
  const { call_id, tool, args } = call;
  const progress = (m) => { try { onProgress?.(m); } catch {} };
  try {
    let result;
    const capped = (p, label) =>
      Promise.race([
        p,
        new Promise((_, reject) =>
          setTimeout(() => reject(new Error(`${label} timed out after 60s`)), 60000)),
      ]);
    if (tool === "execute_script") {
      result = await capped(
        withPageReady(tabId, () => executeScriptAcrossNav(tabId, args?.code ?? ""), progress),
        tool,
      );
    } else if (tool === "navigate") {
      result = await capped(navigate(tabId, args?.url, args?.wait_ms, progress), tool);
    } else if (tool === "screenshot") {
      result = await capped(withPageReady(tabId, () => screenshot(tabId, progress), progress), tool);
    } else if (tool === "page_to_markdown") {
      result = await capped(withPageReady(tabId, () => pageHtml(tabId), progress), tool);
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
async function withPageReady(tabId, fn, onProgress) {
  const progress = (m) => { try { onProgress?.(m); } catch {} };
  progress("Waiting for page to settle…");
  await waitForTabReady(tabId, 10000);
  try {
    return await fn();
  } catch (e) {
    const msg = e?.message || String(e);
    if (TRANSIENT_NAV_RE.test(msg)) {
      progress("Page moved mid-step — waiting for new page…");
      await waitForTabReady(tabId, 10000);
      return await fn();
    }
    throw e;
  }
}

/// Resolve once the tab is loaded and on a page scripts can run in. Guards the
/// common failure: acting on a tab that navigated to a 404/error page or is
/// still loading. Never rejects on timeout — it lets the actual tool call
/// surface a precise error instead.
async function waitForTabReady(tabId, timeoutMs = 10000) {
  const tab = await waitUntilComplete(tabId, timeoutMs);
  assertScriptableUrl(tab.url);
}

/// Resolve with the tab once it is not loading. Unlike `waitForTabReady` this
/// says nothing about whether scripts can run there — `navigate` uses it because
/// navigating *away* from an unscriptable page (a chrome-error:// dead link, say)
/// is exactly how the agent gets unstuck.
async function waitUntilComplete(tabId, timeoutMs = 10000) {
  let tab;
  try {
    tab = await chrome.tabs.get(tabId);
  } catch {
    throw new Error("the page is gone (tab closed) — reopen it and try again.");
  }
  if (tab.status === "complete") return tab;

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
  // The load may have landed somewhere else entirely; re-read rather than
  // reporting the tab as it was before.
  try {
    return await chrome.tabs.get(tabId);
  } catch {
    throw new Error("the page is gone (tab closed) — reopen it and try again.");
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

/// `execute_script`, but aware that the page can go out from under it.
///
/// A pagination click (or any navigation the agent's own code triggers) destroys
/// the JS context mid-run: `userScripts.execute` then either resolves with no
/// value or throws a context-destroyed error, and the data the script had
/// gathered in local variables is gone. Reporting that as `null` — which is what
/// used to happen — reads to the agent as "the extraction returned nothing", so
/// it retries the same doomed script. Instead we wait for the new page to
/// settle and hand back an explicit `{ navigated: true, from, to }` so the agent
/// knows the session moved on and can re-extract where it landed.
async function executeScriptAcrossNav(tabId, code) {
  const from = await tabUrl(tabId);
  const nav = watchNavigation(tabId);
  try {
    const result = await executeScript(tabId, code);
    if (nav.navigated && (result === undefined || result === null)) {
      return await navigationReport(tabId, from);
    }
    return result;
  } catch (e) {
    // A transient error *with* an observed navigation is the same story — and
    // re-running the script (what withPageReady would do) would just replay a
    // click on a page that has already moved.
    if (nav.navigated && TRANSIENT_NAV_RE.test(e?.message || String(e))) {
      return await navigationReport(tabId, from);
    }
    throw e;
  } finally {
    nav.stop();
  }
}

async function navigationReport(tabId, from) {
  await waitForTabReady(tabId);
  return {
    navigated: true,
    from,
    to: await tabUrl(tabId),
    note:
      "The page navigated while your script was running, so its context was destroyed " +
      "and anything it had collected in memory was lost. Your session is still alive on " +
      "the new page. Extract the page you are on now (with `append: true` if you are " +
      "collecting across pages), and use the `navigate` tool to change pages instead of " +
      "clicking inside `execute_script`.",
  };
}

/// Watch a tab for navigation starts until `.stop()`. `.navigated` flips true on
/// the first `loading` status or URL change seen for this tab.
function watchNavigation(tabId) {
  const w = { navigated: false, stop: () => chrome.tabs.onUpdated.removeListener(listener) };
  function listener(updatedId, info) {
    if (updatedId !== tabId) return;
    if (info.status === "loading" || info.url) w.navigated = true;
  }
  chrome.tabs.onUpdated.addListener(listener);
  return w;
}

/// Move the tab to `url` and resolve once the new page has loaded. This is the
/// agent's supported way to change pages: the backend session is untouched, so
/// the next tool call simply runs against the new document.
async function navigate(tabId, url, waitMs, onProgress) {
  if (!url || typeof url !== "string") throw new Error("`url` is required");
  const progress = (m) => { try { onProgress?.(m); } catch {} };
  // Settle any load already in flight first. Otherwise its `complete` event
  // arrives after we start ours and we'd take it for our own, handing the agent
  // the page it was leaving. It also makes `current` the real base for a
  // relative URL.
  progress("Settling current page…");
  const current = (await waitUntilComplete(tabId, 10000)).url ?? null;
  let target;
  try {
    target = current ? new URL(url, current).href : new URL(url).href;
  } catch {
    throw new Error(`not a usable URL: ${url}`);
  }
  assertScriptableUrl(target);

  // Listener first: tabs.update can complete the load before we could attach it.
  progress(`Navigating to ${target}…`);
  const loaded = waitForLoad(tabId, target, 15000, progress);
  await chrome.tabs.update(tabId, { url: target });
  await loaded;

  const settle = Math.min(Math.max(Number(waitMs ?? 500) || 0, 0), 15000);
  if (settle) {
    progress("Waiting for page to settle…");
    await sleep(settle);
  }

  const tab = await chrome.tabs.get(tabId);
  assertScriptableUrl(tab.url);
  return { url: tab.url, title: tab.title ?? null, from: current };
}

/// Resolve when the tab finishes the load we just started. Chrome reports a
/// navigation as loading→complete, but a same-URL or fragment-only navigation
/// may report nothing at all — so if no event arrives shortly after the update,
/// fall back to polling the tab's own state.
function waitForLoad(tabId, target, timeoutMs = 15000, onProgress) {
  return new Promise((resolve) => {
    let started = false;
    let settled = false;
    let timer, poll;
    const startedAt = Date.now();
    const progress = (m) => { try { onProgress?.(m); } catch {} };
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      clearInterval(poll);
      chrome.tabs.onUpdated.removeListener(listener);
      resolve();
    };
    // Any load that begins now is ours — the caller waited out the previous one.
    // We accept whatever URL it settles on rather than insisting on `target`, so
    // a redirect still counts as arrival.
    function listener(updatedId, info) {
      if (updatedId !== tabId) return;
      if (info.status === "loading" || info.url) {
        started = true;
        progress("Page loading…");
      } else if (info.status === "complete" && started) finish();
    }
    chrome.tabs.onUpdated.addListener(listener);
    timer = setTimeout(() => {
      progress("Load wait timed out — continuing with current page…");
      finish();
    }, timeoutMs);
    // No load ever started: finish early once the tab looks stable. Accept any
    // complete page after ~2s (covers instant loads and redirect races where we
    // missed the loading event); exact-URL match finishes immediately.
    // Without this, a redirect that skips our listener would wait out the full
    // timeout with a frozen transcript.
    poll = setInterval(async () => {
      if (started) return;
      try {
        const t = await chrome.tabs.get(tabId);
        if (t.status !== "complete") return;
        if (sameDocument(t.url, target)) {
          finish();
        } else if (Date.now() - startedAt > 2000) {
          // Already stable on some page (redirect / no-op) — don't hang.
          finish();
        }
      } catch {
        finish(); // tab gone; the caller's tabs.get will report it properly
      }
    }, 400);
  });
}

/// Same URL ignoring the fragment — a hash change doesn't reload the document.
function sameDocument(a, b) {
  try {
    const ua = new URL(a), ub = new URL(b);
    return ua.origin + ua.pathname + ua.search === ub.origin + ub.pathname + ub.search;
  } catch {
    return a === b;
  }
}

async function tabUrl(tabId) {
  try {
    return (await chrome.tabs.get(tabId)).url ?? null;
  } catch {
    return null;
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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

/// Capture the session's tab as a PNG and return it base64-encoded for the
/// agent to view. `captureVisibleTab` only sees whichever tab is in front of its
/// window, and the side panel lets the user switch tabs mid-run — so bring the
/// session's tab forward first rather than handing the agent a picture of some
/// other page.
async function screenshot(tabId, onProgress) {
  let tab = await chrome.tabs.get(tabId);
  if (!tab.active) {
    try { onProgress?.("Switching to the session's tab to capture it…"); } catch {}
    await chrome.tabs.update(tabId, { active: true });
    await sleep(300); // let the compositor paint the newly shown tab
    tab = await chrome.tabs.get(tabId);
  }
  const dataUrl = await chrome.tabs.captureVisibleTab(tab.windowId, { format: "png" });
  if (!dataUrl) throw new Error("captureVisibleTab returned nothing");
  const comma = dataUrl.indexOf(",");
  const image_base64 = comma >= 0 ? dataUrl.slice(comma + 1) : dataUrl;
  return { image_base64, mime: "image/png" };
}
