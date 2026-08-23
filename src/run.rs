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
use backend::{
    AbortReason, Backend, ErrorKind, Pick, ProxyBackend, SessionEvent, UsageSnapshot,
    VettedEndpoint,
};
use config::{CaptureLevel, Config, CostBasis, ModelConfig, Privacy, ProviderConfig};
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

/// The fail-closed privacy gate over the whole configured pool, run before any pick. Every provider
/// must (1) declare a `privacy` protocol, (2) point at that protocol's real endpoint host, (3) carry
/// only its *own* attestation keys (a Groq key on an OpenRouter provider is an error), and (4) for a
/// protocol that relies on unverifiable manual account setup, carry that provider's attestation set
/// to `true`. Any violation fails the run rather than silently forwarding.
fn validate_pool_privacy(cfg: &Config) -> Result<()> {
    for p in &cfg.providers {
        let Some(pv) = p.privacy else {
            anyhow::bail!(
                "provider {:?} has no `privacy` declaration — refusing to route to an \
                 unknown-data-policy endpoint (fail-closed). Declare its ZDR protocol.",
                p.slug
            );
        };

        // (2) The declaration is only valid for that provider's real endpoint. For an account-level
        // protocol (which does nothing on the wire) this host match *is* the enforcement. A
        // protocol with no host binding (`no-zdr`) always matches, so the mismatch arm always has a
        // host to name.
        if !pv.matches_endpoint(&p.base_url) {
            anyhow::bail!(
                "provider {:?}: base_url {:?} is not the {} endpoint that privacy = {:?} attests — \
                 an attestation is only valid for that provider's real endpoint.",
                p.slug,
                p.base_url,
                pv.endpoint_host().unwrap_or("(unbound)"),
                pv
            );
        }

        // The no-zdr consent fields are scoped to the no-zdr protocol; on any other provider they
        // are a misconfiguration (likely a copy-paste), refused rather than silently ignored.
        if pv != Privacy::NoZdr && (p.expires.is_some() || !p.non_zdr_attested_models.is_empty()) {
            anyhow::bail!(
                "provider {:?}: `expires` / the per-model non-ZDR attestation are only meaningful \
                 with privacy = \"no-zdr\" — remove them from this provider.",
                p.slug
            );
        }

        // (3) Reject foreign or unknown attestation keys — each key must belong to *this* provider's
        // protocol. Catches `groq_manual_steps_done` set on an OpenRouter provider (and typos).
        for key in p.attestations.keys() {
            match Privacy::for_attestation_key(key) {
                Some(owner) if owner == pv => {}
                Some(owner) => anyhow::bail!(
                    "provider {:?}: attestation key `{}` belongs to the {:?} privacy protocol, not \
                     this provider's {:?} — remove it.",
                    p.slug,
                    key,
                    owner,
                    pv
                ),
                None => anyhow::bail!("provider {:?}: unknown attestation key `{}`.", p.slug, key),
            }
        }

        // (4) A protocol that depends on manual setup blindcoder can't see requires an explicit,
        // provider-named attestation. The required key is revealed *only* here.
        if let Some(k) = pv.attestation_key() {
            if p.attestations.get(k) != Some(&true) {
                let steps = pv
                    .manual_steps()
                    .unwrap_or("(see the provider's data-controls docs)");
                anyhow::bail!(
                    "provider {:?} uses privacy = {:?}, whose Zero-Data-Retention blindcoder cannot \
                     verify on the wire — it depends on a manual account setup you must perform:\n\n\
                     \x20   {}\n\n\
                     Once done, confirm by adding `{} = true` to this provider in your config. (This \
                     key is intentionally not in the example config: it must be a deliberate act, \
                     not a copied default.)",
                    p.slug,
                    pv,
                    steps,
                    k
                );
            }
        }
    }
    Ok(())
}

/// (allow-non-zdr only) The environment second factor of the non-ZDR consent chain: must be set,
/// non-empty, at launch. A committed config can therefore never arm non-ZDR routing on its own.
/// The literal lives only in source and in the reveal error — never in docs, the example config,
/// or `--help`.
#[cfg(feature = "allow-non-zdr")]
const NON_ZDR_ENV_VAR: &str = "BLINDCODER_NON_ZDR_SESSION_OK";

/// (allow-non-zdr only) Hard cap on how far ahead a `no-zdr` provider's `expires` may be dated —
/// the maximum time the capability can stay armed per attestation. Revealed only when violated.
#[cfg(feature = "allow-non-zdr")]
const NON_ZDR_MAX_ARM_DAYS: i64 = 30;

/// The session-level non-ZDR disclosure text. Session-level ONLY: naming the alias (or the
/// per-request route) would deblind the harness — and because the operator cannot tell which
/// requests hit the non-ZDR arm, the whole session must be treated as non-private anyway.
/// Emitted once before launch, and re-asserted after a LAUNCHED CLI exits (see
/// [`disclosure_reassertion`] — a full-screen agentic TUI buries whatever preceded it).
#[cfg(feature = "allow-non-zdr")]
const NON_ZDR_DISCLOSURE: &str = "\
!! NON-ZDR SESSION: this pool contains a model on a non-ZDR endpoint — its provider may log or \
train on prompts. Which alias it is stays blind, so treat EVERYTHING sent in this session as \
non-private.";

/// The non-ZDR consent chain (docs/specs/non-zdr-pay-with-data-routing.md): completely dormant
/// unless a `privacy = "no-zdr"` provider is present in the parsed config (the env var and flag
/// are silently inert without one). When one is present, a single ordered check short-circuits at
/// the FIRST unmet gate, revealing only that gate's requirement — a config-level error can never
/// leak the env var or flag to someone who has not passed the config gates. Returns whether the
/// pool is armed (all gates passed). Pure in its inputs so the chain is testable against a fixed
/// clock and without touching process environment.
fn validate_non_zdr_gates(
    cfg: &Config,
    flag_passed: bool,
    env_present: bool,
    today_days: i64,
) -> Result<bool> {
    let non_zdr: Vec<&ProviderConfig> = cfg
        .providers
        .iter()
        .filter(|p| p.privacy == Some(Privacy::NoZdr))
        .collect();
    if non_zdr.is_empty() {
        return Ok(false);
    }

    // Gate 1 (documented): the routing path must be compiled in at all.
    #[cfg(not(feature = "allow-non-zdr"))]
    {
        let _ = (flag_passed, env_present, today_days);
        anyhow::bail!(
            "provider {:?} declares privacy = \"no-zdr\" — a pay-with-data endpoint whose provider \
             may log or train on prompts — but this build compiled that routing path out. \
             Rebuild with the `allow-non-zdr` Cargo feature to proceed.",
            non_zdr[0].slug
        );
    }

    #[cfg(feature = "allow-non-zdr")]
    {
        use std::collections::BTreeSet;

        for p in &non_zdr {
            // Gate 2: the per-model attestation — the exact real_slug of EVERY model under this
            // provider, so a provider cannot be opted out once and silently grow a second model.
            let key = Privacy::NoZdr
                .non_zdr_attestation_key()
                .expect("no-zdr defines its attestation key");
            if p.non_zdr_attested_models.is_empty() {
                anyhow::bail!(
                    "provider {:?} uses privacy = \"no-zdr\": its endpoint may log or train on \
                     every prompt sent to it, and blindcoder will not route there on the strength \
                     of a config value alone. Consent is per model — add\n\n\
                     \x20   {} = [\"…\"]\n\n\
                     to this provider, listing the exact `real_slug` of every model it offers.",
                    p.slug,
                    key
                );
            }
            let want: BTreeSet<&str> = p.models.iter().map(|m| m.real_slug.as_str()).collect();
            let have: BTreeSet<&str> = p
                .non_zdr_attested_models
                .iter()
                .map(String::as_str)
                .collect();
            if have != want {
                let missing: Vec<&&str> = want.difference(&have).collect();
                let extra: Vec<&&str> = have.difference(&want).collect();
                anyhow::bail!(
                    "provider {:?}: the non-ZDR model attestation must match this provider's \
                     models exactly, by `real_slug`. Unattested models: {:?}; attested but not \
                     among the provider's models: {:?}.",
                    p.slug,
                    missing,
                    extra
                );
            }

            // Gate 2½: a bounded lifetime. Absent → required; past → hard stop; too far ahead →
            // only now reveal the arming cap.
            let Some(expires) = p.expires.as_deref() else {
                anyhow::bail!(
                    "provider {:?}: a non-ZDR attestation must carry a bounded lifetime — add \
                     `expires = \"YYYY-MM-DD\"` to this provider.",
                    p.slug
                );
            };
            let Some(expiry_days) = config::date_to_epoch_days(expires) else {
                anyhow::bail!(
                    "provider {:?}: `expires` must be a \"YYYY-MM-DD\" date (got {:?}).",
                    p.slug,
                    expires
                );
            };
            if expiry_days < today_days {
                anyhow::bail!(
                    "provider {:?}: its non-ZDR attestation expired on {} — refusing to start. \
                     Renew the attestation deliberately if you still mean it.",
                    p.slug,
                    expires
                );
            }
            if expiry_days > today_days + NON_ZDR_MAX_ARM_DAYS {
                anyhow::bail!(
                    "provider {:?}: `expires` = {} is more than {} days out. A non-ZDR attestation \
                     may be dated at most {} days ahead — a hard cap on how long the capability \
                     stays armed, not a reminder. Refusing to start.",
                    p.slug,
                    expires,
                    NON_ZDR_MAX_ARM_DAYS,
                    NON_ZDR_MAX_ARM_DAYS
                );
            }
        }

        // Gate 3: the per-session environment second factor — lives in no file.
        if !env_present {
            anyhow::bail!(
                "the non-ZDR pool is configured and attested, but the per-session second factor is \
                 missing: set {NON_ZDR_ENV_VAR}=1 in the launching environment. A config file \
                 alone can never arm non-ZDR routing."
            );
        }
        // Gate 4: the per-invocation flag — the final deliberate act, hidden from --help.
        if !flag_passed {
            anyhow::bail!(
                "non-ZDR routing is one deliberate act away: pass --route-non-zdr-this-run on \
                 this invocation to route to a non-ZDR endpoint for this run."
            );
        }
        Ok(true)
    }
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

    if entries.is_empty() {
        anyhow::bail!("no candidates in the pool — no models are configured");
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

/// Whether the post-session wrap-up must RE-ASSERT [`NON_ZDR_DISCLOSURE`]: armed pool AND a
/// launched command. In launcher mode the pre-launch copy scrolls away the instant the agentic
/// CLI takes over the terminal with its own full-screen UI, so the wrap-up repeats it where the
/// operator reads the session summary and answers the still-blind rating prompt. A standing
/// proxy never covers the terminal, so its original banner stays visible and gets no repeat.
/// Pure so the launcher-only rule is unit-testable.
#[cfg(feature = "allow-non-zdr")]
fn disclosure_reassertion(
    non_zdr_armed: bool,
    launched_command: &[String],
) -> Option<&'static str> {
    (non_zdr_armed && !launched_command.is_empty()).then_some(NON_ZDR_DISCLOSURE)
}

/// Create `dir` (parents included) with the leaf forced to mode `0700`, tightening an existing,
/// looser leaf too. Used for the non-ZDR accountability log: even the file's existence discloses
/// that a pay-with-data endpoint is configured — a bit every local user could otherwise read off
/// a world-traversable directory listing. Parent directories keep default permissions; only the
/// leaf is private. Idempotent.
#[cfg(feature = "allow-non-zdr")]
fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = dir.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    if let Err(e) = std::fs::create_dir(dir) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e);
        }
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// `blindcoder run [cli args…]`: pick a blinded model and stand up the forwarding proxy. With a
/// command, launch it against the proxy (session ends when it exits, then rate inline); without
/// one, run a standing proxy you point a CLI at yourself (end with Ctrl-C).
pub fn run(cfg: &Config, args: &RunArgs) -> Result<()> {
    // The privacy gate runs first, over the whole configured pool, and fails the run on any
    // violation (fail-closed) — before any network, store write, or pick.
    validate_pool_privacy(cfg)?;

    // The non-ZDR consent chain (dormant unless a `no-zdr` provider is configured). The flag and
    // env-var reads exist only on the opt-in build; a default build passes inert values and the
    // chain can only refuse (or stay dormant).
    #[cfg(feature = "allow-non-zdr")]
    let (non_zdr_flag, non_zdr_env) = (
        args.route_non_zdr_this_run,
        std::env::var(NON_ZDR_ENV_VAR).is_ok_and(|v| !v.trim().is_empty()),
    );
    #[cfg(not(feature = "allow-non-zdr"))]
    let (non_zdr_flag, non_zdr_env) = (false, false);
    let non_zdr_armed =
        validate_non_zdr_gates(cfg, non_zdr_flag, non_zdr_env, config::today_epoch_days())?;
    if non_zdr_armed {
        // Session-level disclosure ONLY (see [`NON_ZDR_DISCLOSURE`]): naming the alias (or the
        // per-request route) would deblind the harness. This copy fires before launch; a launched
        // CLI buries it the moment it paints its full-screen UI, so run() re-asserts it once the
        // child exits and releases the terminal.
        // A default build can never get here (the chain above refuses), so the emission — and
        // with it the disclosure constant — exists only on the opt-in build.
        #[cfg(feature = "allow-non-zdr")]
        eprintln!("\n{NON_ZDR_DISCLOSURE}\n");
        #[cfg(not(feature = "allow-non-zdr"))]
        unreachable!("a default build cannot arm non-ZDR routing");
    }

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

    // The pool build already excluded any provider without a declaration (and any whose endpoint
    // host doesn't match it), so `None` here is unreachable — fail closed if it somehow occurs
    // rather than forward with an unknown policy.
    let privacy = provider.privacy.ok_or_else(|| {
        anyhow::anyhow!(
            "picked provider {:?} has no `privacy` declaration (should have been excluded)",
            provider.slug
        )
    })?;
    let backend = ProxyBackend::new(
        bind_addr,
        api_key,
        extra_headers,
        extra_body,
        privacy,
        capture_path,
    )?;
    // When the pick landed on the no-zdr arm, arm the fail-closed per-request accountability
    // trail (the gates above already passed or we would not be here). Aggregate accountability
    // only — UTC-hour bucket + real_slug, deliberately NO session id: the store keys ratings on
    // session_id, so an id-bearing audit file would join onto the ratings table and deblind the
    // session (and could be read mid-session to unmask it before rating). Unmasking stays the
    // reveal gate's job alone. The log lives in a 0700 directory: its mere existence admits a
    // pay-with-data endpoint is configured.
    #[cfg(feature = "allow-non-zdr")]
    let backend = if privacy == Privacy::NoZdr {
        let dir = config::default_state_dir()
            .context("cannot determine state dir (set XDG_STATE_HOME or HOME)")?;
        ensure_private_dir(&dir)
            .with_context(|| format!("creating {} for the non-ZDR audit trail", dir.display()))?;
        backend.with_non_zdr_audit(dir.join("non-zdr-audit.log"))
    } else {
        backend
    };
    // The forwarding target: the wire path takes a `VettedEndpoint`, and the only way to obtain the
    // `VettedRequest` it sends is `prepare`, which applies the privacy injection above.
    let endpoint = VettedEndpoint::new(route.base_url);
    let pick = Pick {
        canonical_key: route.canonical_key,
        real_slug: route.real_slug,
        endpoint,
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

    // Launcher mode buried the pre-launch disclosure (the child took over the terminal);
    // re-assert it now that the child has exited and released the terminal — the operator sees
    // it again directly above the summary and the rating prompt they answer while still blind.
    #[cfg(feature = "allow-non-zdr")]
    if let Some(disclosure) = disclosure_reassertion(non_zdr_armed, &args.command) {
        eprintln!("{disclosure}");
    }

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
            cached_prompt_tokens: outcome.cached_prompt_tokens.map(|t| t as i64),
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

/// Single-letter shortcuts per rating scale (case-insensitive), shown inline in the legend. The
/// letters are per-scale, so `t`/`e` mean different things on each (terrible vs trivial, excellent
/// vs easy) — the legend spells that out so it isn't a hidden collision.
const PERF_SHORTCUTS: &[(char, i64)] = &[('t', -2), ('p', -1), ('n', 0), ('g', 1), ('e', 2)];
const DIFF_SHORTCUTS: &[(char, i64)] = &[('t', 0), ('e', 1), ('m', 2), ('h', 3), ('v', 4)];

/// Prompt the two blind ratings on stdin after a launched session and record them. Enter on the
/// first question skips rating entirely. Each scale accepts a number or its single-letter shortcut.
fn prompt_and_rate(store: &Store, sid: i64) -> Result<()> {
    println!("  how did it perform?  -2 terrible(t) · -1 poor(p) · 0 neutral(n) · +1 good(g) · +2 excellent(e)");
    let Some(performance) = prompt_int_with_shortcuts(
        "  [-2..2, a letter, or Enter to skip]: ",
        -2,
        2,
        PERF_SHORTCUTS,
    )?
    else {
        println!("  rating skipped.");
        return Ok(());
    };

    println!("  how hard was the task?  0 trivial(t) · 1 easy(e) · 2 moderate(m) · 3 hard(h) · 4 very hard(v)");
    println!("    (rates the task, not the model; credits a good result on a hard task)");
    let difficulty =
        prompt_int_with_shortcuts("  [0..4 or a letter]: ", 0, 4, DIFF_SHORTCUTS)?.unwrap_or(0);
    let id = store.record_rating(sid, performance, difficulty, None)?;
    println!("  recorded rating #{id}.");
    Ok(())
}

/// Read an integer in `[lo, hi]` from stdin, with optional single-letter shortcuts.
/// Re-prompts on bad input. `None` = empty line / EOF.
/// Supports both numeric input and single-letter shortcuts when provided.
fn prompt_int_with_shortcuts(
    msg: &str,
    lo: i64,
    hi: i64,
    shortcuts: &[(char, i64)],
) -> Result<Option<i64>> {
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
        if let Some(v) = parse_rating(s, lo, hi, shortcuts) {
            return Ok(Some(v));
        }
        println!("  please enter a number in [{lo}..{hi}] or a shortcut letter.");
    }
}

/// Interpret one non-empty rating line: a whole number in `[lo, hi]`, or a single case-insensitive
/// letter mapped by `shortcuts`. Returns the value, or `None` if it matches neither (re-prompt).
/// Pure (no I/O) so the parse rules are unit-testable.
fn parse_rating(s: &str, lo: i64, hi: i64, shortcuts: &[(char, i64)]) -> Option<i64> {
    if let Ok(v) = s.parse::<i64>() {
        if (lo..=hi).contains(&v) {
            return Some(v);
        }
    }
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let c = c.to_ascii_lowercase();
        return shortcuts.iter().find(|(k, _)| *k == c).map(|&(_, v)| v);
    }
    None
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

/// The user's pi `models.json` (if readable) with a `blindcoder` provider merged in, pointing at
/// the proxy. Their other providers and settings are preserved untouched — only the `blindcoder`
/// key is ours to overwrite. The model id is the session **alias**, so pi displays the blinded
/// identity (e.g. `blindcoder/x7k2:q4m9`) just like the OpenCode injection; the matching
/// `--model` argument is appended by the adapter (see [`CliAdapter`]), never typed by the user.
/// `maxTokens` is pinned well below pi's default reservation because some gateways' per-minute
/// token gates count `prompt + max_tokens`, so a large default output reservation can reject a
/// request whose actual prompt would fit.
fn pi_models_json(existing: Option<&str>, base_url: &str, alias: &str) -> String {
    let mut root: serde_json::Value = existing
        .and_then(|s| serde_json::from_str(s).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let providers = root
        .as_object_mut()
        .expect("root is an object by construction")
        .entry("providers")
        .or_insert_with(|| serde_json::json!({}));
    if !providers.is_object() {
        *providers = serde_json::json!({});
    }
    providers
        .as_object_mut()
        .expect("providers forced to an object above")
        .insert(
            "blindcoder".to_string(),
            serde_json::json!({
                "baseUrl": base_url,
                "api": "openai-completions",
                "apiKey": "blindcoder",
                "models": [{ "id": alias, "contextWindow": 128000, "maxTokens": 4096 }]
            }),
        );
    serde_json::to_string_pretty(&root).expect("a Value serializes")
}

/// Populate the per-session pi agent dir injected via `PI_CODING_AGENT_DIR` — the pi counterpart
/// of [`opencode_config_content`], with the same merge-not-replace intent. pi only takes a config
/// *directory*, so the mechanism is a temp dir whose entries are symlinks into the user's real
/// agent dir: auth, settings, themes, extensions and sessions all still apply, and pi's writes
/// (including the extensions its self-extend loop produces) land in the real files. `models.json`
/// alone is a merged copy adding the proxy provider. `extensions/` and `sessions/` are created in
/// the real dir first if missing — a fresh install has neither, and without a real dir to symlink
/// the session's writes would be stranded in the temp dir and silently discarded.
///
/// Split from [`pi_agent_dir`] so tests can pass a fabricated real dir instead of racing on
/// `$HOME`.
fn populate_pi_agent_dir(
    dir: &std::path::Path,
    real: &std::path::Path,
    base_url: &str,
    alias: &str,
) -> std::io::Result<()> {
    for keep in ["extensions", "sessions"] {
        std::fs::create_dir_all(real.join(keep))?;
    }
    let mut existing = None;
    for entry in std::fs::read_dir(real)? {
        let entry = entry?;
        if entry.file_name() == "models.json" {
            existing = std::fs::read_to_string(entry.path()).ok();
        } else {
            std::os::unix::fs::symlink(entry.path(), dir.join(entry.file_name()))?;
        }
    }
    std::fs::write(
        dir.join("models.json"),
        pi_models_json(existing.as_deref(), base_url, alias),
    )
}

/// Build the injected pi agent dir from the user's `~/.pi/agent`. The returned guard removes the
/// temp dir (the symlinks and the merged `models.json`, never their targets) when dropped.
fn pi_agent_dir(base_url: &str, alias: &str) -> std::io::Result<tempfile::TempDir> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::other("HOME is not set; cannot locate ~/.pi/agent"))?;
    let real = std::path::PathBuf::from(home).join(".pi").join("agent");
    let dir = tempfile::Builder::new()
        .prefix("blindcoder-pi-")
        .tempdir()?;
    populate_pi_agent_dir(dir.path(), &real, base_url, alias)?;
    Ok(dir)
}

/// Which CLI-specific setup `run` applies in launcher mode. A *recognized* CLI gets complete
/// setup — endpoint, provider config, and model selection — so the bare command works with zero
/// flags and displays the blinded alias. Anything else gets only the universal contract
/// (`OPENAI_BASE_URL`/`OPENAI_API_KEY`): blindcoder cannot know an arbitrary CLI's config
/// surface, and a CLI that ignores those env vars needs its own adapter — this enum is the seam
/// to add one.
#[derive(Clone, Copy, PartialEq, Debug)]
enum CliAdapter {
    OpenCode,
    Pi,
    Generic,
}

impl CliAdapter {
    /// Detect by the executable's file name so both `pi` and `/some/path/to/pi` match.
    fn detect(argv0: &str) -> Self {
        match std::path::Path::new(argv0)
            .file_name()
            .and_then(|n| n.to_str())
        {
            Some("opencode") => Self::OpenCode,
            Some("pi") => Self::Pi,
            _ => Self::Generic,
        }
    }
}

/// True when the user already picked a model on pi's command line (`--model x` or `--model=x`),
/// in which case the adapter must not override it.
fn has_model_arg(args: &[String]) -> bool {
    args.iter()
        .any(|a| a == "--model" || a.starts_with("--model="))
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
        let adapter = CliAdapter::detect(&command[0]);
        let mut args: Vec<String> = command[1..].to_vec();
        let mut cmd = tokio::process::Command::new(&command[0]);
        // universal contract, honored by any env-respecting OpenAI-compatible CLI
        cmd.env("OPENAI_BASE_URL", &base_url)
            .env("OPENAI_API_KEY", "blindcoder");
        // recognized-CLI adapters add complete setup on top; the guard (pi) must outlive the
        // child — dropping it deletes the injected dir
        let mut _pi_dir = None;
        match adapter {
            CliAdapter::OpenCode => {
                cmd.env(
                    "OPENCODE_CONFIG_CONTENT",
                    opencode_config_content(&base_url, alias_display),
                );
            }
            CliAdapter::Pi => {
                // failure only degrades pi to the universal contract — warn and launch anyway
                match pi_agent_dir(&base_url, alias_display) {
                    Ok(dir) => {
                        cmd.env("PI_CODING_AGENT_DIR", dir.path());
                        _pi_dir = Some(dir);
                        if !has_model_arg(&args) {
                            args.push("--model".to_string());
                            args.push(format!("blindcoder/{alias_display}"));
                        }
                    }
                    Err(err) => eprintln!(
                        "blindcoder: pi config injection unavailable ({err}); \
                         pi would need a manual models.json pointing at {base_url}."
                    ),
                }
            }
            CliAdapter::Generic => {}
        }
        let mut child = cmd
            .args(&args)
            .spawn()
            .with_context(|| format!("failed to launch `{}`", command[0]))?;
        println!(
            "blindcoder: launched `{}` on a blinded session (pool of {pool_size}); ends when it exits.",
            command[0]
        );
        match adapter {
            CliAdapter::OpenCode | CliAdapter::Pi => {
                println!("  model shown in the CLI:  blindcoder/{alias_display}");
            }
            CliAdapter::Generic => {
                println!(
                    "  model to request:  {alias_display}   (any value works; the proxy rewrites it)"
                );
            }
        }
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
    // The final gate of the non-ZDR consent chain: per-invocation, hidden from --help, revealed
    // only by the startup error once every earlier gate has passed. (A regular comment, not a doc
    // comment — a doc comment would become clap help text.)
    #[cfg(feature = "allow-non-zdr")]
    #[arg(long, hide = true)]
    pub route_non_zdr_this_run: bool,
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
    use config::{Privacy, ProviderConfig};
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    #[test]
    fn pi_models_json_merges_and_preserves_user_providers() {
        // no existing file → just ours, keyed by the blinded alias
        let ours: serde_json::Value =
            serde_json::from_str(&pi_models_json(None, "http://127.0.0.1:1/v1", "x7k2:q4m9"))
                .unwrap();
        assert_eq!(
            ours["providers"]["blindcoder"]["baseUrl"],
            "http://127.0.0.1:1/v1"
        );
        assert_eq!(
            ours["providers"]["blindcoder"]["models"][0]["id"],
            "x7k2:q4m9"
        );

        // existing providers survive; an existing `blindcoder` entry is ours to overwrite
        let user = r#"{
            "providers": {
                "their-provider": { "baseUrl": "http://their.test/v1", "api": "openai-completions" },
                "blindcoder": { "baseUrl": "http://stale.test/v1" }
            }
        }"#;
        let merged: serde_json::Value = serde_json::from_str(&pi_models_json(
            Some(user),
            "http://127.0.0.1:2/v1",
            "x7k2:q4m9",
        ))
        .unwrap();
        assert_eq!(
            merged["providers"]["their-provider"]["baseUrl"],
            "http://their.test/v1"
        );
        assert_eq!(
            merged["providers"]["blindcoder"]["baseUrl"],
            "http://127.0.0.1:2/v1"
        );

        // corrupt / non-object input degrades to ours-only rather than erroring
        let recovered: serde_json::Value = serde_json::from_str(&pi_models_json(
            Some("not json"),
            "http://127.0.0.1:3/v1",
            "x7k2:q4m9",
        ))
        .unwrap();
        assert_eq!(
            recovered["providers"]["blindcoder"]["baseUrl"],
            "http://127.0.0.1:3/v1"
        );
    }

    #[test]
    fn cli_adapter_detects_by_file_name_and_model_arg_is_respected() {
        assert_eq!(CliAdapter::detect("opencode"), CliAdapter::OpenCode);
        assert_eq!(CliAdapter::detect("pi"), CliAdapter::Pi);
        assert_eq!(CliAdapter::detect("/nix/store/abc/bin/pi"), CliAdapter::Pi);
        assert_eq!(CliAdapter::detect("./aider"), CliAdapter::Generic);
        // "pi" as a path *component* is not a match
        assert_eq!(CliAdapter::detect("/opt/pi/aider"), CliAdapter::Generic);

        let none: Vec<String> = vec!["-p".into(), "hi".into()];
        assert!(!has_model_arg(&none));
        let flagged: Vec<String> = vec!["--model".into(), "their/model".into()];
        assert!(has_model_arg(&flagged));
        let joined: Vec<String> = vec!["--model=their/model".into()];
        assert!(has_model_arg(&joined));
    }

    #[test]
    fn populate_pi_agent_dir_symlinks_real_entries_and_merges_models() {
        let real = tempfile::tempdir().unwrap();
        let injected = tempfile::tempdir().unwrap();
        std::fs::write(real.path().join("auth.json"), r#"{"k":"v"}"#).unwrap();
        std::fs::write(
            real.path().join("models.json"),
            r#"{"providers":{"their-provider":{"baseUrl":"http://their.test/v1"}}}"#,
        )
        .unwrap();

        populate_pi_agent_dir(
            injected.path(),
            real.path(),
            "http://127.0.0.1:9/v1",
            "x7k2:q4m9",
        )
        .unwrap();

        // real entries are symlinked (writes propagate), models.json is a merged real file
        let auth = injected.path().join("auth.json");
        assert!(auth.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(auth).unwrap(), r#"{"k":"v"}"#);
        let models = injected.path().join("models.json");
        assert!(!models.symlink_metadata().unwrap().file_type().is_symlink());
        let merged: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(models).unwrap()).unwrap();
        assert_eq!(
            merged["providers"]["their-provider"]["baseUrl"],
            "http://their.test/v1"
        );
        assert_eq!(
            merged["providers"]["blindcoder"]["baseUrl"],
            "http://127.0.0.1:9/v1"
        );

        // extensions/ and sessions/ were created in the REAL dir (so a fresh install still gets
        // write-through for self-written extensions and session files) and symlinked in
        for keep in ["extensions", "sessions"] {
            assert!(real.path().join(keep).is_dir());
            assert!(injected
                .path()
                .join(keep)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink());
            // a write inside the injected dir lands in the real one
            std::fs::write(injected.path().join(keep).join("probe"), keep).unwrap();
            assert_eq!(
                std::fs::read_to_string(real.path().join(keep).join("probe")).unwrap(),
                keep
            );
        }
    }

    #[test]
    fn parse_rating_accepts_numbers_and_letter_shortcuts() {
        // numbers within range
        assert_eq!(parse_rating("1", -2, 2, PERF_SHORTCUTS), Some(1));
        assert_eq!(parse_rating("-2", -2, 2, PERF_SHORTCUTS), Some(-2));
        // out-of-range numbers re-prompt (None)
        assert_eq!(parse_rating("5", -2, 2, PERF_SHORTCUTS), None);
        assert_eq!(parse_rating("10", 0, 4, DIFF_SHORTCUTS), None);
        // letter shortcuts, case-insensitive
        assert_eq!(parse_rating("t", -2, 2, PERF_SHORTCUTS), Some(-2));
        assert_eq!(parse_rating("E", -2, 2, PERF_SHORTCUTS), Some(2));
        // the same letter maps differently per scale (terrible vs trivial, excellent vs easy)
        assert_eq!(parse_rating("t", 0, 4, DIFF_SHORTCUTS), Some(0));
        assert_eq!(parse_rating("e", 0, 4, DIFF_SHORTCUTS), Some(1));
        // unknown letter / multi-char / no shortcuts → None
        assert_eq!(parse_rating("z", -2, 2, PERF_SHORTCUTS), None);
        assert_eq!(parse_rating("ab", -2, 2, PERF_SHORTCUTS), None);
        assert_eq!(parse_rating("t", -2, 2, &[]), None);
    }

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

    fn provider_with(
        slug: &str,
        base_url: &str,
        privacy: Option<Privacy>,
        attest: &[(&str, bool)],
    ) -> ProviderConfig {
        ProviderConfig {
            slug: slug.into(),
            base_url: base_url.into(),
            privacy,
            attestations: attest.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            ..Default::default()
        }
    }
    fn cfg_of(p: ProviderConfig) -> Config {
        Config {
            providers: vec![p],
            ..Default::default()
        }
    }

    #[test]
    fn privacy_gate_openrouter_self_enforcing_ok() {
        let c = cfg_of(provider_with(
            "or",
            "https://openrouter.ai/api/v1",
            Some(Privacy::OpenRouter),
            &[],
        ));
        assert!(validate_pool_privacy(&c).is_ok());
    }

    #[test]
    fn privacy_gate_groq_requires_attestation_and_error_reveals_the_key() {
        // Real Groq endpoint but no attestation → refused, and the error reveals the exact key.
        let c = cfg_of(provider_with(
            "groq",
            "https://api.groq.com/openai/v1",
            Some(Privacy::Groq),
            &[],
        ));
        let err = validate_pool_privacy(&c).unwrap_err().to_string();
        assert!(
            err.contains("groq_manual_steps_done"),
            "error must reveal the key: {err}"
        );
        // With the attestation set → ok.
        let c = cfg_of(provider_with(
            "groq",
            "https://api.groq.com/openai/v1",
            Some(Privacy::Groq),
            &[("groq_manual_steps_done", true)],
        ));
        assert!(validate_pool_privacy(&c).is_ok());
    }

    #[test]
    fn privacy_gate_rejects_missing_declaration() {
        let c = cfg_of(provider_with(
            "x",
            "https://api.groq.com/openai/v1",
            None,
            &[],
        ));
        assert!(validate_pool_privacy(&c).is_err());
    }

    #[test]
    fn privacy_gate_rejects_host_mismatch() {
        // A Groq attestation for something that isn't the Groq endpoint → refused.
        let c = cfg_of(provider_with(
            "groq",
            "https://not-groq.example/v1",
            Some(Privacy::Groq),
            &[("groq_manual_steps_done", true)],
        ));
        assert!(validate_pool_privacy(&c).is_err());
    }

    #[test]
    fn privacy_gate_rejects_foreign_attestation_key() {
        // Groq's key set on an OpenRouter provider → error naming the owning protocol.
        let c = cfg_of(provider_with(
            "or",
            "https://openrouter.ai/api/v1",
            Some(Privacy::OpenRouter),
            &[("groq_manual_steps_done", true)],
        ));
        let err = validate_pool_privacy(&c).unwrap_err().to_string();
        assert!(err.contains("belongs to"), "{err}");
    }

    #[test]
    fn privacy_gate_rejects_unknown_attestation_key() {
        let c = cfg_of(provider_with(
            "groq",
            "https://api.groq.com/openai/v1",
            Some(Privacy::Groq),
            &[("groq_manual_steps_done", true), ("bogus", true)],
        ));
        assert!(validate_pool_privacy(&c).is_err());
    }

    /// One `no-zdr` provider with one placeholder model, with the attestation list and expiry as
    /// given. Placeholder slug only — no vendor or model name anywhere.
    fn no_zdr_cfg(attested: &[&str], expires: Option<&str>) -> Config {
        let p = ProviderConfig {
            slug: "pwd".into(),
            base_url: "https://api.example.test/v1".into(),
            privacy: Some(Privacy::NoZdr),
            non_zdr_attested_models: attested.iter().map(ToString::to_string).collect(),
            expires: expires.map(String::from),
            models: vec![ModelConfig {
                canonical_key: "non-zdr-model".into(),
                real_slug: "example/non-zdr-model".into(),
                input_per_mtok: Some(0.1),
                output_per_mtok: Some(0.4),
            }],
            ..Default::default()
        };
        cfg_of(p)
    }

    /// The chain is completely dormant without a `no-zdr` provider: even with the env var and flag
    /// supplied, nothing is checked and nothing is armed (they are silently inert).
    #[test]
    fn non_zdr_chain_is_dormant_without_a_no_zdr_provider() {
        let c = mixed_pool_config();
        assert!(!validate_non_zdr_gates(&c, true, true, 0).unwrap());
    }

    /// A `no-zdr` provider passes the generic pool gate (no host binding, no bool attestation);
    /// the consent chain, not `validate_pool_privacy`, is what stands in the way.
    #[test]
    fn pool_privacy_gate_accepts_a_no_zdr_declaration() {
        assert!(validate_pool_privacy(&no_zdr_cfg(&[], None)).is_ok());
    }

    /// The no-zdr consent fields are scoped to the no-zdr protocol — on any other provider they
    /// are refused, not ignored.
    #[test]
    fn pool_privacy_gate_rejects_non_zdr_fields_on_other_protocols() {
        let mut p = provider_with(
            "groq",
            "https://api.groq.com/openai/v1",
            Some(Privacy::Groq),
            &[("groq_manual_steps_done", true)],
        );
        p.expires = Some("2026-09-01".into());
        let err = validate_pool_privacy(&cfg_of(p)).unwrap_err().to_string();
        assert!(err.contains("no-zdr"), "{err}");
    }

    /// Reveal order 0: on a default (feature-less) build, a `no-zdr` provider is refused with the
    /// documented feature requirement — and none of the undocumented later gates leak.
    #[cfg(not(feature = "allow-non-zdr"))]
    #[test]
    fn default_build_reveals_only_the_feature_requirement() {
        // Even a fully attested config must stop at the feature gate.
        let c = no_zdr_cfg(&["example/non-zdr-model"], Some("2026-09-01"));
        let err = validate_non_zdr_gates(&c, true, true, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("allow-non-zdr"), "{err}");
        for later in [
            "no_zdr_models_i_accept_training_on",
            "expires",
            "BLINDCODER",
            "--route",
        ] {
            assert!(!err.contains(later), "must not leak {later:?}: {err}");
        }
    }

    /// The ordered, short-circuiting reveal chain: each run surfaces exactly one gate's
    /// requirement, never a later token before an earlier gate passes.
    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn non_zdr_chain_reveals_one_gate_per_run_in_order() {
        let today = config::date_to_epoch_days("2026-08-23").unwrap();
        let later_than_attestation = [
            "expires",
            "30",
            "BLINDCODER_NON_ZDR_SESSION_OK",
            "route-non-zdr-this-run",
        ];

        // 1: attestation absent → the key is revealed, nothing later.
        let err = validate_non_zdr_gates(&no_zdr_cfg(&[], None), true, true, today)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no_zdr_models_i_accept_training_on"), "{err}");
        assert!(
            err.contains("real_slug"),
            "the shape (exact slugs) is stated: {err}"
        );
        for later in later_than_attestation {
            assert!(!err.contains(later), "must not leak {later:?}: {err}");
        }

        // 1b: attestation present but not the exact slug set → the specific mismatch, no new token.
        let err = validate_non_zdr_gates(&no_zdr_cfg(&["example/other"], None), true, true, today)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("example/non-zdr-model"),
            "names the unattested model: {err}"
        );
        assert!(
            err.contains("example/other"),
            "names the stray attestation: {err}"
        );
        for later in ["expires", "BLINDCODER", "route-non-zdr"] {
            assert!(!err.contains(later), "must not leak {later:?}: {err}");
        }

        // 2: attested, no expiry → `expires` is required; the 30-day bound stays invisible.
        let ok_models = &["example/non-zdr-model"];
        let err = validate_non_zdr_gates(&no_zdr_cfg(ok_models, None), true, true, today)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expires"), "{err}");
        for later in ["30", "BLINDCODER", "route-non-zdr"] {
            assert!(!err.contains(later), "must not leak {later:?}: {err}");
        }

        // 2 (malformed): a non-date reveals the format, nothing later.
        let err = validate_non_zdr_gates(&no_zdr_cfg(ok_models, Some("soon")), true, true, today)
            .unwrap_err()
            .to_string();
        assert!(err.contains("YYYY-MM-DD"), "{err}");
        assert!(!err.contains("30"), "{err}");

        // 2b: expired → hard stop.
        let err = validate_non_zdr_gates(
            &no_zdr_cfg(ok_models, Some("2026-08-01")),
            true,
            true,
            today,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(!err.contains("BLINDCODER"), "{err}");

        // 2c: dated too far ahead → ONLY NOW the 30-day cap surfaces.
        let err = validate_non_zdr_gates(
            &no_zdr_cfg(ok_models, Some("2026-12-01")),
            true,
            true,
            today,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("30 days"), "{err}");
        assert!(!err.contains("BLINDCODER"), "{err}");
        // …while a compliant near-future date sails past without ever mentioning the bound.
        // Exactly 30 days out is compliant (a hard maximum, boundary included).
        let exactly_30 = validate_non_zdr_gates(
            &no_zdr_cfg(ok_models, Some("2026-09-22")),
            true,
            true,
            today,
        );
        assert!(exactly_30.is_ok(), "{exactly_30:?}");

        // 3: config gates all pass, env unset → the env var is revealed; the flag is not.
        let ok_cfg = no_zdr_cfg(ok_models, Some("2026-08-30"));
        let err = validate_non_zdr_gates(&ok_cfg, true, false, today)
            .unwrap_err()
            .to_string();
        assert!(err.contains("BLINDCODER_NON_ZDR_SESSION_OK"), "{err}");
        assert!(!err.contains("route-non-zdr"), "{err}");

        // 4: env set, flag missing → the flag is revealed.
        let err = validate_non_zdr_gates(&ok_cfg, false, true, today)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--route-non-zdr-this-run"), "{err}");

        // ✓: all four satisfied → armed.
        assert!(validate_non_zdr_gates(&ok_cfg, true, true, today).unwrap());
    }

    /// Requirement 9: the cost path is fully live for a `no-zdr` model — it is priced and
    /// normalized exactly like any other provider (non-ZDR does not imply free).
    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn non_zdr_model_is_priced_like_any_other() {
        let store = Store::open_in_memory().unwrap();
        let mut cfg = mixed_pool_config();
        cfg.providers
            .extend(no_zdr_cfg(&["example/non-zdr-model"], Some("2026-08-30")).providers);
        let mut rng = StdRng::seed_from_u64(5);
        seed_pool(&store, &cfg, &mut rng).unwrap();
        let (cands, entries) = build_pool(&store, &cfg).unwrap();
        let nz = entries
            .iter()
            .position(|e| e.provider_slug == "pwd")
            .unwrap();
        // 0.1 * 0.7 + 0.4 * 0.3 = 0.19 blended — a real, nonzero shelf price in the pool.
        assert!((entries[nz].raw_price - 0.19).abs() < 1e-12);
        assert!(cands[nz].normalized_price > 0.0);
        assert_eq!(
            entries[nz].input_per_mtok,
            Some(0.1),
            "cap estimation stays live"
        );
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
                    cached_prompt_tokens: None,
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

    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn disclosure_is_re_asserted_only_for_a_launched_armed_session() {
        // Armed + launched: the pre-launch copy was buried by the child's full-screen UI, so the
        // wrap-up must repeat it where the operator reads the summary and rates still blind.
        let launched = vec!["opencode".to_string()];
        let text =
            disclosure_reassertion(true, &launched).expect("armed launcher session must re-assert");
        assert!(text.contains("NON-ZDR SESSION"), "{text:?}");
        assert!(
            text.contains("treat EVERYTHING"),
            "the wording must keep the treat-everything instruction: {text:?}"
        );
        // Standing-proxy mode kept the original banner visible throughout: no repeat.
        assert_eq!(disclosure_reassertion(true, &[]), None);
        // Nothing armed → never any disclosure.
        assert_eq!(disclosure_reassertion(false, &launched), None);
    }

    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn audit_dir_gets_0700_even_when_it_pre_exists_looser() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state").join("blindcoder");

        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "fresh leaf must be 0700");

        // A directory created earlier by an older build at 0755 must be tightened: a loose
        // directory listing leaks the existence of the non-ZDR log.
        let loose = tmp.path().join("legacy");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&loose).unwrap();
        let mode = std::fs::metadata(&loose).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "pre-existing loose leaf must be tightened"
        );

        // Idempotent.
        ensure_private_dir(&loose).unwrap();
    }
}
