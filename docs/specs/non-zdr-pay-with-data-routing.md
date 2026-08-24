# Spec: opt-in non-ZDR / pay-with-data routing

**Status:** implemented on the `allow-non-zdr` feature (see Requirements 10 for the gated test
invocations).
**Scope:** add a new, generic privacy mode that lets the pool include a model whose provider does
**not** offer Zero Data Retention (i.e. it may log or train on prompts) — a "pay-with-data"
endpoint. The mode is compiled out by default, dormant unless configured, and can only fire after
four independent, deliberate acts. No change to the behaviour of the existing ZDR pool.

## Problem

Every provider in blindcoder must declare a `privacy` protocol, and the two that exist —
`open-router` (injects `provider.zdr = true` + `data_collection = "deny"`) and `groq`
(account-level ZDR) — are **fail-closed**: `apply_request_privacy` is an exhaustive `match`, and
`validate_pool_privacy` refuses to route to any provider whose ZDR story it can't attest. This is
correct and load-bearing, and it must stay the default.

But some genuinely useful models are only reachable on **non-ZDR** endpoints (free "stealth"
previews, pay-with-data tiers, providers that train on traffic). Today there is no way to route to
one, by construction. We want a way that:

- **cannot happen by accident or by copying a config**, and
- **does not weaken the guarantees for the rest of the pool** (the ZDR arms stay ZDR), and
- **does not deblind the router** (blindness is the whole point of the harness).

This is a general capability, not a one-model hack. It is named and framed generically: a mode for
routing to non-ZDR / pay-with-data endpoints. No specific model or vendor appears anywhere in the
source, tests, examples, or this document.

## Goal

A `no-zdr` privacy mode that is **safe by default at every layer** — compiled out, dormant when
unused, and gated behind a chain of four independent deliberate acts — while preserving the
exhaustive-match compile-time review, the per-provider scoping, blindness, and the cost path.

## Design

### The mode

- New `Privacy` variant `NoZdr`, config value `privacy = "no-zdr"`. Provider-agnostic: unlike
  `OpenRouter`, it does **not** bind to a vendor endpoint host and does **not** inject any wire
  flags — its `apply_request_privacy` arm is a **reviewed no-op** (a comment must say so). Because
  it injects nothing, it works for any OpenAI-wire endpoint; the *consent chain below is the
  enforcement*, not endpoint-host matching.
- The arm still produces a `VettedRequest` (injecting nothing), so the transport's
  send-only-accepts-`VettedRequest` typestate invariant is unbroken and the exhaustive `match`
  still forces a reviewer to write this arm deliberately.

### The four gates (a progressively-disclosed consent chain)

The mode is inert unless **all four** independent channels are satisfied. This is a deliberate
*conjunction*, distinct from the normal `flag > env > file` precedence — none overrides another;
all must be present:

1. **Build feature** — a Cargo feature that compiles the `no-zdr` routing path in. Default builds
   omit it entirely. Building with it emits a `cargo:warning` so the build log announces the
   capability is present. *(documented — see Disclosure boundary.)*
2. **Config attestation** — a per-provider key that must list the **exact `real_slug`** of every
   `no-zdr` model under that provider. Not a blanket boolean: a provider cannot be opted out and
   then silently grow a second model. *(undocumented.)*
3. **Environment second factor** — an environment variable that must be set at launch. A committed
   config can therefore never route non-ZDR on its own; an operator must opt in per-session,
   per-machine, through a channel that lives in no file. *(undocumented.)*
4. **Runtime flag** — a CLI flag on the invocation, hidden from `--help`. *(undocumented.)*

Plus a bounded lifetime:
5. **Expiry** — a required per-provider `expires` date. At startup, if a `no-zdr` provider is
   expired **or** dated more than 30 days in the future, blindcoder **refuses to start** (hard
   stop, not a prune — the whole process halts). The window is then enforced **per request**: the
   fail-closed audit hook re-checks `expires` on every forward and refuses once it has passed,
   so a standing proxy cannot route non-ZDR traffic past its attestation's window. The 30-day
   rule caps how long the capability can be armed; it is a hard maximum, not a reminder.
   *(the `expires` key is undocumented; the 30-day bound is revealed only when violated — see
   below.)*

### Reveal chain (when each undocumented token surfaces)

The chain evaluates **only** when a provider with `privacy = "no-zdr"` is present in the parsed
config. Otherwise it is completely dormant — a normal user sees nothing, and setting the env var or
flag with no `no-zdr` provider is silently inert.

When such a provider is present, a single ordered check runs at startup and **short-circuits at the
first unmet gate, revealing only that gate's requirement** — never a later one. So the literal
identifiers surface strictly in sequence, and a config-level error can never leak the env var or
flag to someone who has not yet passed the config gates.

| Order | Condition (all earlier gates already satisfied) | Revealed |
|------:|--------------------------------------------------|----------|
| 0 | `privacy = "no-zdr"` on a build **without** the feature | *(documented)* the build feature is required |
| 1 | feature built; attestation key **absent/empty** | the attestation key + that it must list each model's exact `real_slug` |
| 1b | attestation present but ≠ the provider's model slugs | the specific mismatch (no new token) |
| 2 | attestation satisfied; **no** `expires` | that `expires` is required |
| 2b | `expires` in the past | "expired" (refuse to start) |
| 2c | `expires` more than 30 days out | **only now** the 30-day cap rule (refuse to start) |
| 3 | all config gates pass; env var unset | the environment variable |
| 4 | env set; flag not passed | the CLI flag |
| ✓ | all four satisfied | startup banner fires; audit trail opens (fail-closed) |

Properties: strictly sequential disclosure; the 30-day bound is invisible to a compliant
near-future value; one thing revealed per run; dormant by default.

### Blindness-preserving disclosure

A **per-request** banner naming the model would deblind the route and poison the harness rating.
So disclosure is **session-level only**: a single startup banner states that the pool contains a
non-ZDR model that may train on prompts and that the operator should treat the *entire session* as
non-private — **without** revealing which alias it is. This is also the more conservative posture:
because the operator cannot tell which requests are the non-ZDR one, the whole session must be
treated as non-private.

### Fail-closed audit trail

Every request routed to a `no-zdr` model appends one `<YYYY-MM-DDTHH>\t<real_slug>` line to a
dedicated append-only file (mode `0600`, re-tightened idempotently on every open so a file left
looser by an older build cannot stay loose): a whole-UTC-hour bucket plus the real model slug,
one line per forwarded request.
The record deliberately carries **no session identifier**, and not a
random per-session token either: the store keys ratings on `session_id`, so an id-bearing audit
file would join onto the ratings table and deblind every non-ZDR session and its rating — and
could be read mid-session to unmask the session before it is rated. A per-session token fails the
same way (tail the file once and the freshly appearing token's slug names the live session), and
minute-level timestamps would pin a just-run session by recalling roughly when it ran; the hour
bucket makes requests from concurrent sessions within one hour indistinguishable by construction.
The file therefore guarantees **aggregate accountability only** — which real models received
prompts, in which clock hours — with per-session attribution impossible by construction;
unmasking a specific session remains the reveal gate's sole job. Each record is formatted into
one small buffer and written under `O_APPEND`; what keeps concurrent lines intact is the
**per-syscall atomicity of `write(2)` on a regular file** — each write atomically positions at
end-of-file, so a record small enough to complete in one syscall lands as an indivisible line
even with several processes appending to the shared log. The audit remains **fail-closed** in
both directions: if the file cannot be opened or written, or the attestation's window has
passed, the request is **refused** — no routing without a durable record. The blocking
open/write/fsync runs on tokio's blocking pool — never stalling an async worker — and still
completes before anything is forwarded.

### Cost path

Non-ZDR does **not** imply free. The `no-zdr` mode must honour `input_per_mtok` /
`output_per_mtok` and the `max_session_cost_usd` kill-switch exactly like a priced provider, so a
paid pay-with-data endpoint is costed and capped normally.

## Requirements

1. **`Privacy::NoZdr` variant** with config value `no-zdr`; provider-agnostic; no endpoint-host
   binding; `apply_request_privacy` arm is a reviewed no-op producing a `VettedRequest`.
2. **Feature-gated routing path**, compiled out by default; `cargo:warning` when the feature is on.
   A `no-zdr` provider on a non-feature build is rejected at startup, revealing the (documented)
   feature requirement.
3. **Per-model exact-slug attestation** (undocumented key); startup fails unless it exactly matches
   the set of `real_slug`s under that provider.
4. **Environment second factor** (undocumented) and **CLI flag** (undocumented, `--help`-hidden),
   both required in conjunction with the config.
5. **Required `expires`** per `no-zdr` provider; **refuse to start** if absent, past, or > 30 days
   out; the 30-day bound is revealed only on violation. The window is re-checked **per request**
   at the fail-closed audit hook: once `expires` passes, a standing proxy refuses further non-ZDR
   forwards instead of routing past its window.
6. **Ordered, short-circuiting reveal** per the table above — one gate per run, never a later token
   before an earlier gate passes.
7. **Session-level startup banner only**; no per-request identity disclosure.
8. **Fail-closed append-only audit trail** at `<YYYY-MM-DDTHH>\t<real_slug>` granularity —
   whole-UTC-hour buckets plus the real slug, **no session identifier** (aggregate accountability
   only; per-session attribution impossible by construction, and the reveal gate stays the sole
   unmasking path).
9. **Cost path fully live** for `no-zdr` (pricing + session cap).
10. **Tests both ways:** default `cargo test --workspace` passes with the path compiled out; the
    `no-zdr` behaviour is tested under the feature. Fixtures use a placeholder slug
    (`example/non-zdr-model`) — never a real vendor or model name. The feature-gated tests
    compile out of default builds **by design** — do not add CI or un-gate them; run
    `cargo test --workspace --features allow-non-zdr` (and the matching clippy invocation)
    before changing this path.

## Disclosure boundary

"Documented" = present in the README / `config.example.toml` / `--help`. "Undocumented" = present
in **source and runtime error messages only** — enough to stop accidental or copy-paste
enablement, not a determined source-reader (the intended operator). Concretely:

- **Documented:** the mode exists, its `privacy = "no-zdr"` value, and the build feature. The
  example config carries a short **commented stub** (mode + feature, then "additional required
  attestations surface at startup") — never a copy-pasteable working block.
- **Undocumented (source + errors only):** the attestation key, the environment variable, and the
  CLI flag — their literal identifiers do **not** appear in the README, the example config,
  `--help`, or this spec. This document describes their *shape and reveal conditions*; the exact
  strings live only in the code and the runtime reveal messages.
- **No vendor or model** appears anywhere in source, tests, examples, or docs.

## Enabling on NixOS (operator side)

The public flake exposes only the inert `default` (feature compiled out). The operator enables the
feature declaratively in their own config via `overrideAttrs` — nothing in the public repo ships an
enabled build:

```nix
inputs.blindcoder.packages.${system}.default.overrideAttrs (old: {
  buildFeatures  = (old.buildFeatures  or []) ++ [ "allow-non-zdr" ];
  checkFeatures  = (old.checkFeatures  or []) ++ [ "allow-non-zdr" ];  # run the no-zdr tests
})
```

For a quick pre-packaging trial: `nix develop -c cargo build --release --features allow-non-zdr`
and run the resulting binary.

## Non-goals

- No per-request identity disclosure (would deblind).
- No prune-and-continue on expiry (we chose hard refuse-to-start).
- No secret/passphrase gating (theatre for an open-source tool; the threat model is accidental /
  copy-paste enablement, not a determined source-reader).
- No vendor- or model-specific code.
