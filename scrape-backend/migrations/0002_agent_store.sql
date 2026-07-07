-- Agent persistent memory + extracted-result artifacts.
--
-- origin_files: a per-host virtual filesystem the agent reads/writes via the
-- list/read/write/edit tools. Persists across sessions so a second run on the
-- same site can reuse a scraper / notes it wrote earlier.
--
-- artifacts: the extracted rows a session produced (from an execute_script
-- result shaped { kind, items: [...] }). One per session (latest wins); serves
-- the JSON/CSV downloads in the popup and dashboard.

CREATE TABLE origin_files (
  origin     TEXT NOT NULL,           -- host, e.g. 'lu.ma'
  path       TEXT NOT NULL,           -- virtual path, e.g. 'discover/scraper.js'
  content    TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (origin, path)
);

CREATE TABLE artifacts (
  id         TEXT PRIMARY KEY,        -- ulid
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL,           -- 'listing' | 'event'
  item_count INTEGER NOT NULL,
  items_json TEXT NOT NULL,           -- full item array
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX artifacts_session_uidx ON artifacts(session_id);
