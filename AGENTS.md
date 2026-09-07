# AGENTS.md

Canonical guidance for AI coding agents (and humans) working in this repository. This is the
one instructions file; if your tool reads a differently-named file, symlink it to this one
locally rather than committing another copy.

If a gitignored `AGENTS.local.md` is present, follow it too — it holds personal, machine-local
setup (local toolchain access, VCS workflow) that isn't a project requirement: @AGENTS.local.md

Committed loaders make that local file reach agents that do not auto-read `.local.md`: **opencode**
via `opencode.jsonc` (`instructions`), and **pi** via an auto-discovered extension at
`.pi/extensions/load-agents-local.ts` (it appends `AGENTS.local.md` to pi's system prompt). Both are
no-ops when you keep no local file. Note: because pi auto-discovers `.pi/extensions/`, that committed
extension runs automatically in your pi sessions here; it only reads `AGENTS.local.md`.

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

Requires a Rust stable toolchain (edition 2021). The repo ships a Nix flake providing a
reproducible dev shell (`nix develop`) — recommended for a matching toolchain, but not required:
plain rustup works too. Either way, Nix is dev/build only, never a runtime requirement.

The opt-in non-ZDR routing path (`privacy = "no-zdr"`) is always compiled in and stays dormant
unless configured; its consent chain is enforced entirely at runtime. Even when configured, such a
provider is pruned from the pool unless the run passes `--enable-pay-with-data`, so it can never
block or alter an ordinary run. Its tests run in the plain `cargo test --workspace` suite — there is
no feature flag to remember.

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
- `crates/backend` — the central seam: the `Backend` transport trait. At M0 a trivial
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

- **No AI-assistant attribution** in commits or tracked files.
- **Keep the repo vendor-neutral where the vendor is incidental.** Two cases, and only the second
  is a violation:
  - **Load-bearing — required, keep.** A `Privacy` protocol is *defined by* its vendor: the variants
    are named for them and `matches_endpoint` validates a configured `base_url` against a hard-coded
    host (`crates/config/src/lib.rs`: `Privacy::OpenRouter => Some("openrouter.ai")`,
    `Privacy::Groq => Some("api.groq.com")`). That check is fail-closed and load-bearing, and the
    same names must appear in `config.example.toml` and the protocol docs or the feature is
    unusable. Manual-setup text naming a vendor's console is the same case.
  - **Incidental — use placeholders.** Example and test **model slugs**, and the `provider` field in
    a fixture response body, are opaque to the code under test: nothing branches on them. Use
    `example/model-a`, `example/model-b`, … and `example-provider`. A real slug there dates the
    repo, implies an endorsement, and invites someone to copy it as a working default.

  **The sharpest case is a comment that cites where a finding came from.** This is a public repo, so
  "recovered from real `<model>` transcripts" or "cache hits up to 99.8% (`<model>`)" publishes *the
  operator's own usage* — which models they run, that they capture wire traffic, and measurements
  taken from their private session DB. Keep the engineering rationale, drop the identity: "recovered
  from real provider transcripts" justifies the code just as well. Treat this as a privacy rule, not
  a style one.

  The narrower rule that the **non-ZDR** path names no vendor or model anywhere (source, tests,
  examples, spec) still holds absolutely — that one protects blindness, not neutrality. See
  `docs/specs/non-zdr-pay-with-data-routing.md`.
- Never commit user state: the SQLite DB (which contains the blind key), wire archives, real
  `config.toml`, or secrets. `.gitignore` covers these.
- Ship `config.example.toml`; keep the real config out of the tree.
