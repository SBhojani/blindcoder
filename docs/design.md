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

### What blinding protects — and where it leaks

Blinding defends against the thing that actually biases ratings: **name-driven prior belief**
("it's the expensive famous one, must be good"). It does *not* claim perfect anonymity, and it
is honest to say where it can leak:

- **Cost and latency are tells.** A visibly pricey or conspicuously slow response narrows the
  guess. Mitigation: the rating is collected *before* any cost is shown, so the number can't
  anchor the score; cost enters only the selector's math, not your judgement.
- **Models self-identify.** A model may name itself in its output, or have a recognizable style.
  Blinding can't prevent this; it only avoids *volunteering* the identity.
- **A small pool is low-entropy.** With two or three candidates, a confident guess is sometimes
  right by chance. The tokens stop *deterministic* de-anonymization (a hash wouldn't), not
  informed inference.
- **Error messages can name the model (closed).** A provider's *error message* is free text that can
  embed the real slug — e.g. `Request too large for model \`vendor/model-x\` in organization
  \`org_…\``, printed verbatim by the CLI, which masking the structured `model` field alone would
  miss. This is closed: response masking *additionally* string-replaces every occurrence of the real
  slug with the alias across the **whole** body — error text included — not just the `model` field.
  Provider org IDs are deliberately left intact (an account tell, not a model tell; a blanket `org_`
  scrub risks corrupting unrelated content). A failed session is never rated, so this never biased a
  rating directly, but it defeated blinding for the user until closed.

The design goal is therefore "don't hand you the label," not "make identification impossible."
Peeking is always available through the reveal gate — it is just made deliberate and logged,
because every peek biases the ratings that follow.

## The selector

For each candidate, draw a plausible quality from its Beta posterior (Thompson sampling), then
pick the highest value:

```
value_score = quality_guess − cost_sensitivity × normalized_price
```

- **Price is the shelf rate, normalized pool-relative** (the priciest eligible candidate ≈ 1.0),
  so `cost_sensitivity` means the same thing regardless of absolute prices. This matters because
  catalog rates span roughly four orders of magnitude and are heavily right-skewed (a cheap bulk
  with a long expensive tail), so an absolute price penalty would behave wildly differently on a
  cheap pool versus a pricey one; pool-relative normalization keeps the knob meaningful.
- **Ratings are two questions:** how well it did (`-2..+2`) and how hard the task was (`0..4`).
  They combine as `performance + difficulty_credit × difficulty` **for successful performance
  only** (a negative `performance` earns no difficulty credit), so handling a hard task well can
  outrank acing a trivial one — a fairness correction for a model that drew harder work, without
  letting difficulty rescue an outright failure.
- **Belief is an event-sourced fold,** not a mutable score: replay the rating events with
  recency decay (a configurable half-life) at pick time.
- **Failed sessions are learned against too.** A crash is never rated, so if only ratings fed the
  belief a candidate that keeps failing would stay invisible. Each failed session instead folds in
  as decayed **loss** evidence (no wins), so a candidate that keeps failing sinks in its posterior
  and gets picked less — with recency decay letting it recover if the failures stop, so there is no
  arbitrary strike threshold. How much a failure counts is a **policy weight by kind**: a
  persistent, workload-fatal failure (the model+tier structurally cannot serve you — e.g. a
  request-too-large) counts near a full lost session; a transient backend hiccup (rate limit, 5xx,
  network) counts little (decay clears it); an our-fault error (auth, malformed request) counts
  nothing, because the model is blameless. A single `failure_sensitivity` knob scales the whole
  effect (0 disables it). Crucially the **selector never sees the error taxonomy** — it folds a
  generic weighted loss; the error-kind→weight mapping lives in the router layer, keeping the
  decision core pure.

The selector is pure — no I/O, no clock, no ambient randomness — which is what makes it
property-testable and what the `simulate` harness drives. (The harness does not yet inject
failures, so the per-kind weights are principled defaults awaiting the same sweep treatment as the
other tuneables.)

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

## The transport seam

Everything above the `Backend` trait (selector, store, config, aliasing, the CLI) is written for
real at M0 and never rewritten; everything below it — the actual byte-forwarding — grows in place
across milestones. The seam is deliberately a **session lifecycle**, not a single blocking call:
the router `start`s a session, then observes usage events and can `abort` mid-flight before
`finish`. That shape exists for one reason — **a per-session spend cap can only be enforced by a
transport that reports cost *as it streams*; a one-shot call could only report it after the money
was spent.**

The split is: **mechanism in the backend, policy in the router.** The backend just observes usage
and can be told to stop; the router owns the threshold and the token→cost estimate. So
`max_session_cost_usd` is a genuine kill-switch (halt the session and prompt) rather than an
after-the-fact report — a **catastrophe bound on runaway agentic loops, not a budget lever**. The
router prices each usage tick as tokens × the pick's shelf price, so the cap fires without the
transport having to price mid-stream.

This is a *spend* cap (total dollars this session), distinct from a *rate* ceiling. Some gateways
also accept a per-request rate ceiling — e.g. OpenRouter's `provider.max_price`, a hard `$/Mtok`
bound that refuses any provider above it — a useful complement passed straight through the config's
request-body hooks, but not a substitute: a rate ceiling bounds `$/token`, the session cap bounds
total `$`, and only the latter stops a cheap model looping forever.

That token×shelf-price figure is an **estimate**, not the bill. A model on an aggregating gateway has
no single price — several serving providers offer it at different rates — and by default the gateway
**load-balances across the eligible providers**, so which one (and which price) you get varies run to
run and is often not the cheapest. (Observed in live testing: one model was served by five providers
spanning a ~1.6× spread; back-to-back requests under the same settings landed on different providers,
and the bill matched whichever one served that call, to the cent.) You *can* pin it — sorting by
price routes to the cheapest eligible provider deterministically — but even that can drift as
providers come and go. The estimate is fine for the cap and the cost-tilt (relative ordering is what
the selector needs); for the **authoritative** figure, gateways typically return the real per-request
cost inline in the response (a `cost` field alongside token usage). The transport captures that and
records it as `realized_cost`, falling back to the estimate only when none is returned — and
`session_end` tags the origin (`cost_source` = `provider` | `estimate`) so the number is never
misread. The reported cost also drives the mid-session cap when present.

The M0 transport is a small **streaming reverse proxy**: `run` binds it on a local port, and for
each request it rewrites the blind model to the real slug, merges the provider's `extra_body`,
forwards with the API key and `extra_headers`, and streams the response back while tallying the
`usage` block into cumulative token counts (the `Usage` events the cap acts on). It blinds **both
directions**: the request `model` is rewritten to the real slug, and on the way back the response
`model` is rewritten to the alias with provider fingerprint fields (`provider`, `system_fingerprint`,
`x_groq`) stripped — per SSE frame so streaming is preserved, or once for a buffered JSON body.
Point any OpenAI-compatible CLI at it. The catalog is blinded too: `GET …/models` is answered
locally with just the aliased model, so a CLI's model picker can't deblind the session. The one
remaining M0 limitation is that it accounts usage per completed response rather than token-by-token
mid-stream — which tightens with the M1 tee, behind this same trait, with no change above the seam.

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

**What the sweeps settled about tuning.** Two knobs beyond `cost_sensitivity` shape the pick — how
much extra exploration to add to the Thompson draw, and how aggressively to prune cost-dominated
candidates. Sweeping them — including against a *drifting* ground truth, not just a stationary one —
overturned the a-priori intuitions:

- **Exploration is not the reckless knob.** Low exploration wins in every regime, because Thompson's
  uniform prior plus recency decay already re-explore a moving target on their own; cranking
  exploration just pays regret for re-derived information.
- **Pruning *is* the reckless knob.** Aggressive pruning tops the *stationary* metric but is the
  worst under drift — it permanently discards an arm that later recovers. A stationary win bought by
  aggressive pruning is a mirage.

So the responsible defaults are **low exploration and conservative pruning**, chosen for robustness
across stationary *and* drifting worlds rather than a maximal stationary number. The lesson
generalizes: for any stochastic/learning knob, stress the non-stationary case before trusting a
default the stationary metric likes.

## Storage

An append-only, event-sourced SQLite log. The **capture level** (`metadata` | `contents` |
`replay`, a config enum recorded on each session row) sets how much is kept. The default,
`metadata`, records only the model↔rating↔cost↔time signal the selector needs — **no prompts or
code**. Corrections supersede (a new row), never edit; database triggers enforce append-only.

At `replay`, blindcoder additionally archives the **verbatim four-leg wire exchange** —
`cli_request`, `provider_request`, `provider_response`, `cli_response` — byte-exact to a disposable
WARC file per session, outside the database (`$XDG_STATE_HOME/blindcoder/wire/<session_id>.warc`,
mode `0600`, referenced only by convention). The provider legs are kept **raw** (the real model
identity and provider fingerprints intact) while the CLI legs are the **masked** copy the CLI
actually saw, so the archive captures both sides of the blind↔real rewrite and a session is fully
replayable and auditable. These files hold prompts and model output verbatim, so `replay` is a
deliberate opt-in above the default privacy floor.

The schema evolves through **versioned migrations** (`rusqlite_migration`, tracked by SQLite's
`PRAGMA user_version`): a frozen baseline plus append-only `M::up` steps, applied atomically on
open so a database holding real ratings upgrades **in place** rather than being dropped. Foreign
keys are toggled **off during migration** (so a migration can rebuild a *referenced* table — the
SQLite 12-step `ALTER` — which is how a `CHECK` constraint is added to an existing column) and
**on afterward** so the `REFERENCES` are enforced at runtime.

A failed session is recorded, not silently dropped: the proxy derives an **`error_kind`** (rate
limit / auth / too-large / 5xx / truncated / refused / …) from the upstream status and
`finish_reason`, and stores it **with the raw HTTP `error_status`**. The categories are kept
semantically distinct rather than lumped by status class — a 413 "request too large" (a context or
per-minute-token limit) is `too_large`, not a *malformed* `bad_request` or a *transient* rate limit,
because for a large-context workload it means the model+tier structurally cannot serve you. The
derivation is status-based only; the raw status (and the wire archive at `replay`) preserve the finer
cause without sniffing provider-specific error text. The tag is a *projection* over the stored ground
truth, so a failure is diagnosable even at the `metadata` floor (where no wire archive exists) and
the selector can learn to avoid a provider that keeps failing. Closed-set columns (`error_kind`,
`cost_source`, `terminated_by`, `capture_level`) are Rust enums that serialize to `TEXT` and are
`CHECK`-constrained in the DB — SQLite's enum equivalent, enforced on both sides.

## Privacy

Route only to endpoints that do not retain or train on your prompts, and make that structural.

**Privacy is the one place blindcoder is provider-aware — deliberately.** Everywhere else, providers
differ only in *data* (base URL, slug, key, price), so the code stays generic and branch-free. But
the mechanism for *guaranteeing* Zero-Data-Retention genuinely differs per vendor — OpenRouter takes
a per-request body flag, Groq is an account-level console setting, others use headers — and it is a
security boundary where a wrong setting must fail **closed**, not open. So each provider's protocol is
an explicit, exhaustively-matched `Privacy` variant (adding a provider won't compile until its
protocol is written and reviewed). Provider *names* appear here, and only here.

- **Fail-closed eligibility** — the whole configured pool is validated before any pick. A provider
  with no `privacy` declaration, a declaration whose `base_url` host isn't that provider's real
  endpoint (an account-level attestation is only meaningful for the endpoint it was verified on), a
  foreign or unknown attestation key, or an un-attested manual-setup protocol → the run aborts.
- **Type-enforced forwarding** — the only way to obtain the `VettedRequest` the transport sends is
  the gate (`VettedEndpoint::prepare`) that applies the protocol's per-request injection; the send
  path accepts only a `VettedRequest`. So a body cannot reach the wire without the injection — it does
  not compile otherwise. The wire log *witnesses* enforcement; the types *guarantee* it.
- **Attested manual setup** — where a protocol depends on setup blindcoder can't see on the wire
  (e.g. enabling ZDR in the Groq console), it fails closed until the operator sets a **provider-named,
  undocumented** attestation key (e.g. `groq_manual_steps_done`). The key is revealed *only* by the
  fail-closed error, so it can't be a copied default — it must be a deliberate act after reading the
  steps, and one provider's key set on another is a detectable error.

## Milestones

- **M0** — the permanent core (selector · store · config · alias), `simulate`, and `run`/`rate`
  over a streaming forwarding proxy (real blind pick, live proxying, cost cap, session logging).
- **M1** — the production proxy: fail-closed, type-enforced per-request privacy (shipped); raw-capture
  tee for mid-stream usage accounting (remaining).
- **M2** — capture levels, byte-exact wire archives, a standing serve mode.
- **M3** — many providers, subscription cap-safety, optional whole-market price tracking.

Everything below the `Backend` trait grows in place across these milestones; everything above it
is written for real at M0 and does not get rewritten.
