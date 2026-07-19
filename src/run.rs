//! `run` and `rate` — the daily-driver subcommands, wired to the real selector and store.
//!
//! `run` at M0 seeds the pool from config, folds effective ratings into candidates, makes a real
//! blind pick, resolves the route through the reveal gate, launches a streaming reverse proxy that
//! rewrites the blinded model to the real slug, streams responses back to the caller, and records
//! the session with cost / token usage. The proxy enforces a per-session cost cap and archives raw
//! wire data at the `replay` capture level. `rate` records (or corrects) a session's quality rating
//! post-hoc; corrections supersede earlier entries.

use anyhow::{Context, Result};
use clap::Args;
use rand::Rng;
use std::collections::HashMap;
use std::net::SocketAddr;

use alias::{mint_token, Alias, RevealGate, RevealReason, TOKEN_LEN};
use backend::{AbortReason, Backend, ErrorKind, Pick, ProxyBackend, SessionEvent, UsageSnapshot};
use config::{CaptureLevel, Config, CostBasis, ModelConfig, ProviderConfig};
use selector::{
    fold_track_record_with_failures, normalize_prices, pick, prune_dominated, Candidate, Failure,
    Rating, TrackRecord, Tuneables,
};
use store::Store;

/// One routable pool entry: a model at a provider, plus the alias that blinds it and its blended
/// shelf price. The selector `Candidate` built from this shares its track record with every other
/// entry of the same `canonical_key` (cross-provider), but keeps its own price.
struct PoolEntry {
    canonical_key: String,
    provider_slug: String,
    alias: Alias,
    raw_price: f64,
    /// Split shelf prices for this model at this provider (per Mtok); `None` = free. Used to
    /// estimate realized cost and to drive the mid-session cost cap.
    input_per_mtok: Option<f64>,
    output_per_mtok: Option<f64>,
}

/// Open the authoritative DB at `$XDG_DATA_HOME/blindcoder/blindcoder.db`.
fn open_store() -> Result<Store> {
    let dir = config::default_data_dir()
        .context("cannot determine data dir (set XDG_DATA_HOME or HOME)")?;
    Store::open(&dir.join("blindcoder.db"))
}

/// Resolve a provider's API key. The env var named by `key_env` wins when set and non-empty
/// (consistent with `flag > env > file`); otherwise the inlined `api_key` is used. If auth is
/// configured (either field present) but nothing resolves, that is a misconfiguration and errors;
/// a provider with neither field is treated as keyless (no `Authorization` header).
fn resolve_api_key(p: &ProviderConfig) -> Result<Option<String>> {
    if let Some(var) = &p.key_env {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Ok(Some(v));
            }
        }
    }
    if let Some(k) = &p.api_key {
        if !k.trim().is_empty() {
            return Ok(Some(k.clone()));
        }
    }
    if p.key_env.is_some() || p.api_key.is_some() {
        anyhow::bail!(
            "provider {}: no API key resolved — set the {} env var or inline api_key in config",
            p.slug,
            p.key_env.as_deref().unwrap_or("(unnamed)")
        );
    }
    Ok(None)
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
    let a = Alias {
        provider_token,
        model_token,
    };
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

    // Failed sessions also inform the belief (a crash is never rated). Map each error_kind to its
    // loss weight here — the policy layer — so the selector stays free of error semantics. An
    // unrecognised tag falls back to the `unknown` weight rather than being dropped.
    let mut fails_by_key: HashMap<String, Vec<Failure>> = HashMap::new();
    for f in store.effective_failures_aged()? {
        let loss_weight = ErrorKind::from_wire(&f.error_kind)
            .unwrap_or(ErrorKind::Unknown)
            .loss_weight();
        fails_by_key
            .entry(f.canonical_key)
            .or_default()
            .push(Failure {
                loss_weight,
                age_days: f.age_days,
            });
    }
    let no_ratings: Vec<Rating> = Vec::new();
    let no_fails: Vec<Failure> = Vec::new();

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
                input_per_mtok: m.input_per_mtok,
                output_per_mtok: m.output_per_mtok,
            });
        }
    }

    let norm = normalize_prices(&entries.iter().map(|e| e.raw_price).collect::<Vec<_>>());
    let cands = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let ratings = by_key.get(&e.canonical_key).unwrap_or(&no_ratings);
            let failures = fails_by_key.get(&e.canonical_key).unwrap_or(&no_fails);
            let track = if ratings.is_empty() && failures.is_empty() {
                TrackRecord::blank()
            } else {
                fold_track_record_with_failures(ratings, failures, &t)
            };
            Candidate {
                id: i,
                track,
                normalized_price: norm[i],
            }
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

/// `blindcoder run [cli args…]`: pick a blinded model and stand up the forwarding proxy. With a
/// command, launch it against the proxy (session ends when it exits, then rate inline); without
/// one, run a standing proxy you point a CLI at yourself (end with Ctrl-C).
pub fn run(cfg: &Config, args: &RunArgs) -> Result<()> {
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

    let provider = cfg
        .providers
        .iter()
        .find(|p| p.slug == entry.provider_slug)
        .context("picked provider is missing from config")?;

    // The one place blind→real happens: routing needs the real routing target. The lookup runs
    // inside the reveal gate (the single audited crossing point) and is journaled, so the crossing
    // stays auditable and the real identity never leaks to stdout.
    let cli_label = args.command.first().map_or("proxy", String::as_str);
    let sid = store.record_session_start(
        &alias_display,
        Some(cli_label),
        None,
        cfg.capture_level.as_str(),
    )?;
    let route = RevealGate
        .reveal(&entry.alias, RevealReason::Routing, |a| {
            store.resolve_route(&a.display()).ok().flatten()
        })
        .context("route must resolve for the picked alias")?;
    store.record_reveal(&alias_display, Some(sid), "routing")?;

    // Resolve the provider's credentials + passthrough hooks (all data, no provider branch).
    let api_key = resolve_api_key(provider)?;
    let extra_headers: Vec<(String, String)> = provider
        .extra_headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut extra_body = serde_json::Map::new();
    for (k, v) in &provider.extra_body {
        if let Ok(jv) = serde_json::to_value(v) {
            extra_body.insert(k.clone(), jv);
        }
    }

    let bind_addr: SocketAddr = cfg
        .proxy_addr
        .parse()
        .with_context(|| format!("invalid proxy_addr {:?} in config", cfg.proxy_addr))?;
    let in_price = entry.input_per_mtok.unwrap_or(0.0);
    let out_price = entry.output_per_mtok.unwrap_or(0.0);
    let cap = cfg.max_session_cost_usd;

    // At the `replay` capture level, archive the raw four-leg wire exchange to a disposable WARC
    // file outside the DB (referenced by convention, per the storage design). Gated so the default
    // `metadata` level writes nothing — no prompts or code ever leave the process.
    let capture_path = if cfg.capture_level >= CaptureLevel::Replay {
        let dir = config::default_state_dir()
            .context("cannot determine state dir (set XDG_STATE_HOME or HOME)")?
            .join("wire");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating wire archive dir {}", dir.display()))?;
        Some(dir.join(format!("{sid}.warc")))
    } else {
        None
    };

    let backend = ProxyBackend::new(bind_addr, api_key, extra_headers, extra_body, capture_path)?;
    let pick = Pick {
        canonical_key: route.canonical_key,
        real_slug: route.real_slug,
        base_url: route.base_url,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;
    let outcome = runtime.block_on(drive_session(DriveParams {
        backend: &backend,
        pick: &pick,
        alias_display: &alias_display,
        pool_size: entries.len(),
        cap,
        in_price,
        out_price,
        command: &args.command,
    }))?;

    // Record the terminal event: how it ended, and the realized cost — the provider-reported figure
    // when the transport captured one (authoritative), otherwise our tokens × shelf-price estimate.
    let prompt_tokens = outcome.prompt_tokens.unwrap_or(0);
    let completion_tokens = outcome.completion_tokens.unwrap_or(0);
    let (realized_cost, cost_source) = match outcome.realized_cost {
        Some(c) => (c, CostSource::Provider),
        None => (
            cost_usd(prompt_tokens, completion_tokens, in_price, out_price),
            CostSource::Estimate,
        ),
    };
    store.record_session_end(
        sid,
        &store::SessionEnd {
            realized_cost: Some(realized_cost),
            cost_source: Some(cost_source.as_str()),
            prompt_tokens: Some(prompt_tokens as i64),
            completion_tokens: Some(completion_tokens as i64),
            error_kind: outcome.error_kind.map(|e| e.as_str()),
            error_status: outcome.error_status,
            terminated_by: outcome.terminated_by.map(|r| r.as_str()),
        },
    )?;

    let ended = match outcome.terminated_by {
        Some(AbortReason::CostCap) => "stopped at the cost cap",
        Some(AbortReason::User) => "stopped by you",
        None => "ended",
    };
    let err_note = match (outcome.error_kind, outcome.error_status) {
        (Some(e), Some(code)) => format!(" [error: {} ({code})]", e.as_str()),
        (Some(e), None) => format!(" [error: {}]", e.as_str()),
        _ => String::new(),
    };
    println!(
        "\nsession #{sid} {ended}{err_note}: {prompt_tokens} in + {completion_tokens} out tokens, ${realized_cost:.4} ({}).",
        cost_source.as_str()
    );

    // Launcher mode ends when the CLI exits → rate inline (still blind). Standing mode leaves it to
    // the `rate` subcommand.
    if !args.command.is_empty() && !args.no_rate {
        if let Err(e) = prompt_and_rate(&store, sid) {
            eprintln!("  (rating skipped: {e})");
            println!("  rate later:  blindcoder rate --session {sid} --performance <-2..2> --difficulty <0..4>");
        }
    } else {
        println!(
            "  rate it:  blindcoder rate --session {sid} --performance <-2..2> --difficulty <0..4>"
        );
    }
    Ok(())
}

/// Legend for the interactive performance scale shown to users after a session.
const PERF_LEGEND: &str = "-2 terrible · -1 poor · 0 neutral · +1 good · +2 excellent";

/// Legend for the interactive difficulty scale shown to users after a session.
const DIFFICULTY_LEGEND: &str = "0 trivial · 1 easy · 2 moderate · 3 hard · 4 very hard";

/// Prompt the two blind ratings on stdin after a launched session and record them. Enter on the
/// first question skips rating entirely.
fn prompt_and_rate(store: &Store, sid: i64) -> Result<()> {
    println!("  performance?  {PERF_LEGEND}");
    println!("    (Enter to skip)\n");

    let Some(performance) = prompt_int("  how did it perform?  [-2..2, Enter to skip]: ", -2, 2)?
    else {
        println!("  rating skipped.");
        return Ok(());
    };

    println!("  difficulty?  {DIFFICULTY_LEGEND}");
    println!("    (rates the task, not the model; credits a good result on a hard task)\n");

    let difficulty = prompt_int("  how hard was the task?  [0..4]: ", 0, 4)?.unwrap_or(0);
    let id = store.record_rating(sid, performance, difficulty, None)?;
    println!("  recorded rating #{id}.");
    Ok(())
}

/// Read an integer in `[lo, hi]` from stdin, re-prompting on bad input. `None` = empty line / EOF.
fn prompt_int(msg: &str, lo: i64, hi: i64) -> Result<Option<i64>> {
    use std::io::Write;
    loop {
        print!("{msg}");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            return Ok(None); // EOF
        }
        let s = line.trim();
        if s.is_empty() {
            return Ok(None);
        }
        match s.parse::<i64>() {
            Ok(v) if (lo..=hi).contains(&v) => return Ok(Some(v)),
            _ => println!("  please enter a whole number in [{lo}..{hi}]."),
        }
    }
}

/// The OpenCode provider config injected via `OPENCODE_CONFIG_CONTENT` (merged into the user's
/// config for this child only — nothing is written to disk). A `blindcoder` provider points at the
/// proxy, and the model is keyed by the session **alias** so OpenCode displays the blinded identity
/// (e.g. `blindcoder/x7k2:q4m9`) and uses it by default — no manual config needed.
fn opencode_config_content(base_url: &str, alias: &str) -> String {
    serde_json::json!({
        "provider": {
            "blindcoder": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "blindcoder (blind router)",
                "options": { "baseURL": base_url, "apiKey": "blindcoder" },
                "models": { alias: { "name": alias } }
            }
        },
        "model": format!("blindcoder/{alias}")
    })
    .to_string()
}

/// Where the recorded `realized_cost` came from — the provider's inline figure (authoritative) or
/// our tokens × shelf-price estimate. Serialized to `session_end.cost_source` via [`as_str`].
#[derive(Clone, Copy)]
enum CostSource {
    Provider,
    Estimate,
}

impl CostSource {
    fn as_str(&self) -> &'static str {
        match self {
            CostSource::Provider => "provider",
            CostSource::Estimate => "estimate",
        }
    }
}

/// Estimate USD cost from token counts and per-Mtok shelf prices.
fn cost_usd(prompt_tokens: u64, completion_tokens: u64, in_price: f64, out_price: f64) -> f64 {
    (prompt_tokens as f64 / 1_000_000.0) * in_price
        + (completion_tokens as f64 / 1_000_000.0) * out_price
}

/// Spend so far for the cap: provider-reported cost when the transport captured one, else estimate.
fn spent_of(u: &UsageSnapshot, in_price: f64, out_price: f64) -> f64 {
    u.cost_so_far
        .unwrap_or_else(|| cost_usd(u.prompt_tokens, u.completion_tokens, in_price, out_price))
}

/// Parameters for [`drive_session`], grouped to keep the call site readable (and under clippy's
/// argument-count lint) since they are all distinct session-drive inputs.
struct DriveParams<'a> {
    backend: &'a ProxyBackend,
    pick: &'a Pick,
    alias_display: &'a str,
    pool_size: usize,
    cap: f64,
    in_price: f64,
    out_price: f64,
    command: &'a [String],
}

/// Stand up the proxy and drive the session. With a `command`, launch it against the proxy and end
/// when it exits; otherwise run a standing proxy the user points a CLI at (end with Ctrl-C). The
/// cost cap fires in either mode. Returns the terminal outcome.
async fn drive_session(params: DriveParams<'_>) -> Result<backend::SessionOutcome> {
    let DriveParams {
        backend,
        pick,
        alias_display,
        pool_size,
        cap,
        in_price,
        out_price,
        command,
    } = params;
    let mut sess = backend.start(pick, alias_display).await?;
    let endpoint = sess.endpoint().map_or_else(
        || "the configured proxy_addr".to_string(),
        |a| a.to_string(),
    );
    let base_url = format!("http://{endpoint}/v1");

    if command.is_empty() {
        // Standing-proxy mode: the user points a CLI at us and ends with Ctrl-C.
        println!("blindcoder: routing a blinded session (picked from a pool of {pool_size}).");
        println!("  point your OpenAI-compatible CLI at:  {base_url}");
        println!("  model to request:  {alias_display}   (any value works; the proxy rewrites it)");
        if cap > 0.0 {
            println!("  cost cap:  ${cap:.2} (session is halted if the estimate reaches it)");
        }
        println!("  press Ctrl-C to end the session and record it.");

        let mut ctrl_c = std::pin::pin!(tokio::signal::ctrl_c());
        let mut aborting = false;
        loop {
            let mut abort_reason = None;
            tokio::select! {
                event = sess.next_event() => match event? {
                    SessionEvent::Usage(u) => {
                        if !aborting && cap > 0.0 && spent_of(&u, in_price, out_price) >= cap {
                            abort_reason = Some(AbortReason::CostCap);
                        }
                    }
                    SessionEvent::Ended => break,
                },
                _ = &mut ctrl_c, if !aborting => { abort_reason = Some(AbortReason::User); }
            }
            if let Some(reason) = abort_reason {
                aborting = true;
                match reason {
                    AbortReason::CostCap => {
                        eprintln!("cost cap ${cap:.2} reached — stopping session.");
                    }
                    AbortReason::User => eprintln!("\nstopping session…"),
                }
                sess.abort(reason).await;
            }
        }
    } else {
        // Launcher mode: spawn the CLI against the proxy (env injects the endpoint + an OpenCode
        // provider so no manual config is needed); the session ends when the CLI exits.
        let mut cmd = tokio::process::Command::new(&command[0]);
        cmd.args(&command[1..])
            .env("OPENAI_BASE_URL", &base_url)
            .env("OPENAI_API_KEY", "blindcoder")
            .env(
                "OPENCODE_CONFIG_CONTENT",
                opencode_config_content(&base_url, alias_display),
            );
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to launch `{}`", command[0]))?;
        println!(
            "blindcoder: launched `{}` on a blinded session (pool of {pool_size}); ends when it exits.",
            command[0]
        );
        println!("  model shown in the CLI:  blindcoder/{alias_display}");
        if cap > 0.0 {
            println!("  cost cap:  ${cap:.2}");
        }

        let mut aborting = false;
        loop {
            let mut ended = false;
            let mut abort_reason = None;
            tokio::select! {
                _ = child.wait() => { ended = true; }
                event = sess.next_event() => match event? {
                    SessionEvent::Usage(u) => {
                        if !aborting && cap > 0.0 && spent_of(&u, in_price, out_price) >= cap {
                            abort_reason = Some(AbortReason::CostCap);
                        }
                    }
                    SessionEvent::Ended => { ended = true; }
                },
            }
            if let Some(reason) = abort_reason {
                aborting = true;
                eprintln!(
                    "\nblindcoder: cost cap ${cap:.2} reached — terminating `{}`.",
                    command[0]
                );
                sess.abort(reason).await;
                let _ = child.start_kill();
            }
            if ended {
                break;
            }
        }
        let _ = child.kill().await; // reap if still running
    }

    sess.finish().await
}

/// `blindcoder run [cli args…]` arguments.
#[derive(Args)]
pub struct RunArgs {
    /// Agentic CLI to launch on the blinded model (e.g. `opencode`), with its args. The session
    /// ends when the CLI exits and you rate it inline. Omit to run a standing proxy instead.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
    /// In launcher mode, skip the end-of-session rating prompt.
    #[arg(long)]
    pub no_rate: bool,
}

/// `blindcoder rate`: append a performance/difficulty rating for a past session (difficulty is
/// captured *after* the fact, artifact-framed). A correction supersedes rather than edits.
#[derive(Args)]
pub struct RateArgs {
    /// The session id to rate (see the id printed by `run`).
    #[arg(long)]
    pub session: i64,
    /// How well it performed, -2..=2 (-2 terrible · -1 poor · 0 neutral · +1 good · +2 excellent).
    #[arg(long, allow_hyphen_values = true)]
    pub performance: i64,
    /// How hard the task turned out to be, 0..=4 (0 trivial · 1 easy · 2 moderate · 3 hard · 4 very hard).
    #[arg(long)]
    pub difficulty: i64,
    /// If this corrects an earlier rating, its id (the old one is superseded, not deleted).
    #[arg(long)]
    pub supersedes: Option<i64>,
}

pub fn rate(args: &RateArgs) -> Result<()> {
    let store = open_store()?;
    let id = store
        .record_rating(
            args.session,
            args.performance,
            args.difficulty,
            args.supersedes,
        )
        .context(
            "failed to record rating (check the ranges: performance -2..=2, difficulty 0..=4)",
        )?;
    match args.supersedes {
        Some(old) => println!(
            "recorded rating #{id} for session #{} (supersedes #{old})",
            args.session
        ),
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
        Config {
            providers: vec![free, paid],
            ..Default::default()
        }
    }

    #[test]
    fn api_key_env_wins_then_falls_back_to_inline() {
        let var = "BLINDCODER_TEST_KEY_PRECEDENCE";
        let mut p = ProviderConfig {
            slug: "prov".into(),
            key_env: Some(var.into()),
            api_key: Some("inline-key".into()),
            ..Default::default()
        };
        // Env unset → inline is used.
        std::env::remove_var(var);
        assert_eq!(resolve_api_key(&p).unwrap().as_deref(), Some("inline-key"));
        // Env set and non-empty → env wins.
        std::env::set_var(var, "env-key");
        assert_eq!(resolve_api_key(&p).unwrap().as_deref(), Some("env-key"));
        // Empty env is ignored → inline again.
        std::env::set_var(var, "   ");
        assert_eq!(resolve_api_key(&p).unwrap().as_deref(), Some("inline-key"));
        std::env::remove_var(var);
        // Auth configured but nothing resolves → error.
        p.api_key = None;
        assert!(resolve_api_key(&p).is_err());
        // Neither field → keyless (no auth header), not an error.
        p.key_env = None;
        assert!(resolve_api_key(&p).unwrap().is_none());
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
        assert_eq!(
            free_alias.model_token, paid_alias.model_token,
            "same model → same model-token"
        );
        assert_ne!(
            free_alias.provider_token, paid_alias.provider_token,
            "different providers"
        );
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
        let free_i = entries
            .iter()
            .position(|e| e.provider_slug == "free-prov")
            .unwrap();
        let paid_i = entries
            .iter()
            .position(|e| e.provider_slug == "paid-prov")
            .unwrap();
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
        let free_alias = store
            .alias_for("model-x", "free-prov")
            .unwrap()
            .unwrap()
            .display();
        let sid = store
            .record_session_start(&free_alias, None, None, "metadata")
            .unwrap();
        store.record_rating(sid, 2, 0, None).unwrap();

        let (cands, entries) = build_pool(&store, &cfg).unwrap();
        let free_i = entries
            .iter()
            .position(|e| e.provider_slug == "free-prov")
            .unwrap();
        let paid_i = entries
            .iter()
            .position(|e| e.provider_slug == "paid-prov")
            .unwrap();
        assert!(
            cands[free_i].track.mean() > 0.5,
            "a positive rating lifts the track record"
        );
        assert_eq!(
            cands[free_i].track.mean(),
            cands[paid_i].track.mean(),
            "the track record is shared across providers for one canonical_key"
        );
    }

    #[test]
    fn a_failed_session_drags_the_shared_track_record_down() {
        let store = Store::open_in_memory().unwrap();
        let cfg = mixed_pool_config();
        let mut rng = StdRng::seed_from_u64(4);
        seed_pool(&store, &cfg, &mut rng).unwrap();

        // A too_large failure on the free provider's alias — never rated, but it must still be learned
        // against, keyed on canonical_key so both providers' entries for the model see it.
        let free_alias = store
            .alias_for("model-x", "free-prov")
            .unwrap()
            .unwrap()
            .display();
        let sid = store
            .record_session_start(&free_alias, None, None, "metadata")
            .unwrap();
        store
            .record_session_end(
                sid,
                &store::SessionEnd {
                    realized_cost: Some(0.0),
                    cost_source: Some("estimate"),
                    prompt_tokens: Some(0),
                    completion_tokens: Some(0),
                    error_kind: Some(ErrorKind::TooLarge.as_str()),
                    error_status: Some(413),
                    terminated_by: None,
                },
            )
            .unwrap();

        let (cands, entries) = build_pool(&store, &cfg).unwrap();
        let free_i = entries
            .iter()
            .position(|e| e.provider_slug == "free-prov")
            .unwrap();
        let paid_i = entries
            .iter()
            .position(|e| e.provider_slug == "paid-prov")
            .unwrap();
        assert!(
            cands[free_i].track.mean() < 0.5,
            "a failure drags the track record below the prior"
        );
        assert_eq!(
            cands[free_i].track.mean(),
            cands[paid_i].track.mean(),
            "the failure is shared across providers for one canonical_key"
        );

        // failure_sensitivity = 0 makes it inert: the candidate is back at the blank prior.
        let mut cfg0 = cfg.clone();
        cfg0.failure_sensitivity = 0.0;
        let (cands0, _) = build_pool(&store, &cfg0).unwrap();
        assert!(
            (cands0[free_i].track.mean() - 0.5).abs() < 1e-12,
            "sensitivity 0 ignores failures"
        );
    }
}
