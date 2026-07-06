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
}
