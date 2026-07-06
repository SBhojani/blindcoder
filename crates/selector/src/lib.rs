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
    /// <1 = exploit/commit faster; >1 = explore more (widens the posterior).
    pub exploration: f64,
    /// Logistic scale for squashing a rating into a 0..1 session score. Larger = gentler.
    pub score_spread: f64,
}

impl Default for Tuneables {
    fn default() -> Self {
        Self {
            cost_sensitivity: 0.5,
            difficulty_credit: 0.75,
            rating_half_life_days: 60.0,
            exploration: 1.0,
            score_spread: 2.0,
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

fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Map one rating to a session score in `(0, 1)`.
///
/// `adjusted_performance = performance_points + difficulty_credit * difficulty_points` — so
/// "fine on a hard task" can match or beat "great on a trivial task" — then squashed through a
/// logistic of scale `score_spread`.
pub fn session_score(r: &Rating, t: &Tuneables) -> f64 {
    let adjusted = r.performance_points + t.difficulty_credit * r.difficulty_points;
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
    let mut tr = TrackRecord::blank();
    for r in ratings {
        let decay = 0.5_f64.powf(r.age_days / t.rating_half_life_days);
        let s = session_score(r, t);
        tr.wins += s * decay;
        tr.losses += (1.0 - s) * decay;
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

    proptest! {
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
