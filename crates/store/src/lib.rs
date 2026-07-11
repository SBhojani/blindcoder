//! blindcoder **store** — append-only, event-sourced SQLite.
//!
//! The schema ([`SCHEMA`]) is the full M0 shape, written once; later milestones only *fill more
//! columns*, never restructure. Append-only semantics are enforced by triggers in the DB itself,
//! so a buggy caller cannot silently rewrite history.

use alias::Alias;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// The full append-only schema, applied on open (idempotent — every statement is `IF NOT EXISTS`).
pub const SCHEMA: &str = include_str!("schema.sql");

/// An effective rating with its age already resolved to days, ready to fold. Age is computed by
/// SQLite (`julianday('now') - julianday(rated_at)`) so the binary needs no datetime dependency and
/// the selector stays clock-free — the store owns the one clock, as it already does for row
/// timestamps.
#[derive(Clone, Debug, PartialEq)]
pub struct AgedRating {
    pub canonical_key: String,
    pub performance_points: f64,
    pub difficulty_points: f64,
    pub age_days: f64,
}

/// A resolved routing target for an alias: everything the transport needs to forward one request.
/// The proxy reaches this only through the reveal gate (reason: routing) — it is the single place
/// an alias becomes a real identity.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub canonical_key: String,
    pub provider_slug: String,
    pub real_slug: String,
    pub base_url: String,
    pub wire: String,
}

/// The latest known price for one model at one provider. `None` fields mean free/unpriced — a
/// zero-cost candidate in the pool.
#[derive(Clone, Debug, PartialEq)]
pub struct PricePoint {
    pub canonical_key: String,
    pub provider_slug: String,
    pub input_per_mtok: Option<f64>,
    pub output_per_mtok: Option<f64>,
}

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

    /// Like [`effective_ratings`](Self::effective_ratings) but with the age resolved to days in
    /// SQL, ready for the recency-decayed fold. Same supersede semantics — corrections collapse to
    /// the tip, no double-count.
    pub fn effective_ratings_aged(&self) -> Result<Vec<AgedRating>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.canonical_key,
                    r.performance_points,
                    r.difficulty_points,
                    julianday('now') - julianday(r.rated_at) AS age_days
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
            .query_map([], |r| {
                Ok(AgedRating {
                    canonical_key: r.get(0)?,
                    performance_points: r.get::<_, i64>(1)? as f64,
                    difficulty_points: r.get::<_, i64>(2)? as f64,
                    age_days: r.get::<_, f64>(3)?.max(0.0),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- pool seeding: providers, catalog, prices (mutable registries, not the append-only log) ---

    /// Insert or refresh a provider record (routing facts only; not identity-critical, so mutable).
    pub fn upsert_provider(&self, slug: &str, base_url: &str, wire: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO providers (slug, base_url, wire) VALUES (?1, ?2, ?3)
             ON CONFLICT(slug) DO UPDATE SET base_url = excluded.base_url, wire = excluded.wire",
            rusqlite::params![slug, base_url, wire],
        )?;
        Ok(())
    }

    /// Insert or refresh a model's provider-native slug in the catalog.
    pub fn upsert_model(&self, canonical_key: &str, provider_slug: &str, real_slug: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO catalog (canonical_key, provider_slug, real_slug) VALUES (?1, ?2, ?3)
             ON CONFLICT(canonical_key, provider_slug) DO UPDATE SET real_slug = excluded.real_slug",
            rusqlite::params![canonical_key, provider_slug, real_slug],
        )?;
        Ok(())
    }

    /// Append a price observation, but only when it differs from the latest — the series is
    /// append-on-change, so re-seeding an unchanged price adds no row.
    pub fn record_price_if_changed(
        &self,
        canonical_key: &str,
        provider_slug: &str,
        input_per_mtok: Option<f64>,
        output_per_mtok: Option<f64>,
    ) -> Result<()> {
        let latest: Option<(Option<f64>, Option<f64>)> = self
            .conn
            .query_row(
                "SELECT input_per_mtok, output_per_mtok FROM price_history
                 WHERE canonical_key = ?1 AND provider_slug = ?2
                 ORDER BY observed_at DESC, rowid DESC LIMIT 1",
                rusqlite::params![canonical_key, provider_slug],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        // Treat "no row yet" as equivalent to an all-unpriced observation, so seeding a free model
        // (both None) records nothing, and re-seeding an unchanged price is a no-op either way.
        if latest.unwrap_or((None, None)) == (input_per_mtok, output_per_mtok) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO price_history (canonical_key, provider_slug, input_per_mtok, output_per_mtok)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![canonical_key, provider_slug, input_per_mtok, output_per_mtok],
        )?;
        Ok(())
    }

    /// The latest price per (model, provider) — the price side of the candidate pool.
    pub fn latest_prices(&self) -> Result<Vec<PricePoint>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.canonical_key, p.provider_slug, p.input_per_mtok, p.output_per_mtok
             FROM price_history p
             JOIN (
                 SELECT canonical_key, provider_slug, MAX(observed_at) AS mx
                 FROM price_history GROUP BY canonical_key, provider_slug
             ) latest
               ON latest.canonical_key = p.canonical_key
              AND latest.provider_slug = p.provider_slug
              AND latest.mx = p.observed_at",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(PricePoint {
                    canonical_key: r.get(0)?,
                    provider_slug: r.get(1)?,
                    input_per_mtok: r.get(2)?,
                    output_per_mtok: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // --- the blind key: alias mint/read/resolve (append-only, minted lazily) ---

    /// The model-token already assigned to this `canonical_key` under *any* provider, if any. The
    /// same model shares one model-token across providers so cross-provider matching survives
    /// blinding (`x7k2:q4m9` vs `b3wp:q4m9`).
    pub fn model_token_for(&self, canonical_key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT model_token FROM aliases WHERE canonical_key = ?1 LIMIT 1",
                [canonical_key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The provider-token already assigned to this provider, if any (reused across its models).
    pub fn provider_token_for(&self, provider_slug: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT provider_token FROM aliases WHERE provider_slug = ?1 LIMIT 1",
                [provider_slug],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The alias already minted for this (model, provider), if any.
    pub fn alias_for(&self, canonical_key: &str, provider_slug: &str) -> Result<Option<Alias>> {
        Ok(self
            .conn
            .query_row(
                "SELECT provider_token, model_token FROM aliases
                 WHERE canonical_key = ?1 AND provider_slug = ?2",
                rusqlite::params![canonical_key, provider_slug],
                |r| Ok(Alias { provider_token: r.get(0)?, model_token: r.get(1)? }),
            )
            .optional()?)
    }

    /// Persist a freshly minted alias (immutable once written).
    pub fn insert_alias(&self, alias: &Alias, canonical_key: &str, provider_slug: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO aliases (alias, provider_token, model_token, canonical_key, provider_slug)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                alias.display(),
                alias.provider_token,
                alias.model_token,
                canonical_key,
                provider_slug
            ],
        )?;
        Ok(())
    }

    /// Resolve an alias display string to its real routing target — the reveal-gated crossing from
    /// blind identity to real slug + provider endpoint.
    pub fn resolve_route(&self, alias_display: &str) -> Result<Option<Route>> {
        Ok(self
            .conn
            .query_row(
                "SELECT a.canonical_key, a.provider_slug, c.real_slug, p.base_url, p.wire
                 FROM aliases a
                 JOIN catalog   c ON c.canonical_key = a.canonical_key AND c.provider_slug = a.provider_slug
                 JOIN providers p ON p.slug = a.provider_slug
                 WHERE a.alias = ?1",
                [alias_display],
                |r| {
                    Ok(Route {
                        canonical_key: r.get(0)?,
                        provider_slug: r.get(1)?,
                        real_slug: r.get(2)?,
                        base_url: r.get(3)?,
                        wire: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    // --- session lifecycle events ---

    /// Open a session row; returns its id. Start facts are immutable (append-only table).
    pub fn record_session_start(
        &self,
        alias_display: &str,
        cli: Option<&str>,
        tuneables_json: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (alias, cli, tuneables_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![alias_display, cli, tuneables_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record the terminal metadata event for a session (one per session, append-only).
    /// `terminated_by` mirrors the backend-crate `AbortReason::as_str()` (`None` = natural end).
    pub fn record_session_end(
        &self,
        session_id: i64,
        realized_cost: Option<f64>,
        prompt_tokens: Option<i64>,
        completion_tokens: Option<i64>,
        error_kind: Option<&str>,
        terminated_by: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_end
                 (session_id, realized_cost, prompt_tokens, completion_tokens, error_kind, terminated_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id,
                realized_cost,
                prompt_tokens,
                completion_tokens,
                error_kind,
                terminated_by
            ],
        )?;
        Ok(())
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
    fn seed_resolve_route_round_trips() {
        let s = Store::open_in_memory().unwrap();
        s.upsert_provider("prov-a", "http://prov-a.test/v1", "openai").unwrap();
        s.upsert_model("model-x", "prov-a", "prov-a/model-x").unwrap();
        let a = Alias { provider_token: "pt01".into(), model_token: "mt01".into() };
        s.insert_alias(&a, "model-x", "prov-a").unwrap();

        let route = s.resolve_route(&a.display()).unwrap().expect("route resolves");
        assert_eq!(route.real_slug, "prov-a/model-x");
        assert_eq!(route.base_url, "http://prov-a.test/v1");
        assert_eq!(route.canonical_key, "model-x");
        assert!(s.resolve_route("nope:nope").unwrap().is_none());
    }

    #[test]
    fn model_token_is_shared_across_providers() {
        // The same canonical_key under two providers must reuse one model-token so the blinded
        // aliases still reveal they are the same model.
        let s = Store::open_in_memory().unwrap();
        assert!(s.model_token_for("model-x").unwrap().is_none());
        let a = Alias { provider_token: "pt01".into(), model_token: "mt01".into() };
        s.insert_alias(&a, "model-x", "prov-a").unwrap();
        assert_eq!(s.model_token_for("model-x").unwrap().as_deref(), Some("mt01"));
        assert_eq!(s.provider_token_for("prov-a").unwrap().as_deref(), Some("pt01"));
    }

    #[test]
    fn prices_append_on_change_only() {
        let s = Store::open_in_memory().unwrap();
        s.record_price_if_changed("model-x", "prov-a", Some(0.55), Some(2.2)).unwrap();
        s.record_price_if_changed("model-x", "prov-a", Some(0.55), Some(2.2)).unwrap(); // unchanged → no-op
        let n: i64 = s
            .conn
            .query_row("SELECT count(*) FROM price_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "unchanged re-seed must not append");

        let latest = s.latest_prices().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].input_per_mtok, Some(0.55));
    }

    #[test]
    fn session_lifecycle_records_start_and_terminated_end() {
        let s = Store::open_in_memory().unwrap();
        let sid = s.record_session_start("x7k2:q4m9", Some("opencode"), None).unwrap();
        s.record_session_end(sid, Some(0.0), Some(1200), Some(340), None, Some("cost_cap"))
            .unwrap();
        let (cost, term): (Option<f64>, Option<String>) = s
            .conn
            .query_row(
                "SELECT realized_cost, terminated_by FROM session_end WHERE session_id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cost, Some(0.0));
        assert_eq!(term.as_deref(), Some("cost_cap"));
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
