-- blindcoder store — append-only, event-sourced schema (M0: full schema, written once).
--
-- Design rules encoded here:
--   * Never UPDATE/DELETE the integrity-critical tables — corrections SUPERSEDE (a new row that
--     points at the one it replaces); the mistake stays visible. Triggers enforce this.
--   * Belief is a FOLD over `ratings`, not a mutable column.
--   * The default capture level is `metadata`: no prompts/code are stored here, only the
--     model <-> rating <-> cost <-> time signal the selector needs. Raw bytes (at `replay`) live
--     in disposable WARC files outside the DB, located by convention, never referenced by a row.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- Backends. `policy_source` records where the data-policy verdict came from ('api' | 'curated');
-- `verified_at` timestamps a hand-maintained (curated) assertion so it can expire (fail-closed).
CREATE TABLE IF NOT EXISTS providers (
  slug          TEXT PRIMARY KEY,
  base_url      TEXT NOT NULL,
  wire          TEXT NOT NULL DEFAULT 'openai',
  data_policy   TEXT,
  policy_source TEXT,
  verified_at   TEXT,
  created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Known models per provider. `real_slug` is the provider-native id (blind-key material).
CREATE TABLE IF NOT EXISTS catalog (
  canonical_key TEXT NOT NULL,
  provider_slug TEXT NOT NULL REFERENCES providers(slug),
  real_slug     TEXT NOT NULL,
  status        TEXT NOT NULL DEFAULT 'available',   -- 'available' | 'delisted'
  first_seen    TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (canonical_key, provider_slug)
);

-- Append-on-change price series. Non-regenerable upstream (APIs show only today's price), hence
-- authoritative data, not a cache.
CREATE TABLE IF NOT EXISTS price_history (
  canonical_key   TEXT NOT NULL,
  provider_slug   TEXT NOT NULL,
  observed_at     TEXT NOT NULL DEFAULT (datetime('now')),
  input_per_mtok  REAL,
  output_per_mtok REAL,
  PRIMARY KEY (canonical_key, provider_slug, observed_at)
);

-- The blind key: alias <-> real identity. Minted lazily for models actually used. Immutable once
-- minted. The backing DB file is gitignored; the app only crosses this mapping via the reveal gate.
CREATE TABLE IF NOT EXISTS aliases (
  alias          TEXT PRIMARY KEY,       -- provider_token:model_token
  provider_token TEXT NOT NULL,
  model_token    TEXT NOT NULL,
  canonical_key  TEXT NOT NULL,
  provider_slug  TEXT NOT NULL,
  minted_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Session identity + start facts (immutable). Lifecycle end is a separate append-only event so
-- the row itself is never updated.
CREATE TABLE IF NOT EXISTS sessions (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  alias            TEXT NOT NULL,
  cli              TEXT,
  capture_level    TEXT NOT NULL DEFAULT 'metadata',
  tuneables_json   TEXT,
  start_difficulty INTEGER,              -- nullable; captured only when known up front
  started_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

-- End-of-session metadata event (one per session). At the `metadata` floor this is the entire
-- cost signal: no prompt/code, just tokens + realized cost + error tag.
CREATE TABLE IF NOT EXISTS session_end (
  session_id        INTEGER PRIMARY KEY REFERENCES sessions(id),
  ended_at          TEXT NOT NULL DEFAULT (datetime('now')),
  realized_cost     REAL,
  prompt_tokens     INTEGER,
  completion_tokens INTEGER,
  error_kind        TEXT
);

-- Ratings as append-only events; the track record is a fold over these. Corrections supersede.
CREATE TABLE IF NOT EXISTS ratings (
  id                   INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id           INTEGER NOT NULL REFERENCES sessions(id),
  performance_points   INTEGER NOT NULL CHECK (performance_points BETWEEN -2 AND 2),
  difficulty_points    INTEGER NOT NULL CHECK (difficulty_points BETWEEN 0 AND 4),
  rated_at             TEXT NOT NULL DEFAULT (datetime('now')),
  supersedes_rating_id INTEGER REFERENCES ratings(id)
);

-- Append-only enforcement: block UPDATE/DELETE on the integrity-critical tables.
CREATE TRIGGER IF NOT EXISTS sessions_no_update BEFORE UPDATE ON sessions
  BEGIN SELECT RAISE(ABORT, 'append-only: sessions are immutable'); END;
CREATE TRIGGER IF NOT EXISTS sessions_no_delete BEFORE DELETE ON sessions
  BEGIN SELECT RAISE(ABORT, 'append-only: sessions are immutable'); END;

CREATE TRIGGER IF NOT EXISTS session_end_no_update BEFORE UPDATE ON session_end
  BEGIN SELECT RAISE(ABORT, 'append-only: session_end is immutable'); END;
CREATE TRIGGER IF NOT EXISTS session_end_no_delete BEFORE DELETE ON session_end
  BEGIN SELECT RAISE(ABORT, 'append-only: session_end is immutable'); END;

CREATE TRIGGER IF NOT EXISTS ratings_no_update BEFORE UPDATE ON ratings
  BEGIN SELECT RAISE(ABORT, 'append-only: ratings supersede, never edit'); END;
CREATE TRIGGER IF NOT EXISTS ratings_no_delete BEFORE DELETE ON ratings
  BEGIN SELECT RAISE(ABORT, 'append-only: ratings supersede, never delete'); END;

CREATE TRIGGER IF NOT EXISTS aliases_no_update BEFORE UPDATE ON aliases
  BEGIN SELECT RAISE(ABORT, 'append-only: the alias blind-key is immutable'); END;
CREATE TRIGGER IF NOT EXISTS aliases_no_delete BEFORE DELETE ON aliases
  BEGIN SELECT RAISE(ABORT, 'append-only: the alias blind-key is immutable'); END;

CREATE INDEX IF NOT EXISTS ratings_by_session ON ratings(session_id);
CREATE INDEX IF NOT EXISTS sessions_by_alias ON sessions(alias);
