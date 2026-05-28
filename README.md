# scrape-anything

LLM-generated web scraping from a Chrome extension. A Rust backend asks
Gemini to write a JavaScript function for a given page + intention; the
extension runs it in the tab via `chrome.userScripts` and returns the
extracted records. Works on any page; the bundled system-prompt example
uses Luma because its `__NEXT_DATA__` structure is a clean reference.

## Layout

```
scrape-anything/
├── extension/         # Chrome MV3 extension
└── scrape-backend/    # Rust (axum + sqlx-sqlite + genai) planner service
```

`scrape-backend/SPEC.md` documents the wire protocol and storage schema. Note
that the spec was written for an earlier agentic / Plan-DSL design; the
current implementation is a simpler one-shot codegen flow (the schema and
endpoints are still compatible).

## Run the backend

```sh
cd scrape-backend
GEMINI_API_KEY=… cargo run
```

Listens on `http://localhost:4546` and creates `scrape.db` next to the binary
on first run. Set `RUST_LOG=scrape_backend=debug` to see the full Gemini I/O
per turn; `RUST_LOG=scrape_backend=info` for quieter output.

## Install the extension

The extension is built with Vite + Tailwind v4 + `@crxjs/vite-plugin`. You
need Node 20+.

```sh
cd extension
npm install
npm run build      # produces extension/dist/
```

For iterative development, `npm run dev` keeps Vite watching: edits to
`popup.html`, `popup.css`, `popup.js`, etc. trigger an automatic rebuild and
the extension hot-reloads in Chrome.

Then:

1. Open `chrome://extensions`, enable **Developer mode** (top-right).
2. **Load unpacked** → select `extension/dist/` (not `extension/`).
3. On the extension's card, turn on **Allow User Scripts**.
   - This toggle only appears when Developer mode is on and the extension
     declares the `userScripts` permission. We need it because runtime code
     evaluation is otherwise blocked by Chrome and by most pages' CSPs.
4. Open any web page → click the extension → type what you want extracted →
   click **Plan & run**.

## Flow

```
[extension]                                            [backend]
   ├── collect DOM signals (dom-signals.js)
   ├── POST /api/sessions  ───────────────────────────▶
   │                                                     ├── Gemini writes a
   │                                                     │   ```js-action``` block
   ◀──────────  { kind: "code", code, rationale }  ──────┤
   ├── chrome.userScripts.execute(code)
   ├── render items + show generated code
   ├── POST /api/sessions/:id/result  ────────────────▶ (telemetry / cache)
   └── download JSON / CSV
```

The backend persists every session and turn in SQLite. When a session produces
code, it's cached in the `plans` table keyed by `(host, route_pattern,
intention_hash)`; future runs against pages on the same route template with
the same intention can reuse the cached code without another LLM call.

## Code contract

The model emits a single fenced block:

````
Brief one-line rationale.

```js-action
const data = JSON.parse(document.getElementById("__NEXT_DATA__").textContent);
const list = data?.props?.pageProps?.initialData?.data?.entries ?? [];
const items = list.map((e) => ({
  title: e.event?.name,
  url:   e.event?.url && new URL(e.event.url, "https://lu.ma/").href,
  start: e.event?.start_at,
}));
return { kind: "listing", items };
```
````

The extension wraps the body in an IIFE and runs it through
`chrome.userScripts.execute`. The block must end with `return { kind, items }`
where `kind` is `"listing"` or `"event"` and `items` is an array of plain
objects.

## Layout details

### `extension/`

- `package.json` / `vite.config.js` — build pipeline (Vite +
  `@crxjs/vite-plugin` + Tailwind v4).
- `manifest.json` — MV3, requests `activeTab`, `scripting`, `downloads`,
  `storage`, `userScripts`, broad host access; declares `dom-signals.js`
  as a `web_accessible_resource` so it ends up in the bundle.
- `popup.html` / `popup.css` / `popup.js` — the popup UI; drives the
  collect-signals → POST → run-code → download cycle. Tailwind classes
  on the static markup; `popup.css` defines a few component classes
  (`.turn`, `.result-line`) via `@apply` for the JS-built transcript.
- `dom-signals.js` — content script that builds the page fingerprint
  (`__NEXT_DATA__`, JSON-LD, meta, candidate selectors, samples, DOM
  skeleton) under a 240 KB budget.
- `backend.js` — `fetch` wrapper around `http://localhost:4546`.
- `background.js` — empty MV3 service worker.
- `dist/` — Vite build output. **This is what you load in Chrome.**

### `scrape-backend/`

- `src/main.rs` — config load, sqlite pool, genai client, axum bind.
- `src/agent.rs` — single-shot Gemini call, parses the `js-action` fence.
- `src/session.rs` — session lifecycle, persistence, code cache upsert.
- `src/routes/sessions.rs` / `src/routes/plans.rs` — HTTP endpoints.
- `src/prompts/system.md` — the system prompt the model sees.
- `migrations/0001_initial.sql` — `sessions`, `turns`, `plans` tables.

## Safety notes

`chrome.userScripts` runs code in a context **without** the page's CSP. That
means the LLM-generated code can call `fetch`, read the page DOM, and so on.
The trust chain is: you → backend → Gemini → code. For local YOLO use this is
fine. For shared or cloud deployments, sandbox the runner (e.g. via an
`iframe` with strict CSP) or fall back to a constrained DSL — both paths are
sketched in `scrape-backend/SPEC.md`.
