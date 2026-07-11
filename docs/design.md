# blindcoder — design overview

This is a short, public overview of how blindcoder is put together and why. It is deliberately
high-level; the code is the source of truth.

## The idea

Judge coding models on results, not reputation. Each agentic session, blindcoder picks a model
from your eligible pool, hides which one it is, and — after the session — asks you two quick
questions. Over many sessions it learns which models give you the most quality per dollar on
your actual work, and routes accordingly. It is a daily-driver **router**, not a benchmark.

## Masking

The candidate pool is a small, known list, so any deterministic encoding of a model name (a
hash, say) is trivially reversible. blindcoder instead mints a **random token** per provider and
per canonical model, stored once. The same model keeps the same model-token across providers, so
cross-provider comparison still works, while the display name (`provider_token:model_token`)
reveals nothing. The real identity is resolved only through a **reveal gate**, so unmasking is an
explicit, recorded act.

## The selector

For each candidate, draw a plausible quality from its Beta posterior (Thompson sampling), then
pick the highest value:

```
value_score = quality_guess − cost_sensitivity × normalized_price
```

- **Price is the shelf rate, normalized pool-relative** (the priciest eligible candidate ≈ 1.0),
  so `cost_sensitivity` means the same thing regardless of absolute prices.
- **Ratings are two questions:** how well it did (`-2..+2`) and how hard the task was (`0..4`).
  They combine as `performance + difficulty_credit × difficulty` **for successful performance
  only** (a negative `performance` earns no difficulty credit), so handling a hard task well can
  outrank acing a trivial one — a fairness correction for a model that drew harder work, without
  letting difficulty rescue an outright failure.
- **Belief is an event-sourced fold,** not a mutable score: replay the rating events with
  recency decay (a configurable half-life) at pick time.

The selector is pure — no I/O, no clock, no ambient randomness — which is what makes it
property-testable and what the `simulate` harness drives.

## Providers, the pool, and the proxy

blindcoder is **provider-generic by construction.** Any OpenAI-compatible
`/chat/completions` endpoint is a candidate; the code never branches on which provider it is.
Everything provider-specific is *data* in the config, not a code path:

- A provider is `{ slug, base_url, wire, key_env }` plus two passthrough hooks —
  **`extra_headers`** (verbatim per-request headers, e.g. attribution) and **`extra_body`** (a
  JSON object merged into every request body, e.g. provider-routing or data-policy/ZDR flags).
  This is how per-provider behaviour is expressed without a branch, so adding a new backend is a
  config edit, not a code change.
- Each provider lists its **models** as `{ canonical_key, real_slug, optional prices }`.
  `canonical_key` is the provider-neutral identity the selector learns on, so the *same* model
  offered by two providers shares one quality belief while the two compete on price. `real_slug`
  is what that provider's API expects in the `model` field. **Omitting prices marks a free
  tier** — it simply competes as a zero-cost candidate (no separate "is free" flag).

A useful consequence: a pool of only-free models makes the cost term inert (every candidate is
$0), so the selector reduces to a pure quality race. Mixing in at least one priced provider is
what exercises the cost/quality trade-off — which is the whole point of the router.

**The proxy is a rewrite, not a translation.** For one session the router picks a candidate,
starts a session, and forwards requests to the chosen endpoint with just two edits to the wire
body: the blind `model` field is replaced with the resolved `real_slug`, and the provider's
`extra_body` is shallow-merged in. The resolved model is always written last, so a stray or
hostile `extra_body.model` can never route around the blind. The blind→real crossing happens
only inside the **reveal gate** (reason: routing) and is journaled, so it stays auditable and
the real identity never leaks to the user.

`run` performs the pick, resolves the route through the gate, and records the session; `rate`
appends the two-question rating afterward (a correction supersedes rather than edits). Difficulty
is captured *after* the session, framed against the finished artifact, to avoid anchoring the
rating on an up-front guess.

## simulate — the go/no-go

Before building any proxy, `simulate` answers the one question that can kill the design: *does
the selector actually converge, at a realistic pool size and session volume, or does it stay a
permanent random explorer?* Synthetic raters with a known ground truth drive the real selector;
it reports best-arm pick-rate over time, cumulative regret vs. a random baseline, and
time-to-converge, and prints a GO / MARGINAL / NO-GO verdict. Use it to size the pool and tune.

**Rating sparsity is the load-bearing stress axis.** Real users rate only a fraction of sessions
(~1/day), not every one, so the honest question is whether the selector accrues enough evidence
per arm under *sparse* feedback. The `--rate-prob` knob (probability a session gets rated;
`rate × rate_prob` = ratings/day) exposes this, and it is where the design's size ceiling comes
from:

- **A small pool (3–4) degrades gracefully** — at ~1 rating/day it still captures ~0.71–0.80 of
  the value gap (MARGINAL). This is the recommended operating point.
- **A large pool (8) collapses** — at ~1 rating/day it falls to NO-GO (~0.58 value-efficiency,
  <30 % of trials converge), and a **longer horizon does not rescue it**: at 4× the sessions the
  steady-state efficiency is still ~0.58, because recency decay caps effective evidence density —
  old ratings fade as fast as sparse new ones arrive. The lever is *fewer candidates*, not *more
  time*.
- **Sparsity compounds with drift** — sparse feedback on a moving target (`--drift`) is the true
  worst case (pool=3 ≈ 0.47 value-efficiency), reinforcing that the goal is robustness, not a
  maximal stationary metric.

The prior every-session-rated harness was optimistic (its rater was the exact inverse of the
scorer). The sparsity axis is what makes the go/no-go non-circular. **Conclusion: bound the pool
(≈3–4).**

## Storage

An append-only, event-sourced SQLite log. The default capture level records only the
model↔rating↔cost↔time signal the selector needs — **no prompts or code**. Corrections
supersede (a new row), never edit; database triggers enforce append-only. Raw wire archives, if
ever enabled, are disposable files outside the database, referenced only by convention.

## Privacy

Route only to endpoints that do not retain or train on your prompts, and make that structural:

- **Fail-closed eligibility** — unknown data policy means excluded, as a hard filter.
- **Type-enforced forwarding** — the transport accepts only a vetted endpoint value, so sending
  to an un-vetted endpoint does not compile. The wire log *witnesses* enforcement; the types
  *guarantee* it.

## Milestones

- **M0** — the permanent core (selector · store · config · alias) plus `simulate` and a minimal
  blind `run`.
- **M1** — the production proxy: raw-capture tee, fail-closed per-request privacy.
- **M2** — capture levels, byte-exact wire archives, a standing serve mode.
- **M3** — many providers, subscription cap-safety, optional whole-market price tracking.

Everything below the `Backend` trait grows in place across these milestones; everything above it
is written for real at M0 and does not get rewritten.
