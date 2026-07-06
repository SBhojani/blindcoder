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
  They combine as `performance + difficulty_credit × difficulty`, so handling a hard task fairly
  can outrank acing a trivial one — the fairness correction for a model that drew harder work.
- **Belief is an event-sourced fold,** not a mutable score: replay the rating events with
  recency decay (a configurable half-life) at pick time.

The selector is pure — no I/O, no clock, no ambient randomness — which is what makes it
property-testable and what the `simulate` harness drives.

## simulate — the go/no-go

Before building any proxy, `simulate` answers the one question that can kill the design: *does
the selector actually converge, at a realistic pool size and session volume, or does it stay a
permanent random explorer?* Synthetic raters with a known ground truth drive the real selector;
it reports best-arm pick-rate over time, cumulative regret vs. a random baseline, and
time-to-converge, and prints a GO / MARGINAL / NO-GO verdict. Use it to size the pool and tune.

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
