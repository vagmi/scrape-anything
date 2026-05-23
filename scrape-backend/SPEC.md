# scrape-backend — Spec

Status: **draft, pre-implementation**. This locks the protocol between the Chrome
extension (`../extension/`) and the backend service before any axum code is
written.

## 1. Overview

The extension produces a *structural fingerprint* of the current page plus a
user-supplied **intention** ("get all events with title, url, date"). The
backend, using an LLM (Gemini `gemini-3-flash-preview` via `genai`), works through an
**agentic loop**: it can call back into the extension to probe the page, and
eventually emits a **Plan** in a fixed DSL. The extension's interpreter
executes the plan against the live DOM and downloads the result.

Key properties:

- **Agentic, not one-shot.** The LLM can request more information (selector
  probes, JSON path queries, listing-vs-event disambiguation) before
  committing to a plan.
- **Code-free output.** The LLM never returns executable JavaScript. It
  returns either a *tool call* or a final *plan* JSON object. The extension
  has a hand-written interpreter; nothing from the LLM is `eval`'d.
- **Cacheable.** Final plans are stored in SQLite keyed by
  `(host, route_pattern, intention_hash)` and reused without an LLM round-trip.
- **Local-only for v1.** Backend binds to `http://localhost:4546`. No auth.
  CORS allows `chrome-extension://*`.

## 2. Glossary

| Term | Meaning |
|---|---|
| **Signals** | Compact structural summary of the page produced by the extension (JSON-LD, `__NEXT_DATA__`, meta, DOM skeleton, sampled cards). |
| **Intention** | Free-form user prompt describing what they want extracted. |
| **Session** | An agentic conversation — one intention, one starting page, many turns. |
| **Turn** | One LLM step. Yields either a tool call, a clarification, or a final plan. |
| **Plan** | The DSL document the extension's interpreter executes. |
| **Probe** | A read-only tool call from the LLM to the extension (e.g., "match this selector and show me 3 outerHTMLs"). |

## 3. End-to-end flow

```
extension                                                backend
─────────                                                ───────
  ├── collect signals
  ├── POST /sessions  { url, intention, signals }  ─────▶
  │                                                       ├── create session
  │                                                       ├── run LLM turn 1
  ◀─────────────────────────────  { turn: tool_call }  ───┤
  ├── execute probe locally
  ├── POST /sessions/:id/tool_result { result }  ────────▶
  │                                                       ├── run LLM turn 2
  ◀─────────────────────────────  { turn: plan }  ────────┤
  ├── run plan via interpreter
  ├── POST /sessions/:id/result { items, errors }  ──────▶
  │                                                       └── cache + telemetry
  └── download JSON/CSV
```

For cached cases the extension first calls `GET /plans/lookup?...`; on a hit it
skips session creation.

## 4. Extension → backend: signals contract

The extension's `dom-signals.js` content script returns this object. The
backend treats every field as optional but advisory — missing fields are not
errors.

```jsonc
{
  "url": "https://lu.ma/discover",
  "title": "Discover events on Luma",
  "meta": {
    "description": "…",
    "og:title": "…",
    "og:type": "website"
    // …all <meta> name|property attributes
  },
  "json_ld": [
    // raw parsed JSON-LD blocks, deduped, capped at 32 KB total
  ],
  "next_data": {
    // contents of <script id="__NEXT_DATA__"> verbatim, capped at 64 KB
    // (truncation marker added if exceeded — see §10)
  },
  "skeleton": {
    // top-of-tree summary. Each node: { tag, id?, class?, data?, n_children, text_len }
    // depth-capped at 8, breadth-capped at 40 siblings per parent
    "tag": "body",
    "children": [ /* … */ ]
  },
  "candidates": [
    // selector → match count, for selectors the extension considers likely "item" lists
    { "selector": "a[href^='/']:has(time)", "count": 42 },
    { "selector": "article",                 "count": 0  },
    { "selector": "li[class*='card']",       "count": 12 }
  ],
  "samples": [
    // outerHTML of up to 3 representative repeating elements, each capped at 8 KB
    { "selector": "a[href^='/']:has(time)", "html": "<a …>…</a>" }
  ],
  "viewport": { "width": 1280, "height": 800 },
  "extension_version": "0.2.0"
}
```

Size budget: total payload **≤ 256 KB** after JSON encoding. The extension
truncates aggressively rather than dropping fields silently — every truncation
produces a `"__truncated": true` marker on the parent object so the LLM knows
to probe rather than assume.

## 5. Agentic protocol

### 5.1 `POST /api/sessions`

Create a new session.

```jsonc
// Request
{
  "url": "https://lu.ma/discover",
  "intention": "list all upcoming tech events with title, url, host, start time",
  "signals": { /* §4 */ }
}

// Response
{
  "session_id": "01J…",        // ulid
  "turn": { /* §5.3 */ }       // first turn, ready for the extension to act on
}
```

### 5.2 `POST /api/sessions/:id/tool_result`

Reply to a tool call.

```jsonc
// Request
{
  "tool_call_id": "tc_…",
  "result": { /* shape defined per-tool, §6 */ },
  "error": null                 // or { code, message } if the probe failed
}

// Response
{ "turn": { /* §5.3 */ } }      // next turn
```

### 5.3 Turn envelope

Every turn the backend hands back is one of three shapes, discriminated by `kind`:

```jsonc
// 5.3.a tool_call — extension must run the probe and reply via tool_result
{
  "kind": "tool_call",
  "tool_call_id": "tc_…",
  "tool": "query_selector_all",     // §6 catalog
  "args": { /* tool-specific */ },
  "rationale": "checking whether 'article' nodes carry event metadata",
  "turn_index": 2,
  "tokens_used": 1834               // running session total, for the UI
}

// 5.3.b clarify — ask the user a question, surfaced in the popup
{
  "kind": "clarify",
  "question": "Should I include past events too, or only upcoming?",
  "options": ["upcoming only", "include past"],   // optional
  "turn_index": 3
}

// 5.3.c plan — terminal. Extension runs it.
{
  "kind": "plan",
  "plan": { /* §7 */ },
  "turn_index": 5,
  "tokens_used": 4210
}

// 5.3.d give_up — terminal failure
{
  "kind": "give_up",
  "reason": "Page has no event listings visible; signals only show a sign-in wall.",
  "turn_index": 2
}
```

### 5.4 `POST /api/sessions/:id/clarify`

User-supplied answer to a `clarify` turn.

```jsonc
// Request
{ "answer": "upcoming only" }
// Response
{ "turn": { /* next turn */ } }
```

### 5.5 `POST /api/sessions/:id/result`

Telemetry after the extension runs the plan. Drives cache scoring and the
refinement loop.

```jsonc
{
  "items_count": 42,
  "errors": [],                            // selector-resolution errors
  "field_completeness": {                  // % non-null per field
    "title": 1.0, "url": 1.0, "start": 0.92
  },
  "sample_failures": [                     // up to 3 items that came out malformed
    { "index": 7, "field": "start", "reason": "parse_date failed", "raw": "Tomorrow" }
  ]
}
```

If `items_count == 0` or `field_completeness` for any field is below a configurable
threshold (default 0.5), the backend may emit a follow-up turn with a revised
plan (refinement loop, capped at 3 revisions per session).

### 5.6 `GET /api/sessions/:id`

Inspect session state and full turn history (for debugging the popup).

### 5.7 `GET /api/plans/lookup`

```
GET /api/plans/lookup?host=lu.ma&route=/discover&intention_hash=…
```

Returns a cached plan if one exists with success_count > 0 and not invalidated.
Allows the extension to skip session creation on warm paths.

### 5.8 Limits

- Max turns per session: **50** (hard cap on the agentic loop; configurable).
- Max active sessions per origin: **8**.
- Session TTL: 30 minutes idle, deleted after.
- **YOLO mode:** the extension runs every tool call the LLM emits automatically;
  no user approval step. The popup shows a live ticker of the turn index and
  current tool name so the user can watch progress (and cancel).

## 6. Tool catalog

Tools are read-only probes the LLM can call against the extension. Each tool
has a strict input/output JSON schema; the backend validates LLM calls and
extension responses against it.

### 6.1 `query_selector_all`

Match a CSS selector and return structural data + a few outerHTML samples.

```jsonc
// args
{ "selector": "article.event-card", "limit": 5, "include_html": true }

// result
{
  "count": 14,
  "samples": [
    { "outer_html": "<article …>…</article>", "text": "…", "children_tags": ["h3","time","img"] }
  ]
}
```

### 6.2 `read_json_path`

Query the on-page structured data the extension already serialized.

```jsonc
// args
{ "source": "next_data" | "json_ld", "path": "props.pageProps.events[0]" }

// result
{ "value": <any>, "value_type": "object", "preview": "…" }
```

Path syntax: dotted with bracket index (e.g., `props.a.b[0].c`). Returns
`{ "value": null, "missing": true }` if absent.

### 6.3 `list_attributes`

For a selector, summarize what attributes / data-* keys appear across matches.
Helpful for the LLM to spot id-bearing attributes without dumping all HTML.

```jsonc
// args
{ "selector": "a[href^='/']" }

// result
{
  "count": 87,
  "attribute_frequency": { "href": 87, "aria-label": 84, "data-event-id": 80 },
  "sample_values": { "data-event-id": ["abc123", "def456"] }
}
```

### 6.4 `probe_plan`

Dry-run a candidate plan and report stats without downloading. Lets the LLM
verify a plan before committing.

```jsonc
// args
{ "plan": { /* §7 */ } }

// result
{
  "items_count": 42,
  "field_completeness": { "title": 1.0, "url": 1.0, "start": 0.92 },
  "first_items": [ /* up to 3 fully-extracted items */ ],
  "errors": []
}
```

### 6.5 `fetch_subpage` *(Phase 4, deferred)*

Open a same-origin URL in a background tab, collect signals, return them.
Listed here so the protocol is forward-compatible.

## 7. Plan DSL

The interpreter only needs to handle what's specified here. Anything else is a
validation error.

```ts
type Plan = {
  plan_id: string;             // server-assigned ulid
  kind: "listing" | "event";
  source: "next_data" | "json_ld" | "dom";
  items?: ItemsLocator;        // listing only
  fields: Field[];             // event: page-root; listing: per-item
  post_processors?: Op[];
  rationale: string;           // human-readable, shown in the popup
};

type ItemsLocator =
  | { path: string }            // next_data / json_ld
  | { selector: string };       // dom

type Field =
  | {                           // structured-data field
      name: string;
      type?: "string" | "url" | "datetime" | "number" | "bool";
      path: string;
      resolve_against?: string; // base URL for relative URLs
      default?: unknown;
    }
  | {                           // DOM field
      name: string;
      type?: "string" | "url" | "datetime" | "number" | "bool";
      selector: "self" | string;
      extract: "text" | "html" | `attr:${string}`;
      resolve_url?: boolean;    // resolve href against location
      default?: unknown;
    };

type Op =
  | { field: string; op: "trim" | "lower" | "upper" }
  | { field: string; op: "regex_replace"; pattern: string; replacement: string }
  | { field: string; op: "parse_date" }       // ISO 8601 output
  | { field: string; op: "coerce"; to: "number" | "bool" };
```

Forbidden in the LLM's output: anything not in the grammar above. The backend
strictly validates with a JSON Schema and retries the LLM **once** with the
validation error if the first emission fails. A second failure becomes a
`give_up` turn.

## 8. Storage (SQLite)

One database file (`scrape.db`), default at `./scrape.db`, configurable.

```sql
CREATE TABLE sessions (
  id           TEXT PRIMARY KEY,           -- ulid
  url          TEXT NOT NULL,
  host         TEXT NOT NULL,
  route        TEXT NOT NULL,              -- normalized pathname
  intention    TEXT NOT NULL,
  intention_hash TEXT NOT NULL,            -- sha256(intention) hex
  status       TEXT NOT NULL,              -- 'active' | 'completed' | 'gave_up' | 'expired'
  created_at   INTEGER NOT NULL,           -- unix ms
  updated_at   INTEGER NOT NULL,
  total_tokens INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE turns (
  id           TEXT PRIMARY KEY,           -- ulid
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  turn_index   INTEGER NOT NULL,
  role         TEXT NOT NULL,              -- 'extension' | 'llm' | 'user'
  kind         TEXT NOT NULL,              -- 'signals' | 'tool_call' | 'tool_result' | 'plan' | 'clarify' | 'clarify_answer' | 'result' | 'give_up'
  payload      TEXT NOT NULL,              -- JSON
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  created_at   INTEGER NOT NULL,
  UNIQUE(session_id, turn_index)
);
CREATE INDEX turns_session_idx ON turns(session_id);

CREATE TABLE plans (
  id              TEXT PRIMARY KEY,        -- ulid
  host            TEXT NOT NULL,
  route_pattern   TEXT NOT NULL,           -- normalized pathname pattern (see §9)
  intention_hash  TEXT NOT NULL,
  plan_json       TEXT NOT NULL,
  rationale       TEXT NOT NULL,
  success_count   INTEGER NOT NULL DEFAULT 0,
  failure_count   INTEGER NOT NULL DEFAULT 0,
  invalidated     INTEGER NOT NULL DEFAULT 0, -- 0|1
  created_at      INTEGER NOT NULL,
  last_used_at    INTEGER NOT NULL,
  source_session  TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  UNIQUE(host, route_pattern, intention_hash)
);
CREATE INDEX plans_lookup_idx ON plans(host, route_pattern, intention_hash, invalidated);
```

Plan invalidation: after 3 consecutive failures (no items extracted) within a
24h window, `invalidated = 1` and lookups stop returning it. The next session
on that route will generate a fresh plan and supersede it.

## 9. Route normalization

To cache plans across pages of the same template, the backend canonicalizes the
pathname before hashing:

- Lowercase.
- Replace UUID-like segments (`[0-9a-f-]{16,}`) with `:id`.
- Replace numeric segments (`\d+`) with `:n`.
- Collapse trailing slash.

Examples:
- `/event/abc-123-def` → `/event/:id`
- `/u/42/calendar`     → `/u/:n/calendar`
- `/discover`          → `/discover`

Site-specific overrides (e.g., Luma event slugs are not IDs but should still be
treated as wildcards) live in `config.toml` and ship with sensible defaults for
known hosts.

## 10. Truncation markers

When the backend or extension truncates a value, the truncated string/object is
replaced with:

```json
{ "__truncated": true, "original_size": 124583, "kept": "head" | "head_and_tail" }
```

The LLM is instructed in the system prompt to call `read_json_path` or
`query_selector_all` to drill into truncated regions rather than guess.

## 11. LLM prompt structure

System prompt (`prompts/system.md`):

1. Role description: "You design web-scraping plans for a Chrome extension…"
2. Available tools (JSON-schema-formatted, mirrors §6).
3. Output contract: every turn, emit exactly one of `tool_call` | `clarify` |
   `plan` | `give_up`, as a JSON object matching §5.3.
4. The Plan DSL grammar (§7), with examples.
5. Heuristics:
   - Prefer `next_data` > `json_ld` > `dom` when multiple work.
   - Always emit a `rationale`.
   - Probe before committing if the signals show `__truncated` anywhere
     relevant or if `candidates` has ambiguous selectors.
   - If the page looks gated (sign-in wall, empty body), `give_up` early.
6. Few-shot examples (3): one Luma listing via `next_data`; one event via
   JSON-LD; one DOM fallback with a probe-then-plan loop.

The LLM is called with `genai`'s tool-use API; tools are wired so the model
*must* call one of the four output kinds (enforced via response schema).

## 12. Configuration

`config.toml`:

```toml
[server]
host = "127.0.0.1"
port = 4546

[storage]
path = "./scrape.db"

[llm]
provider = "gemini"          # direct Gemini API; reads GEMINI_API_KEY from env
model = "gemini-3-flash-preview"
max_turns = 50               # hard cap on the agentic tool-call loop
max_refinements = 3

[cors]
allowed_origins = ["chrome-extension://*"]

[routes.lu_ma]
host = "lu.ma"
# Luma event slugs are single-segment; treat any single-segment path as :slug
patterns = [{ match = "^/[^/]+$", replace = "/:slug" }]
```

## 13. Error model

All errors return JSON:

```jsonc
{ "error": { "code": "INVALID_PLAN", "message": "…", "details": { … } } }
```

Codes:

- `SESSION_NOT_FOUND`
- `SESSION_EXPIRED`
- `TURN_LIMIT_EXCEEDED`
- `INVALID_TOOL_RESULT` — extension returned data not matching the tool schema
- `INVALID_PLAN` — LLM produced a plan that fails DSL validation twice
- `LLM_TIMEOUT`
- `LLM_UNAVAILABLE`
- `BAD_SIGNALS` — payload over 256 KB or missing required `url`

## 14. Phasing (revised)

- **P1 — Thin slice, no agentic.** `POST /sessions` returns a `plan` directly
  on turn 0 (no tool calls). Validates wiring end-to-end. Logs into SQLite.
- **P2 — Agentic loop.** Enable `tool_call` turns; implement `query_selector_all`,
  `read_json_path`, `probe_plan`, `list_attributes` in the extension.
- **P3 — Caching.** `GET /plans/lookup`; success/failure scoring; invalidation.
- **P4 — Refinement.** `result` telemetry triggers revision turns.
- **P5 — Sub-page traversal.** `fetch_subpage`; background tab orchestration.

## 15. Decisions (resolved)

- **LLM access:** direct Gemini API via `GEMINI_API_KEY` (no Vertex / gcp_auth).
- **Execution mode:** YOLO. The extension auto-executes every tool call. No
  per-turn user approval. The user can cancel a session from the popup.
- **Turn cap:** hard limit of **50** tool calls per session. On reaching the
  cap the backend emits a `give_up` turn with `reason = "turn_limit"`.
- **Log retention:** keep all turns in SQLite indefinitely for v1 (local-only,
  single user). Revisit if `scrape.db` grows uncomfortably large; rotation is
  a future concern, not a v1 feature.
