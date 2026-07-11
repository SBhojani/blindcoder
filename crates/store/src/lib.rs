//! blindcoder **store** — append-only, event-sourced SQLite.
//!
//! The schema ([`SCHEMA`]) is the full M0 shape, written once; later milestones only *fill more
//! columns*, never restructure. Append-only semantics are enforced by triggers in the DB itself,
//! so a buggy caller cannot silently rewrite history.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// The full append-only schema, applied on open (idempotent — every statement is `IF NOT EXISTS`).
pub const SCHEMA: &str = include_str!("schema.sql");

/// One *effective* rating: a rating event that has not been superseded by a later correction,
/// resolved to the model (`canonical_key`) it rated. This is the exact shape the belief-fold
/// consumes — the caller turns `rated_at` into an `age_days` against its own clock.
#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveRating {
    /// The model this rating is about (via the session's alias). Cross-provider identity key.
    pub canonical_key: String,
    /// Performance in `-2..=2`.
    pub performance_points: i64,
    /// Difficulty in `0..=4`.
    pub difficulty_points: i64,
    /// SQLite datetime text (`YYYY-MM-DD HH:MM:SS`); the caller computes recency decay from it.
    pub rated_at: String,
}

/// A handle to the authoritative event log.
pub struct Store {
    pub conn: Connection,
}

impl Store {
    /// Open (creating parent dirs and the file if needed) and apply the schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// An ephemeral in-memory store with the schema applied — used by tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Record a fresh rating for a session; returns the new rating id. `supersedes` points at the
    /// rating this one corrects (a chain `A ← B ← C` collapses to just `C` in the fold below), or
    /// `None` for an original rating.
    pub fn record_rating(
        &self,
        session_id: i64,
        performance_points: i64,
        difficulty_points: i64,
        supersedes: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO ratings
                 (session_id, performance_points, difficulty_points, supersedes_rating_id)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, performance_points, difficulty_points, supersedes],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Journal an alias unmask (append-only). Keeps a peek at a model's identity visible in
    /// history, since seeing it biases later ratings. `reason` should mirror the alias-crate
    /// `RevealReason` (`"user_requested"` | `"routing"`).
    pub fn record_reveal(
        &self,
        alias: &str,
        session_id: Option<i64>,
        reason: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO reveals (alias, session_id, reason) VALUES (?1, ?2, ?3)",
            rusqlite::params![alias, session_id, reason],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The effective rating set — the single source the belief-fold reads.
    ///
    /// A correction is a **new** rating row whose `supersedes_rating_id` points at the one it
    /// replaces; the superseded row stays in the table (append-only) but must not count. So a
    /// rating is *effective* iff no other rating supersedes it. This collapses a correction chain
    /// `A ← B ← C` to just `C`, so a corrected rating can never double-count in the fold.
    /// Returned oldest-first, joined to the model each rating is about.
    pub fn effective_ratings(&self) -> Result<Vec<EffectiveRating>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.canonical_key, r.performance_points, r.difficulty_points, r.rated_at
             FROM ratings r
             JOIN sessions s ON s.id = r.session_id
             JOIN aliases  a ON a.alias = s.alias
             WHERE r.id NOT IN (
                 SELECT supersedes_rating_id
                 FROM ratings
                 WHERE supersedes_rating_id IS NOT NULL
             )
             ORDER BY r.rated_at ASC, r.id ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(EffectiveRating {
                    canonical_key: row.get(0)?,
                    performance_points: row.get(1)?,
                    difficulty_points: row.get(2)?,
                    rated_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_applies_cleanly() {
        let s = Store::open_in_memory().unwrap();
        let n: i64 = s
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(n >= 7, "expected the full table set, got {n}");
    }

    #[test]
    fn ratings_are_append_only() {
        let s = Store::open_in_memory().unwrap();
        s.conn
            .execute("INSERT INTO sessions (alias) VALUES ('x7k2:q4m9')", [])
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO ratings (session_id, performance_points, difficulty_points) VALUES (1, 1, 2)",
                [],
            )
            .unwrap();
        // A correction must supersede, not edit — the UPDATE trigger fires.
        let edit = s
            .conn
            .execute("UPDATE ratings SET performance_points = 2 WHERE id = 1", []);
        assert!(edit.is_err(), "UPDATE on ratings should be blocked");
        let del = s.conn.execute("DELETE FROM ratings WHERE id = 1", []);
        assert!(del.is_err(), "DELETE on ratings should be blocked");
    }

    /// Seed one session whose alias resolves to `canonical_key`, returning the session id.
    fn seed_session(s: &Store, alias: &str, canonical_key: &str) -> i64 {
        s.conn
            .execute("INSERT INTO sessions (alias) VALUES (?1)", [alias])
            .unwrap();
        let sid = s.conn.last_insert_rowid();
        s.conn
            .execute(
                "INSERT INTO aliases
                     (alias, provider_token, model_token, canonical_key, provider_slug)
                 VALUES (?1, 'ptok', 'mtok', ?2, 'prov')",
                rusqlite::params![alias, canonical_key],
            )
            .unwrap();
        sid
    }

    #[test]
    fn supersede_does_not_double_count() {
        let s = Store::open_in_memory().unwrap();
        let sid = seed_session(&s, "x7k2:q4m9", "acme/model-x");
        let original = s.record_rating(sid, -1, 2, None).unwrap();
        // A correction supersedes the original — the fold must see only the correction.
        s.record_rating(sid, 2, 2, Some(original)).unwrap();

        let eff = s.effective_ratings().unwrap();
        assert_eq!(eff.len(), 1, "superseded original must be excluded");
        assert_eq!(eff[0].performance_points, 2);
        assert_eq!(eff[0].canonical_key, "acme/model-x");
    }

    #[test]
    fn supersede_chain_collapses_to_the_tip() {
        let s = Store::open_in_memory().unwrap();
        let sid = seed_session(&s, "x7k2:q4m9", "acme/model-x");
        let a = s.record_rating(sid, -2, 1, None).unwrap();
        let b = s.record_rating(sid, 0, 1, Some(a)).unwrap();
        let _c = s.record_rating(sid, 2, 1, Some(b)).unwrap();

        let eff = s.effective_ratings().unwrap();
        assert_eq!(eff.len(), 1, "A <- B <- C must collapse to only C");
        assert_eq!(eff[0].performance_points, 2);
    }

    #[test]
    fn effective_ratings_span_sessions_and_models() {
        let s = Store::open_in_memory().unwrap();
        let s1 = seed_session(&s, "aaaa:q4m9", "acme/model-x");
        let s2 = seed_session(&s, "bbbb:z9z9", "acme/model-y");
        s.record_rating(s1, 1, 0, None).unwrap();
        s.record_rating(s2, 2, 0, None).unwrap();

        let eff = s.effective_ratings().unwrap();
        assert_eq!(eff.len(), 2);
        let keys: Vec<&str> = eff.iter().map(|r| r.canonical_key.as_str()).collect();
        assert!(keys.contains(&"acme/model-x") && keys.contains(&"acme/model-y"));
    }

    #[test]
    fn reveals_are_append_only() {
        let s = Store::open_in_memory().unwrap();
        let sid = seed_session(&s, "x7k2:q4m9", "acme/model-x");
        let id = s
            .record_reveal("x7k2:q4m9", Some(sid), "user_requested")
            .unwrap();
        // A session-less reveal (e.g. the `reveal` subcommand) is allowed.
        s.record_reveal("x7k2:q4m9", None, "routing").unwrap();

        let edit = s
            .conn
            .execute("UPDATE reveals SET reason = 'x' WHERE id = ?1", [id]);
        assert!(edit.is_err(), "UPDATE on reveals should be blocked");
        let del = s.conn.execute("DELETE FROM reveals WHERE id = ?1", [id]);
        assert!(del.is_err(), "DELETE on reveals should be blocked");

        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM reveals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }
}
