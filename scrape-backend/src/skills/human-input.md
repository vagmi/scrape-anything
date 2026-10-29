# Skill: human input inside `execute_script`

Every `execute_script` gets a `human` object that drives the page with the
**real** mouse and keyboard: the pointer travels a curved, human-paced path to
its target, keys are typed one at a time with natural gaps, and the browser
sees genuine (`isTrusted`) events. Focus moves, default actions run, and
widgets that ignore script-made events — debounced autocompletes, Downshift /
Headless UI / MUI comboboxes, date pickers — respond as they do to a person.

Use it for **every** interaction. `el.click()`, `dispatchEvent(...)` and
setting `.value` are untrusted; many sites silently ignore them.

## API

All methods return promises — always `await` them. A *target* is a CSS
selector string, a DOM element you already hold, or
`{ selector?, text?, index? }` where `text` matches the
element's visible text (exact match preferred, smallest element wins, so you
get the option rather than its list) and `selector` scopes the search.

| Call | Does | Resolves to |
|---|---|---|
| `human.click(target, { button?, click_count? })` | Scroll into view, move, click | `{ clicked, x, y, warning? }` — `warning` if another element covered the point |
| `human.hover(target)` | Move over it, no click | `{ hovered, x, y }` |
| `human.type(target \| null, text, { clear? })` | Click the field (unless `null` → type into the focused element), optionally clear it, type `text` (`\n` = Enter) | `{ typed, focused, value }` |
| `human.press(key, { repeat? })` | Key or chord: `"Enter"`, `"Tab"`, `"Escape"`, `"Backspace"`, `"ArrowDown"`, `"Shift+Tab"`, a single character | `{ pressed, times }` |
| `human.scroll(dy)` / `human.scroll(target, { dy, dx? })` | Mouse-wheel scroll (over `target`'s container if given) | `{ scrolled, scrollX, scrollY }` |
| `human.pause(min?, max?)` | Random think-time, ms (default 250–800) | — |

Failures (no element matches, etc.) **throw** — catch them where you want to
recover, or let them surface as `script error: …`.

## Rules of thumb

- **Batch a logical step into one script.** Fill a whole section, pick its
  dropdown values, verify, and return a summary. That's one round-trip instead
  of ten, and the returned summary is all you need to read.
- **Wait for the UI, don't guess.** After an action that opens or loads
  something, poll for the element you need next (helper below) instead of a
  fixed sleep.
- **Verify, then return what you saw.** Read back the field values / rendered
  entries at the end and return them, so you know the step really landed.
- **Never navigate mid-script expecting to continue.** A click that loads a new
  page kills the script; that's fine as the *last* action (you get
  `{ navigated: true, to }`), but put follow-up work in the next script.
- **Keep each script under ~45 s.** Typing is human-paced (~10 chars/s), so
  split very long forms across a few scripts.
- `human` acts on the session's tab and brings it to the front while it works.

## Helper: wait for an element

```js
const waitFor = async (sel, { timeout = 4000, text } = {}) => {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    const els = [...document.querySelectorAll(sel)]
      .filter((e) => e.offsetParent !== null)
      .filter((e) => !text || e.innerText.toLowerCase().includes(text.toLowerCase()));
    if (els.length) return els;
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`timed out waiting for ${sel}${text ? ` containing "${text}"` : ""}`);
};
```

## Worked example: profile form with an autocomplete

Updating a profile: set the headline, pick a location from an autocomplete,
save, and confirm. One `execute_script`:

```js
const waitFor = async (sel, { timeout = 4000, text } = {}) => {
  const t0 = Date.now();
  while (Date.now() - t0 < timeout) {
    const els = [...document.querySelectorAll(sel)]
      .filter((e) => e.offsetParent !== null)
      .filter((e) => !text || e.innerText.toLowerCase().includes(text.toLowerCase()));
    if (els.length) return els;
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`timed out waiting for ${sel}${text ? ` containing "${text}"` : ""}`);
};

// 1. Plain text field: replace its content.
await human.type('textarea[name="bio"]', "CTO & Co-founder, Urai AI.", { clear: true });
await human.pause();

// 2. Autocomplete: clear the old chip if there is one, type, pick a suggestion.
const chipRemove = document.querySelector('[data-test="location-chip"] button[aria-label*="Remove"]');
if (chipRemove) await human.click(chipRemove);
await human.type('input[aria-autocomplete="list"]', "Fredericton");
const options = await waitFor('[role="option"]', { text: "Fredericton" });
const optionTexts = options.map((o) => o.innerText.trim());
await human.click({ selector: '[role="option"]', text: "Fredericton, NB" });
// Keyboard alternative: await human.press("ArrowDown"); await human.press("Enter");

// 3. Save and confirm it stuck.
await human.pause(300, 700);
await human.click({ selector: "button", text: "Save" });
await waitFor('[data-test="location-chip"]', { text: "Fredericton" });

return {
  optionsSeen: optionTexts.slice(0, 5),
  bio: document.querySelector('textarea[name="bio"]')?.value,
  location: document.querySelector('[data-test="location-chip"]')?.innerText.trim(),
};
```

The selectors are illustrative — probe the real page first (a short
`execute_script` returning the form's inputs, their `name`/`aria-*`/labels, and
the buttons' texts), then write the script against what you found.

## Worked example: multi-step modal ("Add experience")

```js
await human.click({ selector: "button", text: "Add experience" });
const [dialog] = await waitFor('[role="dialog"]');
const inDialog = (s) => `[role="dialog"] ${s}`;

await human.type(inDialog('input[name="company"]'), "Urai AI Corp.");
// Company pickers are autocompletes too: take the suggestion if offered,
// otherwise close the list so the typed value stands.
try {
  await waitFor('[role="option"]', { text: "Urai", timeout: 2500 });
  await human.click({ selector: '[role="option"]', text: "Urai AI" });
} catch { await human.press("Escape"); }

await human.type(inDialog('input[name="title"]'), "CTO & Co-founder");
// Native <select>: don't click it (its open menu is drawn by the browser, not
// the page). Focus it closed and type the option's text — that selects it and
// fires a real change.
document.querySelector(inDialog('select[name="startMonth"]')).focus();
await human.type(null, "January");
document.querySelector(inDialog('select[name="startYear"]')).focus();
await human.type(null, "2025");
await human.click(inDialog('input[type="checkbox"][name="current"]'));

await human.click({ selector: '[role="dialog"] button', text: "Save" });
// Wait for the modal to go away, then confirm the entry rendered.
const t0 = Date.now();
while (document.querySelector('[role="dialog"]') && Date.now() - t0 < 5000) {
  await new Promise((r) => setTimeout(r, 200));
}
const errors = [...document.querySelectorAll('[role="dialog"] [role="alert"], [role="dialog"] .error')]
  .map((e) => e.innerText.trim());
return {
  saved: !document.querySelector('[role="dialog"]'),
  errors,
  entry: [...document.querySelectorAll("li, article")]
    .find((e) => e.innerText.includes("Urai AI"))?.innerText.slice(0, 200) ?? null,
};
```

If `saved` is false, read `errors`, fix those fields in the next script (the
modal is still open with everything else filled in), and save again.

## Worked example: infinite scroll with the wheel

Some feeds only load on genuine wheel/scroll input. Harvest as you go:

```js
const seen = new Map();
for (let round = 0, stuck = 0; round < 30 && stuck < 4; round++) {
  for (const a of document.querySelectorAll("a.post-card")) {
    if (!seen.has(a.href)) seen.set(a.href, { url: a.href, title: a.innerText.trim() });
  }
  const before = seen.size;
  await human.scroll(Math.round(innerHeight * (0.6 + Math.random() * 0.3)));
  await human.pause(400, 900);
  stuck = seen.size > before ? 0 : stuck + 1;
}
return { kind: "listing", items: [...seen.values()] };
```

## When real input still isn't enough

Rare widgets resist even trusted events (canvas UIs, custom drag-and-drop).
Then, since your script runs in the page's own JS context, find the React props
on the element (`Object.keys(el).find((k) => k.startsWith("__reactProps$"))`)
and call the handler (`onChange`, `onSelect`, `onInputValueChange`) directly.
Prefer `human.*` first — it exercises the site's real validation and save
paths.
