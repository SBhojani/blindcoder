# blindcoder

A **blind, cost/quality-aware router** for agentic coding CLIs.

Every coding session, blindcoder secretly picks a model from your pool, **masks its
identity** for the duration, and learns from a short end-of-session rating which models give
you the best quality *per dollar* on your actual mix of work. It is a cost/quality-aware
multi-armed bandit sitting in a small proxy one layer below the CLI, so it works with any
OpenAI-compatible agentic tool.

The point is to judge models on results, not reputation: you rate the work without knowing
who did it, and over time the router sends more of your work to whatever quietly performs.

> **Status: early (milestone M0).** The permanent decision core — the selector, the
> append-only store schema, the config surface, and the aliasing — is real and tested, and
> [`simulate`](#simulate) (the offline convergence harness) validates the selector. `run` now
> stands up a **streaming forwarding proxy**: it makes a blind pick, proxies your session to the
> chosen model (rewriting the model on the wire, streaming responses straight back, enforcing a
> cost cap), and records it; `rate` records your feedback afterward. The hardening — a
> raw-capture tee and fail-closed per-request privacy — is M1 (see [Roadmap](#roadmap)).

## Why blind?

A hash of a model name is *not* blinding — your candidate pool is a small known list, so any
deterministic encoding is trivially reversible. blindcoder mints a **random, stored token** per
provider and per model instead, so identity is genuinely hidden while the same model stays
recognizable across providers. The real name lives only behind a **reveal gate** (peeking is a
deliberate, logged act, because seeing the identity biases your future ratings).

## How the selector picks

For each candidate it draws a plausible quality from that candidate's Beta posterior (Thompson
sampling), then chooses the highest **value**:

```
value_score = quality_guess − cost_sensitivity × normalized_price
```

Ratings are two quick questions — how well it did, and how hard the task was — combined so that
handling a hard task fairly outranks acing a trivial one. The belief is an **event-sourced fold**
over your past ratings with recency decay, never a mutable score. Cheap-and-good rises, a
confirmed-bad model fades, and a cheap unknown still gets explored.

## Build

Requires a Rust toolchain (stable). With Cargo:

```sh
cargo build --release
cargo test --workspace
```

A Nix flake is provided for a reproducible dev environment (Nix is only ever used for
development and building — it is never required to *run* blindcoder):

```sh
nix develop      # drops you into a shell with the toolchain
```

## simulate

`simulate` is the project's go/no-go test. Synthetic raters with a known ground truth drive the
**real** selector, so you can measure whether the selector actually converges onto the
best-value model — and how fast — as a function of pool size, session volume, and the tuneables.
No model calls, no identity to leak.

```sh
cargo run -- simulate                 # defaults: 8-model pool, 400 sessions, 30 trials
cargo run -- simulate --pool 5 --sessions 800 --exploration 0.6
cargo run -- simulate --help
```

It prints the best-arm pick-rate over training, cumulative regret against the random-choice
baseline, and a time-to-converge estimate, ending in a plain GO / MARGINAL / NO-GO verdict.

## run and rate

Once you have declared a pool in your config (see [Configuration](#configuration)), `run` makes
a blind pick and stands up a local forwarding proxy; point your OpenAI-compatible CLI at it,
work, then end the session and `rate` how it went:

```sh
blindcoder run
#   blindcoder: routing a blinded session (picked from a pool of 4).
#     point your OpenAI-compatible CLI at:  http://127.0.0.1:8787/v1
#     model to request:  x7k2:q4m9   (any value works; the proxy rewrites it)
#     cost cap:  $5.00 ...
#     press Ctrl-C to end the session and record it.

blindcoder rate --session <id> --performance 1 --difficulty 3
```

The proxy rewrites the model on the wire to the real one, forwards to the provider, streams the
response straight back, and tallies token usage — halting the session if the estimated spend
reaches your `max_session_cost_usd`. It prints the blinded alias (never the real name) and the
session id. `performance` is `-2..=2` and `difficulty` is `0..=4`; difficulty is asked *after*
the session, against the finished work, so the rating is not anchored on an up-front guess. Made
a mistake? Rate again with `--supersedes <old-rating-id>` — corrections supersede, never overwrite.

> **M0 limitation:** the proxy forwards `/v1/models` untouched, so a CLI that lists models will
> see real provider names. Blinding covers the chat path; a models-list rewrite comes with the M1
> proxy hardening.

## Configuration

blindcoder is config-file-first. Copy [`config.example.toml`](config.example.toml) to your
config directory and edit it; precedence is **flag > environment > file > built-in default**.
Paths follow the XDG base-directory spec, so nothing is tied to a particular OS:

| What | Location |
|------|----------|
| Config | `$XDG_CONFIG_HOME/blindcoder/config.toml` |
| Authoritative event log (SQLite) | `$XDG_DATA_HOME/blindcoder/` |

The event log also holds the alias↔model map (the blind key), so it is treated as private state
and is never committed — see [`.gitignore`](.gitignore).

## Privacy

blindcoder is built to route only to endpoints that do not retain or train on your prompts, and
to prove it structurally rather than by hope:

- **Private by default, fail-closed.** Eligibility is policy-based, not price-based; a model
  whose data policy is unknown is *excluded*, not included.
- **The capture floor stores nothing sensitive.** By default only the model↔rating↔cost↔time
  signal is recorded — no prompts, no code.
- **The append-only store cannot be quietly rewritten.** Corrections supersede; database
  triggers reject edits and deletes.

## Roadmap

- **M0** — the persistent core (selector · store · config · alias), `simulate` (validation), and
  `run`/`rate` over a streaming forwarding proxy (blind pick, real proxying, cost cap, logging).
  ← *here*
- **M1** — the production proxy: raw-capture tee and fail-closed per-request privacy.
- **M2** — capture levels and byte-exact wire archives; a standing serve mode.
- **M3** — many providers, subscription cap-safety, optional market price tracking.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Contributions are welcome — see
[CONTRIBUTING.md](CONTRIBUTING.md).
