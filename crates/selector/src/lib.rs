//! blindcoder **selector** — the crown-jewel decision core.
//!
//! Pure functions only: no I/O, no clock, no randomness of its own (callers pass an
//! [`rng`](rand::Rng)). This is what the `simulate` harness validates and what `proptest`
//! exercises; it is written for real at M0 and never restructured.
//!
//! The pick rule is: draw a plausible quality for each candidate from its Beta posterior
//! (Thompson sampling), then choose the highest **value** — quality minus a price penalty:
//!
//! ```text
//! value_score = quality_guess - cost_sensitivity * normalized_price
//! ```
//!
//! Track records are **not** stored as mutable state; they are recomputed by folding the
//! candidate's rating events (with recency decay) at pick time — see [`fold_track_record`].

use rand::Rng;
use rand_distr::{Beta, Distribution};

/// Uniform Beta prior: every candidate starts at `wins == losses == 1`
/// ("no idea, 50/50, wide").
pub const PRIOR: f64 = 1.0;

/// The knobs the selector reads. Defaults are the pinned tuneable values.
#[derive(Clone, Copy, Debug)]
pub struct Tuneables {
    /// Price vs quality trade-off. 0 = ignore price; 2 = ruthlessly cheap.
    pub cost_sensitivity: f64,
    /// Extra credit for handling hard tasks (fairness correction).
    pub difficulty_credit: f64,
    /// Recency decay half-life for rating events, in days.
    pub rating_half_life_days: f64,
    /// <1 = exploit/commit faster; >1 = explore more. This scales the *evidenced* part of the
    /// posterior only — a candidate with zero evidence still draws uniform regardless, so a lower
    /// value effectively anneals per-candidate (explore the unseen, exploit the well-rated).
    pub exploration: f64,
    /// Logistic scale for squashing a rating into a 0..1 session score. Larger = gentler.
    pub score_spread: f64,
    /// Confidence width (in posterior std-devs) for cost-dominance pruning. Higher = more
    /// conservative (prunes less); a candidate is dropped only if its optimistic value still
    /// trails the strongest candidate's pessimistic value at this width.
    pub prune_confidence: f64,
    /// Global scale on failure evidence. Each failed session adds `loss_weight * failure_sensitivity`
    /// (decayed) to the candidate's losses. 0 = ignore failures entirely; 1 = a full-weight failure
    /// counts like a lost session. The per-failure `loss_weight` (transient vs structural) is the
    /// caller's policy — the selector stays free of `error_kind` semantics.
    pub failure_sensitivity: f64,
}

impl Default for Tuneables {
    fn default() -> Self {
        Self {
            cost_sensitivity: 0.5,
            difficulty_credit: 0.75,
            rating_half_life_days: 60.0,
            // Lowered from 1.0 to 0.4 on the strength of the `simulate` sweep: lower exploration
            // dominates at every pool size (constant 1.0 is a permanent-exploration floor).
            exploration: 0.4,
            score_spread: 2.0,
            prune_confidence: 2.0,
            failure_sensitivity: 1.0,
        }
    }
}

/// A single rating event, folded into a track record. `age_days` is how long before "now"
/// the rating happened (0 = just now), used for recency decay.
#[derive(Clone, Copy, Debug)]
pub struct Rating {
    /// How well the model performed, in `-2..=2`.
    pub performance_points: f64,
    /// How hard the task was, in `0..=4`.
    pub difficulty_points: f64,
    /// Age of this rating in days, at the moment of the fold.
    pub age_days: f64,
}

/// A failed session, folded into a track record as loss evidence. Unlike a [`Rating`] it carries no
/// user judgement — a crash is never rated — so it contributes pure losses. `loss_weight` encodes how
/// much this *kind* of failure should count (the caller's policy: a persistent `too_large` ~1.0, a
/// transient rate-limit ~0.15, an our-fault auth error ~0.0); the selector never sees `error_kind`.
#[derive(Clone, Copy, Debug)]
pub struct Failure {
    /// How strongly this failure counts against the candidate, in `0..=1` before `failure_sensitivity`.
    pub loss_weight: f64,
    /// Age of this failure in days, at the moment of the fold (recency-decayed like ratings).
    pub age_days: f64,
}

fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Map one rating to a session score in `(0, 1)`.
///
/// Difficulty credit is a *fairness* correction for a model that drew harder work — so "fine on a
/// hard task" can match or beat "great on a trivial task". But it must never rescue a failure: a
/// bad performance on a hard task is still a failure, not a win. So the credit only applies to
/// non-negative (successful) performance; a negative `performance_points` earns no credit and is
/// scored purely on its own merit. `adjusted = performance_points + credit`, squashed through a
/// logistic of scale `score_spread`.
pub fn session_score(r: &Rating, t: &Tuneables) -> f64 {
    let credit = if r.performance_points > 0.0 {
        t.difficulty_credit * r.difficulty_points
    } else {
        0.0
    };
    let adjusted = r.performance_points + credit;
    logistic(adjusted / t.score_spread)
}

/// Beta posterior over a candidate's quality: `wins`/`losses` are pseudo-counts (start at the
/// [`PRIOR`]).
#[derive(Clone, Copy, Debug)]
pub struct TrackRecord {
    pub wins: f64,
    pub losses: f64,
}

impl TrackRecord {
    /// The blank-slate prior every candidate starts from.
    pub fn blank() -> Self {
        Self { wins: PRIOR, losses: PRIOR }
    }

    /// Posterior mean quality (point estimate; the selector uses a *draw*, not this).
    pub fn mean(&self) -> f64 {
        self.wins / (self.wins + self.losses)
    }
}

/// Fold rating events into a track record, applying recency decay so old ratings fade with a
/// half-life of `rating_half_life_days`. This is the event-sourced belief: no mutable state, a
/// pure function of the events.
pub fn fold_track_record(ratings: &[Rating], t: &Tuneables) -> TrackRecord {
    fold_track_record_with_failures(ratings, &[], t)
}

/// Fold rating events **and failed-session events** into a track record. Ratings contribute
/// wins/losses as in [`fold_track_record`]; each [`Failure`] adds decayed losses of
/// `loss_weight * failure_sensitivity` and no wins — so a candidate that keeps failing sinks in its
/// Beta posterior and Thompson picks it less, while recency decay lets it climb back if the failures
/// stop. No arbitrary strike threshold; `failure_sensitivity = 0` disables the whole effect.
pub fn fold_track_record_with_failures(
    ratings: &[Rating],
    failures: &[Failure],
    t: &Tuneables,
) -> TrackRecord {
    let mut tr = TrackRecord::blank();
    for r in ratings {
        let decay = 0.5_f64.powf(r.age_days / t.rating_half_life_days);
        let s = session_score(r, t);
        tr.wins += s * decay;
        tr.losses += (1.0 - s) * decay;
    }
    for f in failures {
        let decay = 0.5_f64.powf(f.age_days / t.rating_half_life_days);
        tr.losses += f.loss_weight * t.failure_sensitivity * decay;
    }
    tr
}

/// Draw a plausible quality in `(0, 1)` from the track record's Beta posterior (Thompson
/// sampling). `exploration` widens the posterior by shrinking the pseudo-counts toward the
/// prior, so a higher value keeps the selector exploring for longer.
pub fn draw_quality<R: Rng + ?Sized>(tr: &TrackRecord, t: &Tuneables, rng: &mut R) -> f64 {
    let e = t.exploration.max(1e-6);
    let alpha = PRIOR + (tr.wins - PRIOR) / e;
    let beta = PRIOR + (tr.losses - PRIOR) / e;
    Beta::new(alpha.max(1e-6), beta.max(1e-6))
        .expect("beta params are positive")
        .sample(rng)
}

/// `quality_guess - cost_sensitivity * normalized_price`.
pub fn value_score(quality_guess: f64, normalized_price: f64, t: &Tuneables) -> f64 {
    quality_guess - t.cost_sensitivity * normalized_price
}

/// A candidate in a single pick: its folded track record and its pool-relative price.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: usize,
    pub track: TrackRecord,
    /// Pool-relative price in `0..=1` (priciest eligible candidate ≈ 1.0).
    pub normalized_price: f64,
}

/// Pool-relative price normalization: scale so the priciest candidate ≈ 1.0, so
/// `cost_sensitivity` means the same thing regardless of absolute prices. An all-zero (or empty)
/// input yields all-zero prices.
pub fn normalize_prices(raw_prices: &[f64]) -> Vec<f64> {
    let max = raw_prices.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return vec![0.0; raw_prices.len()];
    }
    raw_prices.iter().map(|p| p / max).collect()
}

/// One Thompson pick: draw a quality for each candidate, return the **index** of the candidate
/// with the highest `value_score`. Panics on an empty pool (callers guarantee ≥1 candidate).
pub fn pick<R: Rng + ?Sized>(cands: &[Candidate], t: &Tuneables, rng: &mut R) -> usize {
    assert!(!cands.is_empty(), "pick() called on an empty pool");
    let mut best = 0usize;
    let mut best_v = f64::NEG_INFINITY;
    for (i, c) in cands.iter().enumerate() {
        let q = draw_quality(&c.track, t, rng);
        let v = value_score(q, c.normalized_price, t);
        if v > best_v {
            best_v = v;
            best = i;
        }
    }
    best
}

/// Cost-aware **dominance pruning**: return the indices of candidates that can still plausibly
/// win, dropping the ones that provably cannot.
///
/// Using each candidate's posterior mean ± `prune_confidence` std-devs as optimistic/pessimistic
/// quality bounds, a candidate is kept only if its *optimistic* value reaches the strongest
/// candidate's *pessimistic* value. Because price is known exactly, a costly candidate whose even
/// best-case value trails a cheaper rival's conservative value can never win on value — so the
/// selector stops spending picks on it, shrinking the effective pool without an arbitrary cap.
/// Always keeps at least one candidate; never returns empty.
pub fn prune_dominated(cands: &[Candidate], t: &Tuneables) -> Vec<usize> {
    if cands.len() <= 1 {
        return (0..cands.len()).collect();
    }
    let k = t.prune_confidence;
    // (pessimistic value, optimistic value) per candidate.
    let bounds: Vec<(f64, f64)> = cands
        .iter()
        .map(|c| {
            let (w, l) = (c.track.wins, c.track.losses);
            let n = w + l;
            let mean = w / n;
            let std = ((w * l) / (n * n * (n + 1.0))).sqrt();
            let q_lo = (mean - k * std).clamp(0.0, 1.0);
            let q_hi = (mean + k * std).clamp(0.0, 1.0);
            (value_score(q_lo, c.normalized_price, t), value_score(q_hi, c.normalized_price, t))
        })
        .collect();
    let best_pess = bounds.iter().map(|b| b.0).fold(f64::NEG_INFINITY, f64::max);
    let keep: Vec<usize> = (0..cands.len()).filter(|&i| bounds[i].1 >= best_pess).collect();
    if keep.is_empty() {
        (0..cands.len()).collect() // defensive: never prune everything
    } else {
        keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn blank_record_is_uniform() {
        let tr = TrackRecord::blank();
        assert!((tr.mean() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn decay_halves_contribution_at_one_half_life() {
        let t = Tuneables::default();
        let fresh = fold_track_record(
            &[Rating { performance_points: 2.0, difficulty_points: 0.0, age_days: 0.0 }],
            &t,
        );
        let old = fold_track_record(
            &[Rating {
                performance_points: 2.0,
                difficulty_points: 0.0,
                age_days: t.rating_half_life_days,
            }],
            &t,
        );
        // The aged rating adds half the mass above the prior that the fresh one does.
        let fresh_gain = fresh.wins - PRIOR;
        let old_gain = old.wins - PRIOR;
        assert!((old_gain - fresh_gain * 0.5).abs() < 1e-9);
    }

    #[test]
    fn failures_lower_quality_and_sensitivity_disables_them() {
        let t = Tuneables::default();
        let rating = Rating { performance_points: 2.0, difficulty_points: 0.0, age_days: 0.0 };
        let clean = fold_track_record(&[rating], &t);
        // Same rating, but the candidate also failed hard three times → lower posterior mean.
        let fails = [Failure { loss_weight: 1.0, age_days: 0.0 }; 3];
        let with_fails = fold_track_record_with_failures(&[rating], &fails, &t);
        assert!(with_fails.mean() < clean.mean(), "failures must drag the quality belief down");
        assert!(with_fails.losses > clean.losses);
        // failure_sensitivity = 0 makes failures inert (back to the clean record).
        let t0 = Tuneables { failure_sensitivity: 0.0, ..t };
        let disabled = fold_track_record_with_failures(&[rating], &fails, &t0);
        assert_eq!(disabled.losses, clean.losses);
        assert_eq!(disabled.wins, clean.wins);
    }

    #[test]
    fn failures_decay_with_age() {
        let t = Tuneables::default();
        let fresh = fold_track_record_with_failures(
            &[], &[Failure { loss_weight: 1.0, age_days: 0.0 }], &t);
        let old = fold_track_record_with_failures(
            &[], &[Failure { loss_weight: 1.0, age_days: t.rating_half_life_days }], &t);
        // An aged failure adds half the loss mass (above the prior) of a fresh one.
        assert!(((old.losses - PRIOR) - (fresh.losses - PRIOR) * 0.5).abs() < 1e-9);
    }

    #[test]
    fn selector_beats_random_on_a_clear_winner() {
        // One obviously-best free candidate; the selector should pick it far above chance.
        let t = Tuneables::default();
        let mut rng = StdRng::seed_from_u64(42);
        let mut ratings: Vec<Vec<Rating>> = vec![vec![]; 4];
        let true_q: [f64; 4] = [0.9, 0.5, 0.4, 0.3];
        let mut best_hits = 0;
        for s in 0..600 {
            let cands: Vec<Candidate> = (0..4)
                .map(|i| Candidate {
                    id: i,
                    track: fold_track_record(&ratings[i], &t),
                    normalized_price: 0.0, // all free → pure quality race
                })
                .collect();
            let picked = pick(&cands, &t, &mut rng);
            if s >= 500 && picked == 0 {
                best_hits += 1;
            }
            // Noiseless rating: performance tracks true quality.
            let perf = (4.0 * true_q[picked] - 2.0).round();
            ratings[picked].push(Rating {
                performance_points: perf,
                difficulty_points: 0.0,
                age_days: 0.0,
            });
        }
        // Over the last 100 sessions it should pick the winner well above the 25% random rate.
        assert!(best_hits > 70, "best-arm hits over last 100 = {best_hits}");
    }

    #[test]
    fn pruning_drops_a_hopeless_expensive_candidate_but_keeps_the_leader() {
        let t = Tuneables::default();
        let cands = vec![
            // cheap and well-rated
            Candidate { id: 0, track: TrackRecord { wins: 20.0, losses: 2.0 }, normalized_price: 0.0 },
            // priciest possible and badly rated → cannot win on value
            Candidate { id: 1, track: TrackRecord { wins: 2.0, losses: 20.0 }, normalized_price: 1.0 },
        ];
        let keep = prune_dominated(&cands, &t);
        assert!(keep.contains(&0), "the leader must be kept");
        assert!(!keep.contains(&1), "the hopeless expensive candidate should be pruned");
        assert!(!keep.is_empty());
    }

    #[test]
    fn pruning_keeps_genuinely_uncertain_candidates() {
        let t = Tuneables::default();
        // two blank-slate candidates at the same price: nothing is known, prune nothing.
        let cands = vec![
            Candidate { id: 0, track: TrackRecord::blank(), normalized_price: 0.5 },
            Candidate { id: 1, track: TrackRecord::blank(), normalized_price: 0.5 },
        ];
        assert_eq!(prune_dominated(&cands, &t).len(), 2);
    }

    #[test]
    fn hard_failure_is_not_a_win() {
        // Regression: perf -2 on a difficulty-4 task must score as a loss (< 0.5), not a win.
        // Before the credit-gating fix this was -2 + 0.75*4 = +1 -> logistic(0.5) ~ 0.62 (a "win").
        let t = Tuneables::default();
        let hard_fail = session_score(
            &Rating { performance_points: -2.0, difficulty_points: 4.0, age_days: 0.0 },
            &t,
        );
        assert!(hard_fail < 0.5, "hard failure scored {hard_fail} (should be a loss)");
        // It should match a plain failure of the same performance — difficulty gives no credit.
        let plain_fail = session_score(
            &Rating { performance_points: -2.0, difficulty_points: 0.0, age_days: 0.0 },
            &t,
        );
        assert_eq!(hard_fail, plain_fail);
    }

    #[test]
    fn hard_success_still_beats_trivial_success() {
        // The fairness correction is preserved: acing a hard task outranks acing a trivial one.
        let t = Tuneables::default();
        let hard_win = session_score(
            &Rating { performance_points: 2.0, difficulty_points: 4.0, age_days: 0.0 },
            &t,
        );
        let trivial_win = session_score(
            &Rating { performance_points: 2.0, difficulty_points: 0.0, age_days: 0.0 },
            &t,
        );
        assert!(hard_win > trivial_win);
    }

    proptest! {
        #[test]
        fn hard_task_credit_never_rescues_a_failure(
            perf in -2.0f64..=0.0, diff in 0.0f64..=4.0
        ) {
            // For any non-positive performance, adding difficulty must not change the score:
            // credit is gated on success, so a failure is scored on its own merit regardless of difficulty.
            let t = Tuneables::default();
            let with_diff = session_score(&Rating { performance_points: perf, difficulty_points: diff, age_days: 0.0 }, &t);
            let no_diff = session_score(&Rating { performance_points: perf, difficulty_points: 0.0, age_days: 0.0 }, &t);
            prop_assert_eq!(with_diff, no_diff);
        }

        #[test]
        fn session_score_in_unit_interval(
            perf in -2.0f64..=2.0, diff in 0.0f64..=4.0
        ) {
            let s = session_score(&Rating { performance_points: perf, difficulty_points: diff, age_days: 0.0 }, &Tuneables::default());
            prop_assert!(s > 0.0 && s < 1.0);
        }

        #[test]
        fn session_score_monotonic_in_performance(
            lo in -2.0f64..=1.0, delta in 0.01f64..=3.0, diff in 0.0f64..=4.0
        ) {
            let t = Tuneables::default();
            let a = session_score(&Rating { performance_points: lo, difficulty_points: diff, age_days: 0.0 }, &t);
            let b = session_score(&Rating { performance_points: lo + delta, difficulty_points: diff, age_days: 0.0 }, &t);
            prop_assert!(b > a);
        }

        #[test]
        fn normalize_prices_caps_at_one(prices in proptest::collection::vec(0.0f64..100.0, 1..12)) {
            let n = normalize_prices(&prices);
            prop_assert!(n.iter().all(|&p| (0.0..=1.0).contains(&p)));
        }
    }
}
