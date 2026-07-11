//! `run` and `rate` — the daily-driver subcommands, wired to the real selector and store.
//!
//! `run` at M0 does everything *except* the actual byte-forwarding: it seeds the pool from config,
//! folds the effective ratings into candidates, makes a real blind pick, resolves the route through
//! the reveal gate, and records the session. The forwarding transport (the network piece) lands in
//! the next milestone behind the `Backend`/`Session` trait; until then `run` closes the session with
//! an honest `transport_unimplemented` tag rather than pretending a request was proxied.

use anyhow::{Context, Result};
use clap::Args;
use rand::Rng;
use std::collections::HashMap;

use alias::{mint_token, Alias, RevealGate, RevealReason, TOKEN_LEN};
use config::{Config, CostBasis, ModelConfig};
use selector::{
    fold_track_record, normalize_prices, pick, prune_dominated, Candidate, Rating, TrackRecord,
    Tuneables,
};
use store::Store;

/// One routable pool entry: a model at a provider, plus the alias that blinds it and its blended
/// shelf price. The selector `Candidate` built from this shares its track record with every other
/// entry of the same `canonical_key` (cross-provider), but keeps its own price.
struct PoolEntry {
    canonical_key: String,
    #[allow(dead_code)] // carried for diagnostics / the forthcoming transport; not read in M0
    provider_slug: String,
    alias: Alias,
    raw_price: f64,
}

/// Open the authoritative DB at `$XDG_DATA_HOME/blindcoder/blindcoder.db`.
fn open_store() -> Result<Store> {
    let dir = config::default_data_dir()
        .context("cannot determine data dir (set XDG_DATA_HOME or HOME)")?;
    Store::open(&dir.join("blindcoder.db"))
}

/// Blend input/output shelf prices into one number per the config cost basis. A model with no
/// prices (a free provider) blends to 0.0 — a zero-cost candidate.
fn blended_price(m: &ModelConfig, basis: &CostBasis) -> f64 {
    let inp = m.input_per_mtok.unwrap_or(0.0);
    let out = m.output_per_mtok.unwrap_or(0.0);
    inp * basis.input_weight + out * basis.output_weight
}

/// Mint (or reuse) the alias for one (model, provider). The model-token is shared across every
/// provider offering the same `canonical_key`; the provider-token is shared across every model of
/// the same provider — so blinded aliases still reveal cross-provider sameness without leaking the
/// real name.
fn ensure_alias<R: Rng + ?Sized>(
    store: &Store,
    canonical_key: &str,
    provider_slug: &str,
    rng: &mut R,
) -> Result<Alias> {
    if let Some(a) = store.alias_for(canonical_key, provider_slug)? {
        return Ok(a);
    }
    let model_token = match store.model_token_for(canonical_key)? {
        Some(t) => t,
        None => mint_token(rng, TOKEN_LEN),
    };
    let provider_token = match store.provider_token_for(provider_slug)? {
        Some(t) => t,
        None => mint_token(rng, TOKEN_LEN),
    };
    let a = Alias { provider_token, model_token };
    store.insert_alias(&a, canonical_key, provider_slug)?;
    Ok(a)
}

/// Reflect the config pool into the store: upsert providers/models, append changed prices, and mint
/// any missing aliases. Idempotent — safe to run on every `run`.
fn seed_pool<R: Rng + ?Sized>(store: &Store, cfg: &Config, rng: &mut R) -> Result<()> {
    for p in &cfg.providers {
        store.upsert_provider(&p.slug, &p.base_url, &p.wire)?;
        for m in &p.models {
            store.upsert_model(&m.canonical_key, &p.slug, &m.real_slug)?;
            store.record_price_if_changed(
                &m.canonical_key,
                &p.slug,
                m.input_per_mtok,
                m.output_per_mtok,
            )?;
            ensure_alias(store, &m.canonical_key, &p.slug, rng)?;
        }
    }
    Ok(())
}

/// Build the candidate pool: fold each model's effective ratings (by `canonical_key`, decayed) into
/// a track record, pair it with the entry's normalized price. Returns candidates aligned with the
/// entries by index.
fn build_pool(store: &Store, cfg: &Config) -> Result<(Vec<Candidate>, Vec<PoolEntry>)> {
    let t = cfg.tuneables();

    // Fold ratings once, grouped by the provider-neutral identity the selector learns on.
    let mut by_key: HashMap<String, Vec<Rating>> = HashMap::new();
    for r in store.effective_ratings_aged()? {
        by_key.entry(r.canonical_key).or_default().push(Rating {
            performance_points: r.performance_points,
            difficulty_points: r.difficulty_points,
            age_days: r.age_days,
        });
    }

    let mut entries = Vec::new();
    for p in &cfg.providers {
        for m in &p.models {
            let alias = store
                .alias_for(&m.canonical_key, &p.slug)?
                .context("alias must exist after seeding")?;
            entries.push(PoolEntry {
                canonical_key: m.canonical_key.clone(),
                provider_slug: p.slug.clone(),
                alias,
                raw_price: blended_price(m, &cfg.cost_basis),
            });
        }
    }

    let norm = normalize_prices(&entries.iter().map(|e| e.raw_price).collect::<Vec<_>>());
    let cands = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let track = by_key
                .get(&e.canonical_key)
                .map(|rs| fold_track_record(rs, &t))
                .unwrap_or_else(TrackRecord::blank);
            Candidate { id: i, track, normalized_price: norm[i] }
        })
        .collect();
    Ok((cands, entries))
}

/// Production pick: prune cost-dominated candidates, then Thompson-pick among the survivors.
/// Returns the index into the full candidate slice.
fn choose<R: Rng + ?Sized>(cands: &[Candidate], t: &Tuneables, rng: &mut R) -> usize {
    let active = prune_dominated(cands, t);
    let sub: Vec<Candidate> = active.iter().map(|&i| cands[i].clone()).collect();
    active[pick(&sub, t, rng)]
}

/// `blindcoder run`: pick a blinded model and record the session. Forwarding is the next milestone.
pub fn run(cfg: &Config) -> Result<()> {
    let store = open_store()?;
    let mut rng = rand::thread_rng();

    seed_pool(&store, cfg, &mut rng)?;
    let (cands, entries) = build_pool(&store, cfg)?;
    anyhow::ensure!(
        !cands.is_empty(),
        "no models configured — add [[providers.models]] entries to config.toml"
    );

    let t = cfg.tuneables();
    let idx = choose(&cands, &t, &mut rng);
    let entry = &entries[idx];
    let alias_display = entry.alias.display();

    let sid = store.record_session_start(&alias_display, Some("opencode"), None)?;

    // The one place blind→real happens: routing needs the real routing target. The lookup runs
    // inside the reveal gate (the single audited crossing point) and is journaled, so the crossing
    // stays auditable and the real identity never leaks to stdout.
    let route = RevealGate
        .reveal(&entry.alias, RevealReason::Routing, |a| {
            store.resolve_route(&a.display()).ok().flatten()
        })
        .context("route must resolve for the picked alias")?;
    store.record_reveal(&alias_display, Some(sid), "routing")?;
    let _ = &route; // consumed by the transport in the next milestone; resolved here to prove wiring

    println!("blindcoder: picked {alias_display} (blind) from a pool of {}", entries.len());
    println!("  session #{sid} recorded.");
    println!("  the forwarding transport lands next — no request was proxied this run.");

    store.record_session_end(sid, None, None, None, Some("transport_unimplemented"), None)?;
    Ok(())
}

/// `blindcoder rate`: append a performance/difficulty rating for a past session (difficulty is
/// captured *after* the fact, artifact-framed). A correction supersedes rather than edits.
#[derive(Args)]
pub struct RateArgs {
    /// The session id to rate (see the id printed by `run`).
    #[arg(long)]
    pub session: i64,
    /// How well it performed, -2..=2.
    #[arg(long, allow_hyphen_values = true)]
    pub performance: i64,
    /// How hard the task turned out to be, 0..=4.
    #[arg(long)]
    pub difficulty: i64,
    /// If this corrects an earlier rating, its id (the old one is superseded, not deleted).
    #[arg(long)]
    pub supersedes: Option<i64>,
}

pub fn rate(args: &RateArgs) -> Result<()> {
    let store = open_store()?;
    let id = store
        .record_rating(args.session, args.performance, args.difficulty, args.supersedes)
        .context("failed to record rating (check the ranges: performance -2..=2, difficulty 0..=4)")?;
    match args.supersedes {
        Some(old) => println!("recorded rating #{id} for session #{} (supersedes #{old})", args.session),
        None => println!("recorded rating #{id} for session #{}", args.session),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::ProviderConfig;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    /// A pool with one model offered by two providers: a free one (no prices) and a priced one.
    /// Placeholder names only — the code never branches on them, and neither should a reader.
    fn mixed_pool_config() -> Config {
        let free = ProviderConfig {
            slug: "free-prov".into(),
            base_url: "http://free.test/v1".into(),
            wire: "openai".into(),
            key_env: Some("FREE_PROV_KEY".into()),
            models: vec![ModelConfig {
                canonical_key: "model-x".into(),
                real_slug: "free-prov/model-x".into(),
                input_per_mtok: None, // free
                output_per_mtok: None,
            }],
            ..Default::default()
        };
        let paid = ProviderConfig {
            slug: "paid-prov".into(),
            base_url: "http://paid.test/v1".into(),
            wire: "openai".into(),
            key_env: Some("PAID_PROV_KEY".into()),
            models: vec![ModelConfig {
                canonical_key: "model-x".into(), // same model, priced, other provider
                real_slug: "paid-prov/model-x".into(),
                input_per_mtok: Some(0.55),
                output_per_mtok: Some(2.2),
            }],
            ..Default::default()
        };
        Config { providers: vec![free, paid], ..Default::default() }
    }

    #[test]
    fn seed_is_idempotent_and_shares_the_model_token() {
        let store = Store::open_in_memory().unwrap();
        let cfg = mixed_pool_config();
        let mut rng = StdRng::seed_from_u64(1);
        seed_pool(&store, &cfg, &mut rng).unwrap();
        seed_pool(&store, &cfg, &mut rng).unwrap(); // second run must not duplicate

        // Two aliases (one per provider) sharing one model-token.
        let n: i64 = store
            .conn
            .query_row("SELECT count(*) FROM aliases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
        let free_alias = store.alias_for("model-x", "free-prov").unwrap().unwrap();
        let paid_alias = store.alias_for("model-x", "paid-prov").unwrap().unwrap();
        assert_eq!(free_alias.model_token, paid_alias.model_token, "same model → same model-token");
        assert_ne!(free_alias.provider_token, paid_alias.provider_token, "different providers");
        // The free model records no price row; only the priced provider does.
        assert_eq!(store.latest_prices().unwrap().len(), 1);
    }

    #[test]
    fn build_pool_folds_ratings_and_prices_the_free_model_at_zero() {
        let store = Store::open_in_memory().unwrap();
        let cfg = mixed_pool_config();
        let mut rng = StdRng::seed_from_u64(2);
        seed_pool(&store, &cfg, &mut rng).unwrap();

        let (cands, entries) = build_pool(&store, &cfg).unwrap();
        assert_eq!(cands.len(), 2);
        // The free entry normalizes to price 0; the priced entry to 1 (pool max).
        let free_i = entries.iter().position(|e| e.provider_slug == "free-prov").unwrap();
        let paid_i = entries.iter().position(|e| e.provider_slug == "paid-prov").unwrap();
        assert_eq!(cands[free_i].normalized_price, 0.0);
        assert_eq!(cands[paid_i].normalized_price, 1.0);
        // No ratings yet → both candidates fold to the blank prior.
        assert!((cands[free_i].track.mean() - 0.5).abs() < 1e-12);
    }

    #[test]
    fn a_rating_moves_the_shared_track_record() {
        let store = Store::open_in_memory().unwrap();
        let cfg = mixed_pool_config();
        let mut rng = StdRng::seed_from_u64(3);
        seed_pool(&store, &cfg, &mut rng).unwrap();

        // Rate a session on the free provider's alias; the belief is keyed on canonical_key, so the
        // priced provider's entry for the same model must see the same lifted track record. Start
        // the session with the real alias so the ratings→aliases fold join resolves (sessions are
        // append-only, so the alias is set at creation, never updated).
        let free_alias = store.alias_for("model-x", "free-prov").unwrap().unwrap().display();
        let sid = store.record_session_start(&free_alias, None, None).unwrap();
        store.record_rating(sid, 2, 0, None).unwrap();

        let (cands, entries) = build_pool(&store, &cfg).unwrap();
        let free_i = entries.iter().position(|e| e.provider_slug == "free-prov").unwrap();
        let paid_i = entries.iter().position(|e| e.provider_slug == "paid-prov").unwrap();
        assert!(cands[free_i].track.mean() > 0.5, "a positive rating lifts the track record");
        assert_eq!(
            cands[free_i].track.mean(),
            cands[paid_i].track.mean(),
            "the track record is shared across providers for one canonical_key"
        );
    }
}
