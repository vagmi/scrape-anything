# scrape-anything

Agentic LLM web scraping from a Chrome extension. A Rust backend runs Gemini as
a small **function-calling agent**: it probes the page, writes and runs
JavaScript in the tab, and can *see* the page via a screenshot — then returns
the extracted records. It keeps a **per-origin memory** (a virtual file store in
SQLite) so a second run on the same site reuses what it learned. Extracted rows
are stored as **artifacts** (downloadable JSON/CSV), and a built-in **dashboard**
browses past sessions and their memory. Works on any page; the bundled
system-prompt example uses Luma because its `__NEXT_DATA__` is a clean reference.

## Layout

```
scrape-anything/
├── extension/         # Chrome MV3 extension
├── dashboard/         # Vite + Tailwind + htmx history UI (served by the backend)
└── scrape-backend/    # Rust (axum + sqlx-sqlite + genai) agent service
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

Listens on `http://localhost:4546`, serves the dashboard at
`http://localhost:4546/` (build it first — see below), and creates `scrape.db`
next to the binary on first run. Set `RUST_LOG=scrape_backend=debug` to see the
full Gemini I/O per turn; `RUST_LOG=scrape_backend=info` for quieter output.

## Build the dashboard

The dashboard is a small Vite + Tailwind v4 + htmx app; the backend serves its
static build and renders HTML fragments for it.

```sh
cd dashboard
npm install
npm run build      # produces dashboard/dist/, served at http://localhost:4546/
```

The backend serves `dashboard/dist` (path configurable via `[dashboard]
dist_path` in `config.toml`). Open `http://localhost:4546/` to browse past
sessions, their transcripts, downloadable artifacts, and the per-origin memory
files.

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
   click **Scrape**. Watch the agent's tool steps in the transcript; download
   the result as JSON/CSV, or browse it later in the dashboard.

## Flow

The backend runs one LLM step per call and drives a tool loop. **Server-side
tools** (`list_files`/`read_file`/`write_file`/`edit_file` over the per-origin
store) are resolved in the backend with no round-trip. **Extension-side tools**
(`execute_script`, `screenshot`) are handed back to the extension to run in the
live tab; the extension posts results and the loop resumes.

```
[extension]                                            [backend]
   ├── collect DOM signals (dom-signals.js)
   ├── POST /api/sessions  ───────────────────────────▶ agent step (Gemini + tools)
   │                                                     ├── list/read/write/edit
   │                                                     │   resolved server-side
   ◀────────  { kind: "tool_call", tool_calls }  ────────┤ (execute_script / screenshot)
   ├── run tools in the tab (userScripts / captureVisibleTab)
   ├── POST /api/sessions/:id/tool_result  ───────────▶ resume; store any {kind,items}
   │            … loop until …                           artifact
   ◀────────────  { kind: "message", text }  ────────────┤ terminal summary
   └── download JSON / CSV (served from the stored artifact)
```

The backend persists every session and turn in SQLite. Extracted rows are
upserted into the `artifacts` table (one per session, latest wins). The agent's
`write_file`/`read_file` calls hit the `origin_files` table, a per-host virtual
filesystem that persists across sessions — that's how a repeat run on a site
reuses a scraper or notes it saved earlier.

## Tools

| tool | where it runs | purpose |
|---|---|---|
| `list_files` / `read_file` / `write_file` / `edit_file` | backend (SQLite) | the agent's persistent per-origin memory |
| `execute_script` | extension (`chrome.userScripts`) | run JS in the page — probe structure and do the final extraction |
| `screenshot` | extension (`captureVisibleTab`) | send the page image to Gemini (multimodal) when structure is unclear |

`edit_file` uses exact-once search-and-replace with rich recovery errors (ported
from the slop coding agent's `edit_ops.rs`).

## Extraction contract

The agent's `execute_script` code is the body of an `async` function (the
extension wraps it in `(async function(){ … })()` and runs it via
`chrome.userScripts.execute`). The **final** extraction must
`return { kind, items }` where `kind` is `"listing"` or `"event"` and `items`
is an array of plain objects. The last such result becomes the session's
downloadable artifact.

```js
const data = JSON.parse(document.getElementById("__NEXT_DATA__").textContent);
const list = data?.props?.pageProps?.initialData?.data?.entries ?? [];
const items = list.map((e) => ({
  title: e.event?.name,
  url:   e.event?.url && new URL(e.event.url, "https://lu.ma/").href,
  start: e.event?.start_at,
}));
return { kind: "listing", items };
```

## Layout details

### `extension/`

- `package.json` / `vite.config.js` — build pipeline (Vite +
  `@crxjs/vite-plugin` + Tailwind v4).
- `manifest.json` — MV3, requests `activeTab`, `scripting`, `downloads`,
  `storage`, `userScripts`, broad host access; declares `dom-signals.js`
  as a `web_accessible_resource` so it ends up in the bundle.
- `popup.html` / `popup.css` / `popup.js` — the popup UI; drives the
  collect-signals → tool-loop → download cycle. Tailwind classes on the static
  markup; `popup.css` defines component classes (`.turn`, `.turn.tool`) via
  `@apply` for the JS-built transcript.
- `tool-runner.js` — runs the extension-side tools: `execute_script` (via
  `chrome.userScripts`) and `screenshot` (via `chrome.tabs.captureVisibleTab`).
- `dom-signals.js` — content script that builds the page fingerprint
  (`__NEXT_DATA__`, JSON-LD, meta, candidate selectors, samples, DOM
  skeleton) under a 240 KB budget.
- `backend.js` — `fetch` wrapper around `http://localhost:4546`.
- `background.js` — empty MV3 service worker.
- `dist/` — Vite build output. **This is what you load in Chrome.**

### `dashboard/`

- Vite + Tailwind v4 + htmx static app. `index.html` is the shell; the backend
  renders HTML fragments (`/ui/sessions`, `/ui/sessions/:id`,
  `/ui/origins/:host/files`) that htmx swaps in. `src/styles.css` uses
  `@source "../../scrape-backend/src"` so Tailwind scans the Rust `maud`
  fragments for the utility classes they emit.

### `scrape-backend/`

- `src/main.rs` — config load, sqlite pool, genai client, axum bind.
- `src/agent.rs` — one function-calling Gemini step; replays the turn log
  (assistant tool calls, tool responses, screenshot images) into the chat.
- `src/session.rs` — session lifecycle, the tool loop, artifact upsert.
- `src/tools.rs` — the 6 `Tool`s + server-side file-store dispatch.
- `src/edit_ops.rs` — exact-once `string_replace` for `edit_file` (ported from slop).
- `src/csv.rs` — CSV serializer for artifact downloads.
- `src/routes/` — `sessions.rs`, `plans.rs`, `data.rs` (list/artifact/files
  JSON + downloads), `dashboard.rs` (maud fragments).
- `src/prompts/system.md` — the agent's system prompt.
- `migrations/0001_initial.sql` — `sessions`, `turns`, `plans` tables.
- `migrations/0002_agent_store.sql` — `origin_files` (memory) + `artifacts` (rows).

## Safety notes

`chrome.userScripts` runs code in a context **without** the page's CSP. That
means the LLM-generated code can call `fetch`, read the page DOM, and so on.
The trust chain is: you → backend → Gemini → code. For local YOLO use this is
fine. For shared or cloud deployments, sandbox the runner (e.g. via an
`iframe` with strict CSP) or fall back to a constrained DSL — both paths are
sketched in `scrape-backend/SPEC.md`.
