//! `blindcoder simulate` — the offline convergence harness.
//!
//! This is the project-killer test. Synthetic raters with a **known ground truth** drive the
//! *real* [`selector`] crate; we measure whether the selector actually converges onto the
//! best-value candidate (and how fast) versus staying a permanent random explorer. No model
//! calls, no blind to leak — just the decision core under a controlled world.
//!
//! World model per trial:
//!   * Each candidate has a hidden true quality `q ∈ (0,1)` and a shelf price.
//!   * The best candidate is the one maximizing the *true* value
//!     `q - cost_sensitivity * normalized_price`.
//!   * Each session: the selector picks; the world returns a noisy rating whose performance
//!     rises with the model's quality and falls with task difficulty (`difficulty_drag`). The
//!     selector's `difficulty_credit` is meant to correct that drag back out.
//!
//! We report cumulative regret, the best-candidate pick-rate over time, and time-to-converge,
//! averaged over independent trials — plus the random-choice baseline for context.

use clap::Args;
use config::Config;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};
use selector::{fold_track_record, normalize_prices, pick, value_score, Candidate, Rating, Tuneables};

#[derive(Args, Debug)]
pub struct SimulateArgs {
    /// Number of candidate models in the pool.
    #[arg(long, default_value_t = 8)]
    pub pool: usize,
    /// Sessions to simulate per trial.
    #[arg(long, default_value_t = 400)]
    pub sessions: usize,
    /// Independent trials to average over (different seeds).
    #[arg(long, default_value_t = 30)]
    pub trials: usize,
    /// Sessions per simulated day (sets the recency-decay clock).
    #[arg(long, default_value_t = 3.0)]
    pub rate: f64,
    /// Base RNG seed (each trial uses seed + trial index).
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// True difficulty drag: how much each difficulty point lowers raw performance.
    #[arg(long, default_value_t = 0.75)]
    pub difficulty_drag: f64,
    /// Rating observation noise (std-dev, in performance points).
    #[arg(long, default_value_t = 0.8)]
    pub noise: f64,
    /// Trailing window (sessions) for the best-arm pick-rate convergence check.
    #[arg(long, default_value_t = 50)]
    pub window: usize,
    /// Convergence threshold: best-arm pick-rate over the trailing window.
    #[arg(long, default_value_t = 0.7)]
    pub converge_at: f64,
}

struct TrueModel {
    quality: f64,
    raw_price: f64,
}

struct TrialResult {
    cumulative_regret: f64,
    final_window_best_rate: f64,
    time_to_converge: Option<usize>,
    /// Best-arm pick-rate at 25/50/75/100% checkpoints.
    checkpoints: [f64; 4],
}

/// Run the harness and print a report.
pub fn run(args: &SimulateArgs, cfg: &Config) -> anyhow::Result<()> {
    if args.pool == 0 || args.sessions == 0 || args.trials == 0 {
        anyhow::bail!("--pool, --sessions and --trials must all be >= 1");
    }
    let t = cfg.tuneables();

    let mut results = Vec::with_capacity(args.trials);
    let mut random_regret_sum = 0.0;
    for trial in 0..args.trials {
        let mut rng = StdRng::seed_from_u64(args.seed.wrapping_add(trial as u64));
        let (res, random_regret) = run_trial(args, &t, &mut rng);
        random_regret_sum += random_regret;
        results.push(res);
    }

    report(args, cfg, &t, &results, random_regret_sum / args.trials as f64);
    Ok(())
}

fn run_trial(args: &SimulateArgs, t: &Tuneables, rng: &mut StdRng) -> (TrialResult, f64) {
    // --- draw a hidden world ---
    let models: Vec<TrueModel> = (0..args.pool)
        .map(|_| TrueModel {
            quality: rng.gen_range(0.20..0.95),
            // Prices span cheap-open ($0.10) to premium ($15) per Mtok, log-uniform.
            raw_price: 10f64.powf(rng.gen_range(-1.0..1.18)),
        })
        .collect();
    let norm_price = normalize_prices(&models.iter().map(|m| m.raw_price).collect::<Vec<_>>());
    let true_value: Vec<f64> = (0..args.pool)
        .map(|i| value_score(models[i].quality, norm_price[i], t))
        .collect();
    let best = argmax(&true_value);
    let best_value = true_value[best];

    // Random-choice baseline: expected per-session regret if you picked uniformly.
    let mean_value = true_value.iter().sum::<f64>() / args.pool as f64;
    let random_regret_per_session = best_value - mean_value;

    // --- run the sessions ---
    let noise = Normal::new(0.0, args.noise.max(1e-9)).unwrap();
    let mut events: Vec<Vec<Rating>> = vec![Vec::new(); args.pool];
    // store (rating_time_days, perf, diff) so we can recompute ages each pick
    let mut raw_events: Vec<Vec<(f64, f64, f64)>> = vec![Vec::new(); args.pool];

    let mut cumulative_regret = 0.0;
    let mut hit_history = vec![false; args.sessions];
    let mut time_to_converge = None;

    for s in 0..args.sessions {
        let now_days = s as f64 / args.rate;

        // Fold each candidate's events into a fresh track record (ages relative to now).
        let cands: Vec<Candidate> = (0..args.pool)
            .map(|i| {
                events[i].clear();
                for &(ts, perf, diff) in &raw_events[i] {
                    events[i].push(Rating {
                        performance_points: perf,
                        difficulty_points: diff,
                        age_days: now_days - ts,
                    });
                }
                Candidate {
                    id: i,
                    track: fold_track_record(&events[i], t),
                    normalized_price: norm_price[i],
                }
            })
            .collect();

        let picked = pick(&cands, t, rng);
        cumulative_regret += best_value - true_value[picked];
        hit_history[s] = picked == best;

        // Convergence: first session after which the trailing window stays above threshold.
        if time_to_converge.is_none() && s + 1 >= args.window {
            let rate = window_rate(&hit_history, s + 1, args.window);
            if rate >= args.converge_at {
                time_to_converge = Some(s + 1);
            }
        }

        // World returns a noisy rating for the picked candidate.
        let difficulty = rng.gen_range(0..=4) as f64;
        // Baseline (difficulty 0) performance from quality: q=0 -> -2, q=1 -> +2.
        let base = 4.0 * models[picked].quality - 2.0;
        let latent = base - args.difficulty_drag * difficulty + noise.sample(rng);
        let perf = latent.round().clamp(-2.0, 2.0);
        raw_events[picked].push((now_days, perf, difficulty));
    }

    let final_window_best_rate = window_rate(&hit_history, args.sessions, args.window);
    let checkpoints = [
        window_rate(&hit_history, args.sessions / 4, args.window.min(args.sessions / 4).max(1)),
        window_rate(&hit_history, args.sessions / 2, args.window),
        window_rate(&hit_history, args.sessions * 3 / 4, args.window),
        final_window_best_rate,
    ];

    (
        TrialResult {
            cumulative_regret,
            final_window_best_rate,
            time_to_converge,
            checkpoints,
        },
        random_regret_per_session * args.sessions as f64,
    )
}

/// Best-arm pick-rate over the `window` sessions ending at `end` (exclusive upper bound).
fn window_rate(hits: &[bool], end: usize, window: usize) -> f64 {
    if end == 0 {
        return 0.0;
    }
    let start = end.saturating_sub(window);
    let slice = &hits[start..end];
    slice.iter().filter(|&&h| h).count() as f64 / slice.len() as f64
}

fn argmax(xs: &[f64]) -> usize {
    let mut best = 0;
    for i in 1..xs.len() {
        if xs[i] > xs[best] {
            best = i;
        }
    }
    best
}

fn mean(xs: impl Iterator<Item = f64>, n: usize) -> f64 {
    xs.sum::<f64>() / n as f64
}

fn report(args: &SimulateArgs, cfg: &Config, t: &Tuneables, results: &[TrialResult], random_regret: f64) {
    let n = results.len();
    let avg_regret = mean(results.iter().map(|r| r.cumulative_regret), n);
    let avg_final = mean(results.iter().map(|r| r.final_window_best_rate), n);
    let converged: Vec<usize> = results.iter().filter_map(|r| r.time_to_converge).collect();
    let converge_frac = converged.len() as f64 / n as f64;
    let median_ttc = median(&converged);
    let random_rate = 1.0 / args.pool as f64;

    let cp = |k: usize| mean(results.iter().map(|r| r.checkpoints[k]), n);

    println!("blindcoder simulate — selector convergence harness");
    println!("──────────────────────────────────────────────────");
    println!(
        "pool={}  sessions={}  trials={}  rate={}/day  difficulty_drag={}  noise={}",
        args.pool, args.sessions, args.trials, args.rate, args.difficulty_drag, args.noise
    );
    println!(
        "tuneables: cost_sensitivity={}  difficulty_credit={}  rating_half_life={}d  exploration={}  score_spread={}",
        t.cost_sensitivity, t.difficulty_credit, t.rating_half_life_days, t.exploration, t.score_spread
    );
    println!("config source: max_session_cost=${}  (curated_policy_max_age={}d)", cfg.max_session_cost_usd, cfg.curated_policy_max_age_days);
    println!();
    println!("best-arm pick-rate over training (trailing window = {} sessions):", args.window);
    println!("   25%: {:.2}   50%: {:.2}   75%: {:.2}   100%: {:.2}", cp(0), cp(1), cp(2), cp(3));
    println!("   random-choice baseline: {:.2}", random_rate);
    println!();
    println!("cumulative regret (lower = better):");
    println!("   selector: {:.2}", avg_regret);
    println!("   random baseline: {:.2}   → selector captures {:.0}% of the gap", random_regret, 100.0 * (1.0 - avg_regret / random_regret.max(1e-9)));
    println!();
    println!("convergence (best-arm rate ≥ {:.2} sustained):", args.converge_at);
    match median_ttc {
        Some(m) => println!("   {:.0}% of trials converged; median time-to-converge ≈ {} sessions ({:.1} days)", 100.0 * converge_frac, m, m as f64 / args.rate),
        None => println!("   {:.0}% of trials converged (no median available)", 100.0 * converge_frac),
    }
    println!("   final-window best-arm rate: {:.2}", avg_final);
    println!();
    let verdict = if avg_final >= args.converge_at && converge_frac >= 0.5 {
        "GO — selector converges at this pool size / session volume."
    } else if avg_final >= 2.0 * random_rate {
        "MARGINAL — learns, but does not reliably lock on; consider bounding the pool or retuning."
    } else {
        "NO-GO — selector stays near random; the design needs a smaller/bounded pool or stronger signal."
    };
    println!("verdict: {verdict}");
}

fn median(xs: &[usize]) -> Option<usize> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    Some(v[v.len() / 2])
}
