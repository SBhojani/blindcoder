# blindcoder

A **blind, cost/quality-aware router** for agentic coding CLIs.

Every coding session, blindcoder secretly picks a model from your pool, **masks its
identity** for the duration, and learns from a short end-of-session rating which models give
you the best quality *per dollar* on your actual mix of work. It is a cost/quality-aware
multi-armed bandit sitting in a small proxy one layer below the CLI, so it works with any
OpenAI-compatible agentic tool.

The point is to judge models on results, not reputation: you rate the work without knowing
who did it, and over time the router sends more of your work to whatever quietly performs.

> **Status: milestone M1 (in progress).** The permanent decision core — the selector, the
> append-only store schema, the config surface, and the aliasing — is real and tested, and
> [`simulate`](#simulate) (the offline convergence harness) validates the selector. `run` stands
> up a **streaming forwarding proxy**: it makes a blind pick, proxies your session to the chosen
> model (rewriting the model on the wire, streaming responses straight back, enforcing a cost
> cap), and records it; `rate` records your feedback afterward. `stats` reads the event log back
> as a blind per-model leaderboard. **Privacy is type-enforced:** each provider declares a ZDR
> `privacy` protocol, and the only way to build the request the transport sends (`VettedRequest`) is
> through the gate that applies that protocol's per-request injection — so a body can't reach the
> wire without it (it doesn't compile otherwise). The whole pool is checked fail-closed before any
> pick: a provider with no declaration, a declaration whose host isn't that provider's real endpoint,
> or a protocol whose unverifiable manual setup you haven't attested, aborts the run. The remaining
> M1 piece — a raw-capture tee for token-by-token usage accounting — is still to come (see
> [Roadmap](#roadmap)).

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

Once you have declared a pool in your config (see [Configuration](#configuration)), the easiest
way is **launcher mode** — hand `run` your agentic CLI and it does the rest:

```sh
blindcoder run opencode          # or: blindcoder run -- aider --some-flag
```

It makes a blind pick, stands up the pinned proxy, launches the CLI against it, and — when the CLI
exits — records the session and asks you the two rating questions inline. **No manual CLI config:**
it injects the endpoint via `OPENAI_BASE_URL`/`OPENAI_API_KEY`, and for OpenCode it injects a
`blindcoder` provider (default model = the session alias) through `OPENCODE_CONFIG_CONTENT`, which
merges into your config for that run only — nothing is written to disk. The CLI shows the blinded
alias (e.g. `blindcoder/x7k2:q4m9`), never the real name.

Prefer to drive the CLI yourself? Omit the command for **standing-proxy mode** — point any
OpenAI-compatible client at the printed endpoint and end with Ctrl-C, then rate separately:

```sh
blindcoder run
#     point your OpenAI-compatible CLI at:  http://127.0.0.1:8787/v1
#     press Ctrl-C to end the session and record it.
blindcoder rate --session <id> --performance 1 --difficulty 3
```

Either way the proxy rewrites the model on the wire, forwards to the provider, streams the response
back, and tallies usage — halting if spend reaches your `max_session_cost_usd`. `performance` is
`-2..=2` and `difficulty` is `0..=4`; difficulty is asked *after* the session, against the finished
work, so the rating is not anchored on an up-front guess. Made a mistake? Rate again with
`--supersedes <old-rating-id>` — corrections supersede, never overwrite.

> **Blinding is end-to-end:** the request `model` is rewritten to the real slug, the response
> `model` is masked back to the alias (fingerprint fields stripped), and `GET /v1/models` returns
> just the aliased model — so neither the chat path nor a CLI's model list reveals the real name.

## stats

After a few sessions, `blindcoder stats` prints a per-model leaderboard from the event store:
sessions, ratings, total cost, failures, and the selector's learned quality and value — all keyed
by the blind `model_token` so you can compare without being biased by identity.

```sh
blindcoder stats                  # best value first
blindcoder stats --sort cost      # sort by total spend
blindcoder stats --reveal         # unmask real slugs (journaled)
```

Use `--reveal` only when you are willing to bias future ratings: each unmask is routed through the
reveal gate and written to the append-only `reveals` table.

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

- **M0** — the persistent core (selector · store · config · alias), `simulate` (validation),
  `run`/`rate` over a streaming forwarding proxy, and a `stats` leaderboard over the event log.
  *shipped*
- **M1** — the production proxy: the fail-closed, type-enforced per-request privacy gate
  (`VettedRequest` typestate + host-bound, attested pool validation) is shipped; still to come is a
  raw-capture tee for mid-stream usage accounting. ← *here*
- **M2** — capture levels and byte-exact wire archives; a standing serve mode.
- **M3** — many providers, subscription cap-safety, optional market price tracking.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Contributions are welcome — see
[CONTRIBUTING.md](CONTRIBUTING.md).
