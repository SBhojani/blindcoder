# Spec: `stats` subcommand — per-model leaderboard from the event store

**Status:** implemented
**Scope:** implement the currently-stubbed `stats` CLI subcommand. Read-only over the event store,
reusing the existing selector math. Respects the blind by default.

## Problem

Everything blindcoder records — ratings, costs, tokens, failures — is currently invisible. The only
readout is raw SQLite. `stats` is one of two stubbed subcommands (`src/main.rs`: `Cmd::Stats` prints
"lands in a later milestone" and exits 2). We now have real sessions in the store (successful runs,
ratings, and `too_large`/auth failures) with nothing to surface them.

## Goal

`blindcoder stats` prints a **per-model leaderboard** answering "which models are best per dollar,
and how much have they cost me" — the project's core question — **without breaking the blind**.

## The blind (read this first — it drives the design)

- The selector learns per **`canonical_key`** (the provider-neutral model identity). This is the row
  identity to aggregate on.
- **`canonical_key` and the real slug are identifying.** Printing them deblinds an ongoing evaluation
  (seeing "model X was great" biases your next rating). So they must **not** appear by default.
- Each model has a stable **blind display token** (the alias / `model_token`). `stats` shows *that*
  by default — you can compare rows without learning identities.
- The **reveal gate** (`crates/alias`: `RevealGate::reveal(..)`, `RevealReason`) is the single audited
  crossing point, and reveals are journaled (`reveals` table). Unmasking real model names in `stats`
  must go **through the gate**, so it is opt-in and recorded.

## Requirements

1. **Remove the stub.** `Cmd::Stats` runs the real command; drop it from the "later milestone" arm.
2. **Aggregate per model (`canonical_key`)** over all recorded sessions. Each row shows:
   - the **blind model token** (default) — never the real slug or `canonical_key` unless revealed;
   - **# sessions** and **# rated** sessions;
   - **avg performance** rating and **avg difficulty** (from effective, supersede-aware ratings);
   - **learned quality** — the selector's current belief: fold this model's effective, decayed ratings
     + failures via `fold_track_record_with_failures(..)` and show `TrackRecord::mean()`. Do **not**
     invent new rating math;
   - **value score** — `value_score(quality_guess, normalized_price, &tuneables)` with `quality_guess`
     = the track-record mean (deterministic, **not** a Thompson `draw_quality`) and `normalized_price`
     from `normalize_prices(..)` over the pool's latest prices. This is the "best per dollar" column;
   - **total cost** and **avg cost/session** (from `session_end.realized_cost`);
   - **total prompt/completion tokens**;
   - **failures**: count of sessions with a non-null `session_end.error_kind`, ideally broken down by
     kind (e.g. `too_large:2 auth:1`).
3. **Default sort = value score, descending** (best value first). Allow `--sort <col>` for at least
   `value`, `quality`, `cost`, `sessions` (and `--asc` to reverse). Reasonable defaults over
   completeness.
4. **Blind by default; `--reveal` to unmask.** Without `--reveal`, rows are keyed by the blind token.
   With `--reveal`, replace the blind token with the real model slug **via the reveal gate**
   (`RevealGate::reveal` with an appropriate `RevealReason`) so each unmasking is **journaled** to the
   `reveals` table. (Add a `RevealReason` variant if the existing ones don't fit; keep it minimal.)
5. **Read-only** except the reveal journal rows written under `--reveal`. `stats` must never mutate
   sessions/ratings/prices. Do not add `UPDATE`/`DELETE` to any integrity table (triggers forbid it).
6. **Empty/degenerate input is friendly:** no sessions → print a short "no sessions recorded yet"
   message and exit 0, not a panic or an empty crash. A model with sessions but zero ratings shows
   its cost/token/failure columns with blanks (or `—`) for the rating/quality columns.

## Non-goals

- No new rating, scoring, decay, or pricing math — **reuse** `selector` (`fold_track_record_with_failures`,
  `value_score`, `normalize_prices`) and the store's existing `effective_ratings_aged`,
  `effective_failures_aged`, `latest_prices`.
- No charts/TUI. A plain aligned text table is the deliverable. (`--json` output is an *optional*
  nice-to-have if trivial; skip if it complicates the core.)
- No per-session listing (that's a separate `sessions` command, not this spec).
- No config/network access — purely offline over the local DB.

## Suggested implementation (guidance, not prescriptive)

- New store read method(s) returning per-`canonical_key` aggregates by joining `sessions` →
  `aliases` (`sessions.alias` → `aliases.alias` → `canonical_key`) → `session_end` (cost/tokens/
  error_kind), plus counts. Keep the SQL in `crates/store` behind a typed struct; do the fold/value
  math in the command layer (the selector stays I/O- and clock-free — pass `age_days` from SQL like
  the existing loaders do).
- Reuse `effective_ratings_aged()` / `effective_failures_aged()` (already grouped by `canonical_key`)
  for the quality/value columns rather than re-deriving from raw rows.
- Put the command in `src/` alongside `run`/`rate`. Render an aligned table (mirror the existing CLI
  output style).
- Well-typed over stringly: a `StatsRow` struct, not tuples of strings, assembled then formatted.

## Acceptance criteria

- `blindcoder stats` on the real DB prints a per-model table, default-sorted by value desc, showing
  blind tokens (no real slugs / `canonical_key` visible).
- `blindcoder stats --reveal` shows real model slugs **and** writes a `reveals` row per unmasked model
  (verify a row lands in `reveals`).
- Columns are correct on a seeded DB: a unit test builds a small store with known sessions/ratings/
  failures/costs and asserts the aggregates (counts, avg rating, total cost, failure breakdown) and
  the value ordering.
- **Keep at least one test that opens a real file-backed store** (a temp-file DB), not only an
  in-memory `:memory:` double. Rationale: some SQLite behavior — notably `journal_mode`/WAL pragmas
  and migrations run inside a transaction — is a silent no-op on an in-memory DB and can hide a bug
  that only bites a real file. An all-in-memory test suite can be 100% green over a file-only crash.
- Empty DB prints the friendly message and exits 0.
- Build, tests, clippy, and `fmt --check` are all green.
- No stale "stats lands in a later milestone" claims remain (`src/main.rs` module doc, README,
  Milestones) — reconcile them to reflect that `stats` ships.

## Verification note for the reviewer

This session is captured (`capture_level = "replay"`) to `~/.local/state/blindcoder/wire/<sid>.warc`,
so the full transcript — how the change was designed, whether the blind invariant was respected, and
how it handled the workflow — will be reviewed, not just the resulting diff.
