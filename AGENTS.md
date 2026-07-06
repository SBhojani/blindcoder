# AGENTS.md

Canonical guidance for AI coding agents (and humans) working in this repository. This is the
one instructions file; if your tool reads a differently-named file, symlink it to this one
locally rather than committing another copy.

## What this project is

blindcoder is a blind, cost/quality-aware **router** for agentic coding CLIs: it secretly picks
a model, masks its identity, captures the session, and learns from a short rating which models
are best per dollar. It is a bandit + logging proxy, not an eval harness.

## Build, test, run

```sh
cargo build --workspace
cargo test  --workspace
cargo run -- simulate            # the M0 deliverable; --help for options
cargo fmt --all                  # rustfmt is the formatter
cargo clippy --workspace         # clippy is the linter
```

A Nix flake provides a reproducible dev shell (`nix develop`). Nix is for development/build
only — never a runtime requirement.

## Architecture — split by durability, not by feature

The permanent core is written for real now; only the I/O edge below one trait is stubbed and
grows in place. Nothing in the core knows the proxy/network exists.

- `crates/selector` — the crown jewel. Pure functions: Thompson draw, `value_score`, rating
  scoring, recency-decayed event fold. No I/O, no clock, no ambient randomness (callers pass an
  RNG). This is what `simulate` and the property tests exercise.
- `crates/store` — append-only, event-sourced SQLite. The full schema is written once; later
  work only fills more columns. Append-only is enforced by DB triggers.
- `crates/config` — TOML config with `flag > env > file > default` precedence; XDG paths.
- `crates/alias` — random stored masking tokens + the reveal gate.
- `crates/backend` — the load-bearing seam: the `Backend` transport trait. At M0 a trivial
  rewrite proxy; M1+ grow the *same* trait into a full tee + fail-closed privacy proxy.
- `src/` — the CLI binary and the `simulate` harness.

## Invariants — do not regress these

- **Masking is by random *stored* tokens, never a hash of the name** (the pool is small and
  known, so a hash is reversible).
- **The store is append-only.** Corrections supersede with a new row; never `UPDATE`/`DELETE`
  the integrity tables. The triggers will reject it — keep them.
- **Private by default, fail-closed.** Unknown data policy ⇒ excluded. Eligibility is a hard
  filter, not a soft score term.
- **Capture raw, capture early** at the transport layer (M1+); the default capture level stores
  no prompts or code.
- **The real enforcement guarantee is the type system** (a `VettedEndpoint`-style newtype that
  `forward()` alone accepts), not the wire log — the log *witnesses*, the types *guarantee*.

## Conventions

- Rust stable, `edition = 2021`; keep `cargo clippy` clean.
- Prefer pure, testable functions; property-test the selector math.
- Keep the selector free of I/O and global state.

## Commit / repo hygiene

- **No AI-assistant attribution or vendor names in commits or tracked files.** Keep the repo
  vendor-neutral so any agent or contributor can pick it up.
- Never commit user state: the SQLite DB (which contains the blind key), wire archives, real
  `config.toml`, or secrets. `.gitignore` covers these.
- Ship `config.example.toml`; keep the real config out of the tree.
