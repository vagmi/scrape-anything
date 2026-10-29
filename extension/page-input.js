// Real (trusted) mouse and keyboard input for the agent, sent through CDP's
// Input domain. Unlike events dispatched from a script, these are
// indistinguishable from the user's own: `isTrusted` is true, focus moves,
// default actions run, and widgets that only listen for genuine typing
// (debounced autocompletes, Downshift/Headless UI comboboxes) respond.
//
// Mouse moves follow human-like curves (see human-cursor.js) from wherever the
// pointer was last left in that tab, timed by distance and target size.

import { humanPath, movementMs, pointInBox, randInt, randomNormal } from "./human-cursor.js";
import { evaluateInPage, sendCommand } from "./page-debugger.js";

const sleep = (ms) => new Promise((r) => setTimeout(r, Math.max(0, ms)));

/// Last pointer position per tab, viewport CSS px. Lost on panel reload, in
/// which case the first move starts from somewhere plausible mid-page.
const pointer = new Map();

// ───────────────────────────── targeting ────────────────────────────────────

/// Find the element (by CSS `selector`, or by visible `text` optionally scoped
/// to `selector`), scroll it into view and return its viewport box plus a short
/// description. Runs in the page.
async function locate(tabId, { selector, text, index }) {
  const out = await evaluateInPage(
    tabId,
    `const SEL = ${JSON.stringify(selector ?? null)};
     const TEXT = ${JSON.stringify(text ?? null)};
     const INDEX = ${JSON.stringify(index ?? 0)};
     const visible = (el) => {
       const r = el.getBoundingClientRect();
       if (r.width < 1 || r.height < 1) return false;
       const cs = getComputedStyle(el);
       return cs.visibility !== "hidden" && cs.display !== "none" && cs.opacity !== "0";
     };
     let el;
     if (TEXT != null) {
       const want = TEXT.trim().toLowerCase().replace(/\\s+/g, " ");
       const pool = [...document.querySelectorAll(SEL || "body *")].filter(visible);
       const norm = (e) => (e.innerText || e.value || e.getAttribute("aria-label") || "")
         .trim().toLowerCase().replace(/\\s+/g, " ");
       const exact = pool.filter((e) => norm(e) === want);
       const partial = pool.filter((e) => norm(e).includes(want));
       // Smallest match is the most specific one (the option, not its list).
       const area = (e) => { const r = e.getBoundingClientRect(); return r.width * r.height; };
       const ranked = (exact.length ? exact : partial).sort((a, b) => area(a) - area(b));
       el = ranked[INDEX];
       if (!el) return { error: \`no visible element with text "\${TEXT}"\${SEL ? \` inside \${SEL}\` : ""}\` };
     } else {
       const all = [...document.querySelectorAll(SEL)];
       el = all.filter(visible)[INDEX] ?? all[INDEX];
       if (!el) return { error: \`no element matches \${SEL}\` };
     }
     el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
     await new Promise((r) => setTimeout(r, 60));
     const r = el.getBoundingClientRect();
     const label = (el.innerText || el.value || el.getAttribute("aria-label") || el.getAttribute("placeholder") || "")
       .trim().replace(/\\s+/g, " ").slice(0, 80);
     window.__scrapeTarget = el;
     return {
       box: { x: r.left, y: r.top, width: r.width, height: r.height },
       viewport: { width: innerWidth, height: innerHeight },
       element: el.tagName.toLowerCase() + (el.id ? "#" + el.id : "") + (label ? \` "\${label}"\` : ""),
     };`,
  );
  if (!out || out.error) throw new Error(out?.error || "could not locate the element");
  return out;
}

/// What sits at viewport point (x, y), and whether it is the located target
/// (or inside it). Lets the click report when an overlay swallowed it.
async function hitTest(tabId, x, y) {
  return await evaluateInPage(
    tabId,
    `const hit = document.elementFromPoint(${x}, ${y});
     const t = window.__scrapeTarget;
     const ok = !!(hit && t && (t === hit || t.contains(hit)));
     return { ok, hit: hit ? hit.tagName.toLowerCase() + (hit.id ? "#" + hit.id : "") +
       (hit.className && typeof hit.className === "string" ? "." + hit.className.trim().split(/\\s+/).slice(0, 2).join(".") : "") : null };`,
  );
}

async function viewportSize(tabId) {
  return await evaluateInPage(tabId, "return { width: innerWidth, height: innerHeight };");
}

// ───────────────────────────── mouse ────────────────────────────────────────

async function moveMouse(tabId, to, targetWidth, viewport) {
  const from = pointer.get(tabId) ?? {
    x: viewport.width * (0.3 + Math.random() * 0.4),
    y: viewport.height * (0.3 + Math.random() * 0.4),
  };
  const clampPt = (p) => ({
    x: Math.min(Math.max(p.x, 0), viewport.width - 1),
    y: Math.min(Math.max(p.y, 0), viewport.height - 1),
  });
  const path = humanPath(from, to).map(clampPt);
  const total = movementMs(Math.hypot(to.x - from.x, to.y - from.y), targetWidth);
  const step = total / Math.max(path.length - 1, 1);
  for (const p of path) {
    await sendCommand(tabId, "Input.dispatchMouseEvent", { type: "mouseMoved", x: p.x, y: p.y });
    await sleep(step);
  }
  pointer.set(tabId, path[path.length - 1] ?? to);
}

/// Move to the target along a human path and click it. Target is a
/// `selector`, visible `text`, or raw viewport `x`/`y`.
export async function click(tabId, args = {}) {
  const button = ["left", "right", "middle"].includes(args.button) ? args.button : "left";
  const clicks = Math.min(Math.max(Number(args.click_count ?? 1) || 1, 1), 3);
  let box, viewport, element = null;
  if (args.selector || args.text) {
    ({ box, viewport, element } = await locate(tabId, args));
  } else if (Number.isFinite(args.x) && Number.isFinite(args.y)) {
    viewport = await viewportSize(tabId);
    box = { x: args.x - 2, y: args.y - 2, width: 4, height: 4 };
  } else {
    throw new Error("give a `selector`, `text`, or `x` and `y`");
  }

  let at = element ? pointInBox(box) : { x: args.x, y: args.y };
  let hit = element ? await hitTest(tabId, at.x, at.y) : null;
  if (hit && !hit.ok) {
    // A random point can land on a sibling overlapping the edge; the middle
    // is the next best guess before reporting it as covered.
    const mid = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
    const midHit = await hitTest(tabId, mid.x, mid.y);
    if (midHit.ok) { at = mid; hit = midHit; }
  }

  await moveMouse(tabId, at, Math.min(box.width, box.height), viewport);
  await sleep(randInt(40, 140)); // settle before pressing
  for (let n = 1; n <= clicks; n++) {
    const base = { x: at.x, y: at.y, button, clickCount: n };
    await sendCommand(tabId, "Input.dispatchMouseEvent", { type: "mousePressed", ...base, buttons: 1 });
    await sleep(randInt(45, 110));
    await sendCommand(tabId, "Input.dispatchMouseEvent", { type: "mouseReleased", ...base, buttons: 0 });
    if (n < clicks) await sleep(randInt(70, 140));
  }

  const result = { clicked: element ?? `(${Math.round(at.x)}, ${Math.round(at.y)})`, x: Math.round(at.x), y: Math.round(at.y) };
  if (hit && !hit.ok) {
    result.warning = `the point was covered by ${hit.hit}, which received the click instead`;
  }
  return result;
}

/// Move the pointer over the target without clicking (menus, tooltips).
export async function hover(tabId, args = {}) {
  const { box, viewport, element } = await locate(tabId, args);
  const at = pointInBox(box);
  await moveMouse(tabId, at, Math.min(box.width, box.height), viewport);
  return { hovered: element, x: Math.round(at.x), y: Math.round(at.y) };
}

/// Scroll with the mouse wheel, in uneven notches like a real wheel or
/// trackpad flick. With a target, the pointer moves over it first so the wheel
/// scrolls that container rather than the page. `dy` > 0 scrolls down.
export async function scroll(tabId, args = {}) {
  const dy = Number(args.dy ?? 0) || 0;
  const dx = Number(args.dx ?? 0) || 0;
  if (!dy && !dx) throw new Error("give `dy` (and/or `dx`) in pixels");
  let at = pointer.get(tabId);
  if (args.selector || args.text) {
    at = (await hover(tabId, args));
  } else if (!at) {
    const vp = await viewportSize(tabId);
    at = { x: vp.width * (0.4 + Math.random() * 0.2), y: vp.height * (0.4 + Math.random() * 0.2) };
  }
  const total = Math.hypot(dx, dy);
  let done = 0;
  while (done < total) {
    const notch = Math.min(total - done, randInt(80, 130));
    const f = notch / total;
    await sendCommand(tabId, "Input.dispatchMouseEvent", {
      type: "mouseWheel", x: at.x, y: at.y, deltaX: dx * f, deltaY: dy * f,
    });
    done += notch;
    await sleep(randInt(25, 70));
  }
  const pos = await evaluateInPage(tabId, "return { scrollX: Math.round(scrollX), scrollY: Math.round(scrollY) };");
  return { scrolled: { dx, dy }, ...pos };
}

// ───────────────────────────── keyboard ─────────────────────────────────────

const KEYS = {
  Enter: { code: "Enter", keyCode: 13, text: "\r" },
  Tab: { code: "Tab", keyCode: 9 },
  Backspace: { code: "Backspace", keyCode: 8 },
  Delete: { code: "Delete", keyCode: 46 },
  Escape: { code: "Escape", keyCode: 27 },
  ArrowUp: { code: "ArrowUp", keyCode: 38 },
  ArrowDown: { code: "ArrowDown", keyCode: 40 },
  ArrowLeft: { code: "ArrowLeft", keyCode: 37 },
  ArrowRight: { code: "ArrowRight", keyCode: 39 },
  Home: { code: "Home", keyCode: 36 },
  End: { code: "End", keyCode: 35 },
  PageUp: { code: "PageUp", keyCode: 33 },
  PageDown: { code: "PageDown", keyCode: 34 },
  Space: { key: " ", code: "Space", keyCode: 32, text: " " },
};
const MODIFIERS = { Alt: 1, Control: 2, Ctrl: 2, Meta: 4, Cmd: 4, Shift: 8 };

function keyDef(key) {
  if (KEYS[key]) return { key: KEYS[key].key ?? key, ...KEYS[key] };
  if ([...key].length === 1) {
    const upper = key.toUpperCase();
    const keyCode = /[A-Z0-9]/.test(upper) ? upper.charCodeAt(0) : 0;
    const code = /[A-Z]/.test(upper) ? `Key${upper}` : /[0-9]/.test(key) ? `Digit${key}` : "";
    return { key, code, keyCode, text: key };
  }
  throw new Error(`unknown key "${key}" — use a single character or one of ${Object.keys(KEYS).join(", ")}`);
}

async function sendKey(tabId, def, modifiers = 0) {
  // With Ctrl/Alt/Meta held it's a shortcut, not text.
  const typing = def.text && !(modifiers & ~8);
  const common = {
    key: def.key, code: def.code, modifiers,
    windowsVirtualKeyCode: def.keyCode, nativeVirtualKeyCode: def.keyCode,
  };
  await sendCommand(tabId, "Input.dispatchKeyEvent", {
    type: typing ? "keyDown" : "rawKeyDown",
    ...common,
    ...(typing ? { text: def.text, unmodifiedText: def.text } : {}),
  });
  await sleep(randInt(25, 70));
  await sendCommand(tabId, "Input.dispatchKeyEvent", { type: "keyUp", ...common });
}

/// Press one key or chord, e.g. "Enter", "ArrowDown", "Shift+Tab", "Control+a".
/// `repeat` presses it that many times.
export async function pressKey(tabId, args = {}) {
  const spec = String(args.key ?? "");
  if (!spec) throw new Error("`key` is required");
  const parts = spec.split("+").filter(Boolean);
  const main = parts.pop() ?? "+";
  let modifiers = 0;
  for (const m of parts) {
    if (!(m in MODIFIERS)) throw new Error(`unknown modifier "${m}" — use Alt, Control, Meta or Shift`);
    modifiers |= MODIFIERS[m];
  }
  const def = keyDef(main);
  const times = Math.min(Math.max(Number(args.repeat ?? 1) || 1, 1), 50);
  for (let i = 0; i < times; i++) {
    await sendKey(tabId, def, modifiers);
    if (i < times - 1) await sleep(randInt(60, 160));
  }
  return { pressed: spec, times };
}

/// Type `text` with human pacing. With a target, clicks it first to focus; with
/// `clear: true`, selects the field's existing content so typing replaces it.
export async function typeText(tabId, args = {}) {
  const text = String(args.text ?? "");
  let focused = null;
  if (args.selector || args.target_text) {
    const r = await click(tabId, { selector: args.selector, text: args.target_text, index: args.index });
    focused = r.clicked;
    await sleep(randInt(80, 200));
  }
  if (args.clear) {
    // ⌘A/Ctrl+A over CDP doesn't trigger the editing command on every
    // platform, so select from the page, then delete with a real keypress.
    await evaluateInPage(
      tabId,
      `const el = document.activeElement;
       if (el && typeof el.select === "function") el.select();
       else document.execCommand("selectAll");`,
    );
    await sendKey(tabId, keyDef("Backspace"));
    await sleep(randInt(60, 160));
  }
  for (const ch of text) {
    if (ch === "\n") await sendKey(tabId, keyDef("Enter"));
    else if (ch === "\t") await sendKey(tabId, keyDef("Tab"));
    else await sendKey(tabId, keyDef(ch));
    // Mostly brisk, with the odd pause after a word.
    await sleep(ch === " " ? randInt(80, 220) : Math.max(25, randomNormal(85, 30)));
  }
  const value = await evaluateInPage(
    tabId,
    `const el = document.activeElement;
     return el ? { focused: el.tagName.toLowerCase() + (el.id ? "#" + el.id : ""),
                   value: "value" in el ? String(el.value).slice(0, 200) : (el.innerText || "").slice(0, 200) } : null;`,
  );
  return { typed: text.length, ...(focused ? { clicked: focused } : {}), ...(value ?? {}) };
}
