// Runs agent JS in the page's *main* world through the Chrome DevTools Protocol
// (chrome.debugger → Runtime.evaluate).
//
// chrome.userScripts runs in an isolated world: it shares the DOM but not the
// page's JS heap, so it can't see app globals (Apollo/Redux stores, framework
// hooks) or the per-world expandos frameworks hang on nodes (React's
// `__reactProps$…`), and the page CSP stops it from bridging into the main
// world with an inline <script>. Runtime.evaluate has none of those limits — it
// is exactly what typing into the DevTools console does.
//
// The cost is Chrome's "started debugging this browser" infobar while attached.
// We attach lazily on the first script and stay attached across navigations
// (a tab target survives them), detaching when the session ends or the tab
// closes. If the user dismisses the infobar we honour that and the caller falls
// back to userScripts for the rest of the panel's life.

const PROTOCOL_VERSION = "1.3";

const attached = new Set();
let declinedByUser = false;

chrome.debugger.onDetach.addListener((source, reason) => {
  if (source.tabId == null) return;
  attached.delete(source.tabId);
  if (reason === "canceled_by_user") declinedByUser = true;
});

/// False once the user has cancelled the debugger infobar; callers should then
/// use the isolated-world path instead of re-prompting them.
export function debuggerAllowed() {
  return !declinedByUser && !!chrome.debugger;
}

/// The debugger couldn't attach (unscriptable page, policy, user refusal).
/// Nothing ran in the page, so the caller can safely try another route.
export class DebuggerUnavailable extends Error {}

async function attach(tabId) {
  if (attached.has(tabId)) return;
  try {
    await chrome.debugger.attach({ tabId }, PROTOCOL_VERSION);
  } catch (e) {
    // Left over from an earlier panel instance — that session is still ours.
    const msg = e?.message || String(e);
    if (!/already attached/i.test(msg)) throw new DebuggerUnavailable(msg);
  }
  attached.add(tabId);
}

export async function detach(tabId) {
  if (tabId == null || !attached.has(tabId)) return;
  attached.delete(tabId);
  try { await chrome.debugger.detach({ tabId }); } catch {}
}

/// Detach from every tab we attached to (session stopped or replaced).
export async function detachAll() {
  await Promise.all([...attached].map(detach));
}

/// Evaluate `code` (an async function body) in the tab's top frame, main world,
/// and return its value. Throws with the page's own error message on an
/// uncaught exception.
export async function evaluateInPage(tabId, code) {
  await attach(tabId);
  const expression = `(async function(){\n${code}\n})()`;
  const res = await chrome.debugger.sendCommand({ tabId }, "Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
    // Lets the script do things the page gates on user activation
    // (focus, clipboard, opening popovers some components guard).
    userGesture: true,
    allowUnsafeEvalBlockedByCSP: true,
  });
  if (res.exceptionDetails) {
    const d = res.exceptionDetails;
    throw new Error(`script error: ${d.exception?.description || d.text || "uncaught exception"}`);
  }
  return res.result?.value;
}
