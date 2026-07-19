# Spec: stricter, enforced lint baseline (and remove the one `#[allow]`)

**Status:** proposed
**Scope:** add a workspace lint configuration that is stricter than clippy's defaults, make the code
satisfy it, and remove the single `#[allow(clippy::too_many_arguments)]` by fixing it properly. No
behavior change.

## Problem

The workspace has **no lint configuration** — just clippy's defaults. There is exactly one lint
suppression in the tree: `#[allow(clippy::too_many_arguments)]` on `Store::record_session_end`
(`crates/store/src/lib.rs`), which *suppresses* the lint instead of fixing the 8-argument signature.
We want a deliberately stricter, **enforced** lint baseline, and the allow gone.

`rust-version` is `1.74`, so the Cargo `[lints]` / `[workspace.lints]` table is available.

## Goal

A curated, enforced stringency: lock in the cleanliness the repo already has, forbid unsafe, adopt a
small set of high-value extra lints, and remove the lone `#[allow]` by refactoring — all while the
workspace stays `clippy`-clean and every test passes.

## Requirements

1. **Workspace lint table.** Add `[workspace.lints]` to the root `Cargo.toml`, and opt every crate in
   with `[lints]\nworkspace = true` (root package + each `crates/*`). One source of truth.

2. **Forbid unsafe.** `[workspace.lints.rust] unsafe_code = "forbid"`. This project is pure safe Rust;
   `forbid` makes that a guarantee that cannot be locally re-allowed. (If any genuine `unsafe` exists,
   stop and flag it rather than downgrading to `deny`.)

3. **Deny clippy's default groups.** Promote the default-warn groups to **deny** so they can't rot:
   `[workspace.lints.clippy]` with `correctness`, `suspicious`, `complexity`, `perf`, `style` at
   `"deny"` (use `priority = -1` on group entries so individual lint overrides win). The repo is
   already clean on these, so this should require **no code changes** — it just locks the state in.
   `too_many_arguments` lives in `complexity`, so after this it is enforced (see requirement 5).

4. **Adopt a curated set of high-value extra lints** (these are *not* on by default). Enable each at
   `"warn"` or `"deny"` and **fix every resulting finding**. Recommended set (all low-noise, mechanical):
   - `uninlined_format_args`
   - `map_unwrap_or`
   - `explicit_iter_loop`
   - `semicolon_if_nothing_returned`
   - `manual_let_else`
   - `redundant_closure_for_method_calls`
   - `needless_pass_by_value`
   You may **drop an individual lint** from this list if enabling it produces subjective or low-value
   churn — but say which you dropped and why (one line each). **Do NOT** blanket-enable
   `clippy::pedantic` or `clippy::nursery`: they produce ~100 and ~87 findings here respectively, many
   subjective (cast precision/truncation on intentional casts, `must_use_candidate`,
   `missing_errors_doc`, `doc_markdown`, `too_many_lines`) — out of scope for this pass.

5. **Remove the `#[allow(clippy::too_many_arguments)]`** on `Store::record_session_end` and fix it
   properly: bundle the **seven terminal-event fields** into a `SessionEnd` params struct, keeping
   `session_id` as a separate key argument. Derive `Default` on the struct so the test seed-calls (most
   pass several `None`s) stay terse via `..Default::default()`. Update **all call sites** (there are
   ~8: one production caller in `src/run.rs`, the rest are test seeds in `src/run.rs`, `src/stats.rs`,
   and `crates/store/src/lib.rs`). Export `SessionEnd` from the `store` crate; import it where needed.
   The SQL and column order are unchanged. (This mirrors the existing `DriveParams` struct in
   `src/run.rs`.)

6. **No `#[allow]` left behind.** After this change there should be **zero** `#[allow(...)]` in the
   tree (grep to confirm). If a finding genuinely warrants an allow, that's a red flag for this spec —
   prefer fixing it; if truly unavoidable, use a **narrowly-scoped** `#[allow]` with a `// reason:`
   comment and call it out.

## Non-goals

- No blanket `pedantic`/`nursery`. No `missing_docs`/doc-lints pass. No cast-lint pass.
- No behavior change, no perf tuning, no new features, no schema change.
- No CI wiring (there is no CI); enforcement is via the lints table honored by `cargo clippy`.

## Suggested implementation (guidance, not prescriptive)

- The `SessionEnd` struct (in `crates/store/src/lib.rs`, next to the other public row structs, outside
  `impl Store`):
  ```rust
  #[derive(Debug, Default)]
  pub struct SessionEnd<'a> {
      pub realized_cost: Option<f64>,
      pub cost_source: Option<&'a str>,
      pub prompt_tokens: Option<i64>,
      pub completion_tokens: Option<i64>,
      pub error_kind: Option<&'a str>,
      pub error_status: Option<u16>,
      pub terminated_by: Option<&'a str>,
  }
  // pub fn record_session_end(&self, session_id: i64, end: SessionEnd<'_>) -> Result<()>
  ```
- Work group-by-group: add the lint table, run `cargo clippy --workspace`, fix what it flags, repeat.

## Acceptance criteria

- `cargo clippy --workspace` is **clean (zero warnings)** with the new lint table in place — i.e. the
  stricter config passes, not clippy's defaults.
- The `[workspace.lints]` table exists and every crate opts in (`workspace = true`).
- `unsafe_code = "forbid"` is set; the default groups are denied; the curated extra lints are enabled
  (minus any you justified dropping).
- `Store::record_session_end` takes `(session_id, SessionEnd)`, the `#[allow]` is gone, and **no
  `#[allow(...)]` remains anywhere** in the tree.
- `cargo build --workspace`, `cargo test --workspace`, and `cargo fmt --all -- --check` are all green.
- Behavior is unchanged: the change is lint config + a mechanical signature refactor only (no SQL,
  storage, selector, or control-flow changes).

## Verification note for the reviewer

This session is captured (`capture_level = "replay"`) to `~/.local/state/blindcoder/wire/<sid>.warc`,
so the full transcript — how the lint set was chosen, the refactor carried out, and whether any lint
was dropped with justification — will be reviewed, not just the resulting diff.
