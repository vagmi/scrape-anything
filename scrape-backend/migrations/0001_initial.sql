-- Initial schema for scrape-backend. See SPEC §8.

CREATE TABLE sessions (
  id             TEXT PRIMARY KEY,
  url            TEXT NOT NULL,
  host           TEXT NOT NULL,
  route          TEXT NOT NULL,
  intention      TEXT NOT NULL,
  intention_hash TEXT NOT NULL,
  status         TEXT NOT NULL,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  total_tokens   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE turns (
  id           TEXT PRIMARY KEY,
  session_id   TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  turn_index   INTEGER NOT NULL,
  role         TEXT NOT NULL,
  kind         TEXT NOT NULL,
  payload      TEXT NOT NULL,
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  created_at   INTEGER NOT NULL,
  UNIQUE (session_id, turn_index)
);

CREATE INDEX turns_session_idx ON turns(session_id);

CREATE TABLE plans (
  id              TEXT PRIMARY KEY,
  host            TEXT NOT NULL,
  route_pattern   TEXT NOT NULL,
  intention_hash  TEXT NOT NULL,
  plan_json       TEXT NOT NULL,
  rationale       TEXT NOT NULL,
  success_count   INTEGER NOT NULL DEFAULT 0,
  failure_count   INTEGER NOT NULL DEFAULT 0,
  invalidated     INTEGER NOT NULL DEFAULT 0,
  created_at      INTEGER NOT NULL,
  last_used_at    INTEGER NOT NULL,
  source_session  TEXT REFERENCES sessions(id) ON DELETE SET NULL,
  UNIQUE (host, route_pattern, intention_hash)
);

CREATE INDEX plans_lookup_idx ON plans(host, route_pattern, intention_hash, invalidated);
