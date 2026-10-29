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
//
// Agent scripts also get a `human` helper (click/type/press/scroll with real,
// human-paced input). Trusted input can only come from CDP's Input domain,
// i.e. from out here, so the helper is a bridge: a `Runtime.addBinding`
// function the page calls with `{ id, op, args }`, which surfaces here as
// `Runtime.bindingCalled`; we perform the input and settle the page-side
// promise with a second evaluate. One `execute_script` can therefore fill a
// whole form instead of one tool round-trip per keystroke-ish action.

const PROTOCOL_VERSION = "1.3";
const BINDING = "__scrapeHuman";
const STATE_KEY = `Symbol.for("scrape.human")`;

let bindingHandler = null;

/// Register `(tabId, op, args) => Promise<result>` to serve `human.*` calls.
export function setBindingHandler(fn) {
  bindingHandler = fn;
}

chrome.debugger.onEvent.addListener(async (source, method, params) => {
  if (method !== "Runtime.bindingCalled" || params?.name !== BINDING || source.tabId == null) return;
  let msg;
  try { msg = JSON.parse(params.payload); } catch { return; }
  let ok = true, value = null;
  try {
    if (!bindingHandler) throw new Error("human input is not available");
    value = (await bindingHandler(source.tabId, msg.op, msg.args ?? {})) ?? null;
  } catch (e) {
    ok = false;
    value = String(e?.message || e);
  }
  // The script may have navigated away meanwhile; then there's nobody to tell.
  try {
    await chrome.debugger.sendCommand({ tabId: source.tabId }, "Runtime.evaluate", {
      contextId: params.executionContextId,
      expression: `(() => {
        const st = globalThis[${STATE_KEY}], p = st && st.pending.get(${Number(msg.id)});
        if (!p) return;
        st.pending.delete(${Number(msg.id)});
        ${ok ? "p.resolve" : "p.reject"}(${ok ? JSON.stringify(value) : `new Error(${JSON.stringify(value)})`});
      })()`,
    });
  } catch {}
});

// Page-side half of the bridge. Grabs the binding into symbol-keyed state and
// removes the visible global, so page code doesn't trip over it.
const HUMAN_PRELUDE = `const human = (() => {
  const g = globalThis, K = ${STATE_KEY};
  let st = g[K];
  if (!st) {
    const send = g.${BINDING};
    if (typeof send !== "function") return null;
    st = g[K] = { seq: 0, pending: new Map(), send };
    try { delete g.${BINDING}; } catch {}
  }
  const call = (op, args) => new Promise((resolve, reject) => {
    const id = ++st.seq;
    st.pending.set(id, { resolve, reject });
    st.send(JSON.stringify({ id, op, args }));
  });
  // Elements can't cross the binding, so tag them and pass a selector instead.
  const target = (t) => {
    if (t == null) return {};
    if (typeof t === "string") return { selector: t };
    if (t instanceof Element) {
      const id = String(++st.seq);
      t.setAttribute("data-human-target", id);
      return { selector: '[data-human-target="' + id + '"]' };
    }
    return t;
  };
  return {
    click: (t, o) => call("click", { ...target(t), ...o }),
    hover: (t, o) => call("hover", { ...target(t), ...o }),
    type: (t, text, o) => {
      const { selector, text: target_text, index } = target(t);
      return call("type", { selector, target_text, index, text, ...o });
    },
    press: (key, o) => call("press", { key, ...o }),
    scroll: (t, o) => call("scroll", typeof t === "number" ? { dy: t, ...o } : { ...target(t), ...o }),
    pause: (min = 250, max = 800) => new Promise((r) => setTimeout(r, min + Math.random() * (max - min))),
  };
})();`;

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
  // Installed in every current and future context of the tab, so it survives
  // navigations for as long as we stay attached.
  try {
    await chrome.debugger.sendCommand({ tabId }, "Runtime.addBinding", { name: BINDING });
  } catch {}
  // The side panel holds the window's focus while the agent works, so without
  // this the page sees no focus/blur and :focus-gated widgets stay shut.
  try {
    await chrome.debugger.sendCommand({ tabId }, "Emulation.setFocusEmulationEnabled", { enabled: true });
  } catch {}
}

/// Send a raw CDP command to the tab, attaching first if needed.
export async function sendCommand(tabId, method, params = {}) {
  await attach(tabId);
  return await chrome.debugger.sendCommand({ tabId }, method, params);
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
/// uncaught exception. `{ human: true }` gives the code the `human` helper —
/// in an outer scope, so agent code may still declare its own `human`.
export async function evaluateInPage(tabId, code, { human = false } = {}) {
  await attach(tabId);
  const body = `(async function(){\n${code}\n})()`;
  const expression = human
    ? `(async function(){\n${HUMAN_PRELUDE}\nreturn await ${body};\n})()`
    : body;
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
