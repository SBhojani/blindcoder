//! `blindcoder simulate` — the offline convergence harness (and `sweep`, its grid form).
//!
//! Synthetic raters with a **known ground truth** drive the *real* [`selector`] crate; we measure
//! whether the selector routes to good *value*, and how fast. No model calls, no blind to leak.
//!
//! Two views of "did it converge?":
//!   * **best-arm pick-rate** — how often it picks *the single* highest-value candidate. Strict:
//!     picking a near-tie #2 scores as a total miss.
//!   * **value capture** — a router only needs to capture *value*, not name the best arm. We
//!     report a within-ε "good-rate" (picked value within `value_epsilon` of the best) and a
//!     **value efficiency** (1 − regret / random-baseline regret), which is the honest headline.
//!
//! World model per trial:
//!   * candidate `i`: hidden `quality ∈ (0.20,0.95)`, `price` log-uniform over ≈ $0.10–$15/Mtok.
//!   * `true_value = quality − cost_sensitivity · normalized_price`; best = argmax.
//!   * rating: difficulty `d ~ U{0..4}`; `base = 4·quality − 2`;
//!     `latent = base − difficulty_drag·d + Normal(0,noise)`; `perf = round(clamp(latent,−2,2))`.
//!     `difficulty_credit` is meant to cancel `difficulty_drag`.

use clap::Args;
use config::Config;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};
use selector::{fold_track_record, normalize_prices, pick, value_score, Candidate, Rating, Tuneables};

// ---------------------------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------------------------

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
    /// Trailing window (sessions) for the convergence / rate metrics.
    #[arg(long, default_value_t = 50)]
    pub window: usize,
    /// Convergence threshold on the within-ε value good-rate.
    #[arg(long, default_value_t = 0.8)]
    pub converge_at: f64,
    /// A pick is "good" if its true value is within this of the best candidate's.
    #[arg(long, default_value_t = 0.05)]
    pub value_epsilon: f64,

    // --- selector tuneable overrides (fall back to config when unset) ---
    #[arg(long)]
    pub exploration: Option<f64>,
    #[arg(long)]
    pub cost_sensitivity: Option<f64>,
    #[arg(long)]
    pub score_spread: Option<f64>,
    #[arg(long)]
    pub difficulty_credit: Option<f64>,
    #[arg(long)]
    pub rating_half_life: Option<f64>,
}

#[derive(Args, Debug)]
pub struct SweepArgs {
    /// Pool sizes to sweep (comma-separated).
    #[arg(long, value_delimiter = ',', default_value = "3,4,5,6,8")]
    pub pools: Vec<usize>,
    /// Exploration values to sweep (comma-separated).
    #[arg(long, value_delimiter = ',', default_value = "0.5,0.7,1.0")]
    pub explorations: Vec<f64>,
    #[arg(long, default_value_t = 400)]
    pub sessions: usize,
    #[arg(long, default_value_t = 30)]
    pub trials: usize,
    #[arg(long, default_value_t = 3.0)]
    pub rate: f64,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    #[arg(long, default_value_t = 0.75)]
    pub difficulty_drag: f64,
    #[arg(long, default_value_t = 0.8)]
    pub noise: f64,
    #[arg(long, default_value_t = 50)]
    pub window: usize,
    #[arg(long, default_value_t = 0.8)]
    pub converge_at: f64,
    #[arg(long, default_value_t = 0.05)]
    pub value_epsilon: f64,
}

// ---------------------------------------------------------------------------------------------
// Harness core (shared by `simulate` and `sweep`)
// ---------------------------------------------------------------------------------------------

/// One fully-specified experiment.
struct Harness {
    pool: usize,
    sessions: usize,
    trials: usize,
    rate: f64,
    seed: u64,
    difficulty_drag: f64,
    noise: f64,
    window: usize,
    converge_at: f64,
    value_epsilon: f64,
    t: Tuneables,
}

/// Aggregated (trial-averaged) results.
struct Aggregate {
    /// Final-window rate of picking *the* best candidate.
    best_rate: f64,
    /// Final-window rate of picking a candidate within `value_epsilon` of the best.
    good_rate: f64,
    /// Final-window value efficiency: 1 − mean regret / random-baseline per-session regret.
    value_eff_window: f64,
    /// Cumulative value efficiency (share of the total regret gap captured vs random).
    gap_captured: f64,
    /// Fraction of trials whose trailing good-rate reached `converge_at`.
    converged_frac: f64,
    median_ttc: Option<usize>,
    /// Good-rate at 25/50/75/100 % checkpoints.
    cp_good: [f64; 4],
    /// Best-arm rate at the same checkpoints.
    cp_best: [f64; 4],
}

struct Trial {
    best_rate: f64,
    good_rate: f64,
    value_eff_window: f64,
    gap_captured: f64,
    ttc: Option<usize>,
    cp_good: [f64; 4],
    cp_best: [f64; 4],
}

fn run_config(h: &Harness) -> Aggregate {
    let mut trials = Vec::with_capacity(h.trials);
    for k in 0..h.trials {
        let mut rng = StdRng::seed_from_u64(h.seed.wrapping_add(k as u64));
        trials.push(run_trial(h, &mut rng));
    }
    aggregate(h, &trials)
}

fn run_trial(h: &Harness, rng: &mut StdRng) -> Trial {
    let t = &h.t;

    // hidden world
    let quality: Vec<f64> = (0..h.pool).map(|_| rng.gen_range(0.20..0.95)).collect();
    let raw_price: Vec<f64> = (0..h.pool).map(|_| 10f64.powf(rng.gen_range(-1.0..1.18))).collect();
    let norm_price = normalize_prices(&raw_price);
    let true_value: Vec<f64> = (0..h.pool).map(|i| value_score(quality[i], norm_price[i], t)).collect();
    let best = argmax(&true_value);
    let best_value = true_value[best];
    let mean_value = true_value.iter().sum::<f64>() / h.pool as f64;
    let random_regret_ps = best_value - mean_value; // expected per-session regret of random choice

    let noise = Normal::new(0.0, h.noise.max(1e-9)).unwrap();
    let mut raw_events: Vec<Vec<(f64, f64, f64)>> = vec![Vec::new(); h.pool];
    let mut buf: Vec<Rating> = Vec::new();

    let mut regret_hist = vec![0.0f64; h.sessions];
    let mut best_hist = vec![false; h.sessions];
    let mut good_hist = vec![false; h.sessions];
    let mut ttc = None;

    for s in 0..h.sessions {
        let now = s as f64 / h.rate;
        let cands: Vec<Candidate> = (0..h.pool)
            .map(|i| {
                buf.clear();
                for &(ts, perf, diff) in &raw_events[i] {
                    buf.push(Rating { performance_points: perf, difficulty_points: diff, age_days: now - ts });
                }
                Candidate { id: i, track: fold_track_record(&buf, t), normalized_price: norm_price[i] }
            })
            .collect();

        let picked = pick(&cands, t, rng);
        regret_hist[s] = best_value - true_value[picked];
        best_hist[s] = picked == best;
        good_hist[s] = true_value[picked] >= best_value - h.value_epsilon;

        if ttc.is_none() && s + 1 >= h.window && window_rate(&good_hist, s + 1, h.window) >= h.converge_at {
            ttc = Some(s + 1);
        }

        let d = rng.gen_range(0..=4) as f64;
        let base = 4.0 * quality[picked] - 2.0;
        let latent = base - h.difficulty_drag * d + noise.sample(rng);
        let perf = latent.round().clamp(-2.0, 2.0);
        raw_events[picked].push((now, perf, d));
    }

    let best_rate = window_rate(&best_hist, h.sessions, h.window);
    let good_rate = window_rate(&good_hist, h.sessions, h.window);
    let win_regret = window_mean(&regret_hist, h.sessions, h.window);
    let value_eff_window = if random_regret_ps < 1e-9 { 1.0 } else { 1.0 - win_regret / random_regret_ps };
    let cum_regret: f64 = regret_hist.iter().sum();
    let random_cum = random_regret_ps * h.sessions as f64;
    let gap_captured = if random_cum < 1e-9 { 1.0 } else { 1.0 - cum_regret / random_cum };

    let q = |frac: f64| ((h.sessions as f64 * frac) as usize).max(1);
    let cp_good = [
        window_rate(&good_hist, q(0.25), h.window),
        window_rate(&good_hist, q(0.50), h.window),
        window_rate(&good_hist, q(0.75), h.window),
        good_rate,
    ];
    let cp_best = [
        window_rate(&best_hist, q(0.25), h.window),
        window_rate(&best_hist, q(0.50), h.window),
        window_rate(&best_hist, q(0.75), h.window),
        best_rate,
    ];

    Trial { best_rate, good_rate, value_eff_window, gap_captured, ttc, cp_good, cp_best }
}

fn aggregate(h: &Harness, trials: &[Trial]) -> Aggregate {
    let n = trials.len() as f64;
    let avg = |f: fn(&Trial) -> f64| trials.iter().map(f).sum::<f64>() / n;
    let converged: Vec<usize> = trials.iter().filter_map(|t| t.ttc).collect();
    let mut cp_good = [0.0; 4];
    let mut cp_best = [0.0; 4];
    for t in trials {
        for k in 0..4 {
            cp_good[k] += t.cp_good[k] / n;
            cp_best[k] += t.cp_best[k] / n;
        }
    }
    let _ = h;
    Aggregate {
        best_rate: avg(|t| t.best_rate),
        good_rate: avg(|t| t.good_rate),
        value_eff_window: avg(|t| t.value_eff_window),
        gap_captured: avg(|t| t.gap_captured),
        converged_frac: converged.len() as f64 / n,
        median_ttc: median(&converged),
        cp_good,
        cp_best,
    }
}

/// Value-based verdict: a router needs to capture value, not name the single best arm.
fn verdict(a: &Aggregate) -> &'static str {
    if a.value_eff_window >= 0.90 && a.good_rate >= 0.80 {
        "GO — captures ~all achievable value and reliably lands within tolerance."
    } else if a.value_eff_window >= 0.70 {
        "MARGINAL — captures most of the value but leaves some on the table."
    } else {
        "NO-GO — leaves substantial value uncaptured; bound the pool / retune / stronger signal."
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

/// `simulate` — one config, a human-readable report.
pub fn run(args: &SimulateArgs, cfg: &Config) -> anyhow::Result<()> {
    if args.pool == 0 || args.sessions == 0 || args.trials == 0 {
        anyhow::bail!("--pool, --sessions and --trials must all be >= 1");
    }
    let t = effective_tuneables(
        cfg,
        args.exploration,
        args.cost_sensitivity,
        args.score_spread,
        args.difficulty_credit,
        args.rating_half_life,
    );
    let h = Harness {
        pool: args.pool,
        sessions: args.sessions,
        trials: args.trials,
        rate: args.rate,
        seed: args.seed,
        difficulty_drag: args.difficulty_drag,
        noise: args.noise,
        window: args.window,
        converge_at: args.converge_at,
        value_epsilon: args.value_epsilon,
        t,
    };
    let a = run_config(&h);
    let random_rate = 1.0 / h.pool as f64;

    println!("blindcoder simulate — selector convergence harness");
    println!("──────────────────────────────────────────────────");
    println!(
        "pool={}  sessions={}  trials={}  rate={}/day  difficulty_drag={}  noise={}  value_ε={}",
        h.pool, h.sessions, h.trials, h.rate, h.difficulty_drag, h.noise, h.value_epsilon
    );
    println!(
        "tuneables: cost_sensitivity={}  difficulty_credit={}  rating_half_life={}d  exploration={}  score_spread={}",
        t.cost_sensitivity, t.difficulty_credit, t.rating_half_life_days, t.exploration, t.score_spread
    );
    println!();
    println!("value capture (the router's actual job):");
    println!("   value efficiency (final window): {:.2}   (1.00 = perfect, 0.00 = random)", a.value_eff_window);
    println!("   cumulative gap captured vs random: {:.0}%", 100.0 * a.gap_captured);
    println!("   within-ε good-rate (final window): {:.2}", a.good_rate);
    println!("   good-rate over training  25%: {:.2}   50%: {:.2}   75%: {:.2}   100%: {:.2}", a.cp_good[0], a.cp_good[1], a.cp_good[2], a.cp_good[3]);
    println!();
    println!("best-arm identification (strict; picks THE best of {}):", h.pool);
    println!("   over training  25%: {:.2}   50%: {:.2}   75%: {:.2}   100%: {:.2}", a.cp_best[0], a.cp_best[1], a.cp_best[2], a.cp_best[3]);
    println!("   random-choice baseline: {:.2}", random_rate);
    println!();
    println!("convergence (good-rate ≥ {:.2} sustained over {} sessions):", h.converge_at, h.window);
    match a.median_ttc {
        Some(m) => println!("   {:.0}% of trials converged; median ≈ {} sessions ({:.1} days)", 100.0 * a.converged_frac, m, m as f64 / h.rate),
        None => println!("   {:.0}% of trials converged", 100.0 * a.converged_frac),
    }
    println!();
    println!("verdict: {}", verdict(&a));
    Ok(())
}

/// `sweep` — a grid over pool × exploration, CSV to stdout.
pub fn run_sweep(args: &SweepArgs, cfg: &Config) -> anyhow::Result<()> {
    if args.pools.is_empty() || args.explorations.is_empty() {
        anyhow::bail!("--pools and --explorations must each have at least one value");
    }
    eprintln!(
        "# sweep: sessions={} trials={} rate={}/day noise={} difficulty_drag={} value_ε={}",
        args.sessions, args.trials, args.rate, args.noise, args.difficulty_drag, args.value_epsilon
    );
    println!("pool,exploration,value_eff,gap_captured,good_rate,best_rate,converged_frac,median_ttc,verdict");
    for &pool in &args.pools {
        for &expl in &args.explorations {
            let t = effective_tuneables(cfg, Some(expl), None, None, None, None);
            let h = Harness {
                pool,
                sessions: args.sessions,
                trials: args.trials,
                rate: args.rate,
                seed: args.seed,
                difficulty_drag: args.difficulty_drag,
                noise: args.noise,
                window: args.window,
                converge_at: args.converge_at,
                value_epsilon: args.value_epsilon,
                t,
            };
            let a = run_config(&h);
            let tag = verdict(&a).split(' ').next().unwrap_or("?");
            let ttc = a.median_ttc.map(|m| m.to_string()).unwrap_or_else(|| "-".to_string());
            println!(
                "{},{:.2},{:.3},{:.3},{:.3},{:.3},{:.2},{},{}",
                pool, expl, a.value_eff_window, a.gap_captured, a.good_rate, a.best_rate, a.converged_frac, ttc, tag
            );
        }
    }
    Ok(())
}

fn effective_tuneables(
    cfg: &Config,
    exploration: Option<f64>,
    cost_sensitivity: Option<f64>,
    score_spread: Option<f64>,
    difficulty_credit: Option<f64>,
    rating_half_life: Option<f64>,
) -> Tuneables {
    let mut t = cfg.tuneables();
    if let Some(v) = exploration {
        t.exploration = v;
    }
    if let Some(v) = cost_sensitivity {
        t.cost_sensitivity = v;
    }
    if let Some(v) = score_spread {
        t.score_spread = v;
    }
    if let Some(v) = difficulty_credit {
        t.difficulty_credit = v;
    }
    if let Some(v) = rating_half_life {
        t.rating_half_life_days = v;
    }
    t
}

// ---------------------------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------------------------

fn window_rate(hits: &[bool], end: usize, window: usize) -> f64 {
    if end == 0 {
        return 0.0;
    }
    let start = end.saturating_sub(window);
    let slice = &hits[start..end];
    slice.iter().filter(|&&h| h).count() as f64 / slice.len() as f64
}

fn window_mean(xs: &[f64], end: usize, window: usize) -> f64 {
    if end == 0 {
        return 0.0;
    }
    let start = end.saturating_sub(window);
    let slice = &xs[start..end];
    slice.iter().sum::<f64>() / slice.len() as f64
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

fn median(xs: &[usize]) -> Option<usize> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_unstable();
    Some(v[v.len() / 2])
}
