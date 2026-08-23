//! blindcoder **config** — the declarative surface.
//!
//! `config.toml` declares the tuneables, the provider records, and the pool/eligibility rules.
//! Precedence is **flag > env > file > default**: [`Config::load`] builds defaults, overlays a
//! TOML file if present, then overlays `BLINDCODER_*` environment variables; the CLI layer
//! applies flag overrides last.
//!
//! Paths follow the XDG base-directory spec so the tool is OS-agnostic at runtime (no NixOS
//! assumptions): config in `$XDG_CONFIG_HOME/blindcoder/`, the authoritative DB in
//! `$XDG_DATA_HOME/blindcoder/`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which shelf price feeds the cost bias. Agentic context-resending makes *input* token volume
/// large even though it is cheaper per token, so the default blends 70:30 input:output.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct CostBasis {
    pub input_weight: f64,
    pub output_weight: f64,
}

impl Default for CostBasis {
    fn default() -> Self {
        Self {
            input_weight: 0.70,
            output_weight: 0.30,
        }
    }
}

/// How much of each session blindcoder captures — monotonic supersets. Deserializes from the
/// lowercase names; an unknown value is a config error (fails at load, not silently). `Ord` follows
/// declaration order, so `level >= CaptureLevel::Replay` etc. express the "at least this level" gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CaptureLevel {
    /// Model↔rating↔cost↔time only — no prompts or code on disk. The default privacy floor.
    #[default]
    Metadata,
    /// + parsed content projections stored in the DB.
    Contents,
    /// + the verbatim four-leg WARC archive on disk (full fidelity, re-runnable).
    Replay,
}

impl CaptureLevel {
    /// The wire/DB form (matches the serde `rename_all = "lowercase"` names and the DB `CHECK`
    /// set). Used at the persistence seam so the enum stays the in-memory type.
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureLevel::Metadata => "metadata",
            CaptureLevel::Contents => "contents",
            CaptureLevel::Replay => "replay",
        }
    }
}

/// One model offered by a provider. `canonical_key` is the provider-neutral identity the selector
/// learns on (so the same model under two providers shares a track record); `real_slug` is what the
/// provider's API actually expects in the request `model` field. Prices are optional — a free
/// provider simply omits them and competes as a zero-cost candidate.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    pub canonical_key: String,
    pub real_slug: String,
    #[serde(default)]
    pub input_per_mtok: Option<f64>,
    #[serde(default)]
    pub output_per_mtok: Option<f64>,
}

/// A backend provider record. `key_env` names an environment variable holding the API key so the
/// real key need never sit in the file.
///
/// The two passthrough hooks are what keep the proxy provider-blind: `extra_headers` and
/// `extra_body` are forwarded verbatim, so provider-specific knobs (attribution headers, a
/// ZDR/data-policy body flag, provider-routing preferences) live in config as data instead of as
/// branches in code. A bare provider needs neither; a gateway provider uses them for attribution
/// and privacy. Anything OpenAI-wire slots in the same way.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    pub slug: String,
    pub base_url: String,
    #[serde(default = "default_wire")]
    pub wire: String,
    #[serde(default)]
    pub key_env: Option<String>,
    /// API key inlined directly (convenience for a private, un-shared config). The env var named
    /// by `key_env` takes precedence when it is set and non-empty — consistent with the tool's
    /// `flag > env > file` precedence — so this is the fallback, and a shell export can still
    /// override it for a one-off. Prefer `key_env` for anything you might share.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Extra HTTP headers sent on every request to this provider.
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    /// JSON object merged (shallow, top-level) into every request body to this provider.
    #[serde(default)]
    pub extra_body: BTreeMap<String, toml::Value>,
    /// The provider's ZDR privacy basis. **Absent ⇒ the provider is ineligible** and the run is
    /// refused before any pick (fail-closed, design.md §Privacy). Present values decide how the
    /// transport shapes each request — see [`Privacy`].
    #[serde(default)]
    pub privacy: Option<Privacy>,
    /// Attestations that the operator completed the unverifiable, out-of-band manual setup a
    /// [`Privacy`] protocol depends on (e.g. enabling ZDR in the Groq console). Captured as
    /// *flattened extra keys* so each provider's attestation is a distinct, provider-named flag —
    /// e.g. `groq_manual_steps_done = true` — rather than one generic boolean. blindcoder cannot see
    /// the setup on the wire, so for such protocols it fails closed until the provider's key is set.
    ///
    /// **Intentionally omitted from `config.example.toml` and the docs.** The exact key name is
    /// revealed *only* by the fail-closed error, so setting it must be a deliberate act after
    /// reading the manual steps — never a value copied from a template.
    #[serde(flatten)]
    pub attestations: BTreeMap<String, bool>,
    /// Per-model consent for a `privacy = "no-zdr"` (pay-with-data) provider: must list the exact
    /// `real_slug` of every model under the provider. Deliberately typed as a slug list rather than
    /// a blanket boolean, so a provider cannot be opted out once and then silently grow a second
    /// model. Like the flattened attestations, the key is intentionally absent from
    /// `config.example.toml` and the docs — it is revealed only by the fail-closed startup error
    /// (see [`Privacy::non_zdr_attestation_key`]), and only after the build-feature gate passes.
    /// Rejected on any other privacy protocol. The field parses in every build so a default
    /// (feature-less) build fails with the feature-gate error, never a confusing parse error.
    #[serde(default, rename = "no_zdr_models_i_accept_training_on")]
    pub non_zdr_attested_models: Vec<String>,
    /// Bounded lifetime of a `no-zdr` provider's consent, as a `"YYYY-MM-DD"` date. Required for
    /// `privacy = "no-zdr"` (startup refuses when absent, expired, or dated too far ahead);
    /// rejected on any other privacy protocol.
    #[serde(default)]
    pub expires: Option<String>,
    /// The models this provider offers in the pool.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// Which provider's ZDR privacy protocol applies. Privacy is the one place blindcoder names
/// providers in code (everywhere else providers are just data): the mechanism for guaranteeing a
/// provider won't retain or train on prompts genuinely differs per vendor, and it is the
/// fail-closed security boundary — so each provider is an explicit, reviewed variant here rather
/// than a config blob that could fail *open*. Declaring one is what makes a provider *eligible*;
/// omitting it excludes the provider (fail-closed). A new provider cannot enter the pool without a
/// variant added here and a matching arm in the injector — forcing its privacy to be code-reviewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Privacy {
    /// OpenRouter — request-time ZDR: blindcoder injects
    /// `provider = { zdr = true, data_collection = "deny" }` into every request body, and OpenRouter
    /// itself fails closed (404) if no ZDR-compliant serving endpoint matches. Config value:
    /// `"open-router"`.
    OpenRouter,
    /// Groq — ZDR enabled at the account level (console data-controls); nothing is sent per request.
    /// Config value: `"groq"`.
    Groq,
    /// A non-ZDR / pay-with-data endpoint: the provider may log or train on prompts. Config value:
    /// `"no-zdr"`. Provider-agnostic — no endpoint-host binding and no wire injection; the
    /// enforcement is the multi-gate consent chain the router runs at startup (build feature +
    /// per-model attestation + expiry + environment second factor + invocation flag), not this
    /// variant. The variant itself parses in every build so a default (feature-less) build can
    /// refuse it with the feature-gate error instead of an opaque parse failure.
    NoZdr,
}

impl Privacy {
    /// The endpoint host this privacy protocol is verified against, if it binds one. An
    /// account-level attestation (e.g. Groq) does *nothing* on the wire, so it is only meaningful
    /// for that provider's real endpoint — the pool build refuses a provider whose `base_url` host
    /// doesn't match, rather than silently trusting an arbitrary endpoint that merely *claims* the
    /// protocol. For request-time protocols (OpenRouter) the check is defence-in-depth on top of the
    /// self-enforcing injection. `None` for `no-zdr`: it promises nothing about data retention, so
    /// there is no attestation to scope to a host — the consent chain, not host matching, gates it.
    pub fn endpoint_host(self) -> Option<&'static str> {
        match self {
            Privacy::OpenRouter => Some("openrouter.ai"),
            Privacy::Groq => Some("api.groq.com"),
            Privacy::NoZdr => None,
        }
    }

    /// Whether `base_url`'s host is this protocol's verified endpoint host (or a subdomain of it).
    /// A protocol with no host binding matches any endpoint.
    pub fn matches_endpoint(self, base_url: &str) -> bool {
        let Some(want) = self.endpoint_host() else {
            return true;
        };
        let host = host_of(base_url);
        host == want || host.strip_suffix(want).is_some_and(|p| p.ends_with('.'))
    }

    /// The out-of-band manual setup this protocol depends on but blindcoder cannot verify on the
    /// wire, if any. `Some` for account-level protocols whose ZDR is a console/account setting — the
    /// operator must attest completion via this protocol's [`attestation_key`](Self::attestation_key).
    /// `None` for self-enforcing request-time protocols (the injection + the provider's own
    /// fail-closed routing suffice).
    pub fn manual_steps(self) -> Option<&'static str> {
        match self {
            Privacy::OpenRouter | Privacy::NoZdr => None,
            Privacy::Groq => Some(
                "In the Groq console (console.groq.com), open Settings → Data Controls and enable \
                 Zero-Data-Retention for the account/API key you use here.",
            ),
        }
    }

    /// The **provider-specific** config key that attests this protocol's manual setup was done, if it
    /// needs one. Distinct per provider (includes the provider's name) so the attestation cannot be
    /// generic or copied — and so setting one provider's key on another is detectably wrong (see
    /// [`for_attestation_key`](Self::for_attestation_key)).
    pub fn attestation_key(self) -> Option<&'static str> {
        match self {
            Privacy::OpenRouter | Privacy::NoZdr => None,
            Privacy::Groq => Some("groq_manual_steps_done"),
        }
    }

    /// The **typed, per-model** attestation key of the `no-zdr` protocol — the config key of
    /// [`ProviderConfig::non_zdr_attested_models`], which must list the exact `real_slug` of every
    /// model under the provider. The reveal pattern of [`attestation_key`](Self::attestation_key)
    /// with a slug-list shape instead of a boolean: the literal lives only here (matching the
    /// field's serde rename — a test pins the two together) and in the fail-closed startup error,
    /// never in the example config or docs.
    pub fn non_zdr_attestation_key(self) -> Option<&'static str> {
        match self {
            Privacy::OpenRouter | Privacy::Groq => None,
            Privacy::NoZdr => Some("no_zdr_models_i_accept_training_on"),
        }
    }

    /// Which protocol, if any, owns `key`. Lets validation reject a key that belongs to a *different*
    /// provider's protocol (e.g. `groq_manual_steps_done` on an OpenRouter provider) or is unknown.
    pub fn for_attestation_key(key: &str) -> Option<Privacy> {
        [Privacy::OpenRouter, Privacy::Groq, Privacy::NoZdr]
            .into_iter()
            .find(|p| p.attestation_key() == Some(key))
    }
}

/// The host of a `scheme://host[:port]/path` URL, without pulling in a URL-parsing dependency.
fn host_of(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    host_port.split(':').next().unwrap_or(host_port)
}

fn default_wire() -> String {
    "openai".to_string()
}

/// Parse a strict `"YYYY-MM-DD"` calendar date into days since the Unix epoch (proleptic
/// Gregorian, Hinnant's civil-days algorithm — no time-of-day, no timezone, no date dependency).
/// `None` for anything else. Used for the `expires` bound on `no-zdr` providers; pure so the
/// expiry checks can be tested against a fixed "today".
pub fn date_to_epoch_days(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let yy = if m <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Today (UTC) in days since the Unix epoch — the one clock read callers pass into the pure
/// [`date_to_epoch_days`]-based expiry checks.
pub fn today_epoch_days() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| (d.as_secs() / 86_400) as i64)
}

/// The full application config. `#[serde(default)]` means any field missing from the TOML falls
/// back to its pinned default, so partial config files just work.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // --- selector tuneables (pinned defaults) ---
    pub cost_sensitivity: f64,
    pub cost_basis: CostBasis,
    pub provider_pooling: f64,
    pub difficulty_credit: f64,
    pub rating_half_life_days: f64,
    pub exploration: f64,
    pub score_spread: f64,
    /// Confidence width (posterior std-devs) for cost-dominance pruning; higher = prunes less.
    pub prune_confidence: f64,
    /// Global scale on failed-session loss evidence in the selector fold. 0 = ignore failures.
    pub failure_sensitivity: f64,
    pub track_market: bool,
    pub price_refresh_interval_hours: f64,
    // --- safety knobs ---
    /// Per-session spend kill-switch (USD). 0 disables it.
    pub max_session_cost_usd: f64,
    /// Freshness bound (days) on hand-maintained ZDR/data-policy entries; past this a curated
    /// entry is treated as stale and its models are excluded (fail-closed).
    pub curated_policy_max_age_days: f64,
    /// Local address the `run` proxy listens on; point your agentic CLI at `http://<this>/v1`.
    pub proxy_addr: String,
    /// How much of each session to capture (see [`CaptureLevel`]). Raising it above `metadata`
    /// writes your prompts/code to disk (on-box, `0600`); leave at the default unless you want that.
    pub capture_level: CaptureLevel,
    // --- backends ---
    pub providers: Vec<ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cost_sensitivity: 0.5,
            cost_basis: CostBasis::default(),
            provider_pooling: 0.3,
            difficulty_credit: 0.75,
            rating_half_life_days: 60.0,
            exploration: 0.4,
            score_spread: 2.0,
            prune_confidence: 2.0,
            failure_sensitivity: 1.0,
            track_market: false,
            price_refresh_interval_hours: 24.0,
            max_session_cost_usd: 5.0,
            curated_policy_max_age_days: 90.0,
            proxy_addr: "127.0.0.1:8787".to_string(),
            capture_level: CaptureLevel::Metadata,
            providers: Vec::new(),
        }
    }
}

impl Config {
    /// Build the effective config: defaults → TOML file (if given/found) → environment overlay.
    /// CLI-flag overrides are applied by the caller afterwards (highest precedence).
    pub fn load(explicit_path: Option<&Path>) -> anyhow::Result<Config> {
        let path = explicit_path
            .map(PathBuf::from)
            .or_else(default_config_path);
        let mut cfg = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(&p)?;
                toml::from_str(&text)?
            }
            _ => Config::default(),
        };
        cfg.apply_env();
        Ok(cfg)
    }

    /// Overlay a small set of `BLINDCODER_*` environment variables (env > file).
    pub fn apply_env(&mut self) {
        if let Some(v) = env_f64("BLINDCODER_COST_SENSITIVITY") {
            self.cost_sensitivity = v;
        }
        if let Some(v) = env_f64("BLINDCODER_EXPLORATION") {
            self.exploration = v;
        }
        if let Some(v) = env_f64("BLINDCODER_MAX_SESSION_COST") {
            self.max_session_cost_usd = v;
        }
        if let Some(v) = env_f64("BLINDCODER_RATING_HALF_LIFE_DAYS") {
            self.rating_half_life_days = v;
        }
    }

    /// Project the selector-relevant knobs into a [`selector::Tuneables`].
    pub fn tuneables(&self) -> selector::Tuneables {
        selector::Tuneables {
            cost_sensitivity: self.cost_sensitivity,
            difficulty_credit: self.difficulty_credit,
            rating_half_life_days: self.rating_half_life_days,
            exploration: self.exploration,
            score_spread: self.score_spread,
            prune_confidence: self.prune_confidence,
            failure_sensitivity: self.failure_sensitivity,
        }
    }
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|s| s.trim().parse().ok())
}

/// `$XDG_CONFIG_HOME/blindcoder/config.toml`, falling back to `$HOME/.config/...`.
pub fn default_config_path() -> Option<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config").map(|d| d.join("blindcoder").join("config.toml"))
}

/// `$XDG_DATA_HOME/blindcoder/`, falling back to `$HOME/.local/share/...`. The authoritative DB
/// lives here.
pub fn default_data_dir() -> Option<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share").map(|d| d.join("blindcoder"))
}

/// `$XDG_STATE_HOME/blindcoder/`, falling back to `$HOME/.local/state/...`. Disposable wire
/// archives (the `replay` capture level) live under `wire/` here — state, not portable data.
pub fn default_state_dir() -> Option<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state").map(|d| d.join("blindcoder"))
}

fn xdg_dir(env_key: &str, home_suffix: &str) -> Option<PathBuf> {
    if let Ok(v) = std::env::var(env_key) {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(home_suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_pinned_tuneables() {
        let c = Config::default();
        assert_eq!(c.cost_sensitivity, 0.5);
        assert_eq!(c.difficulty_credit, 0.75);
        assert_eq!(c.rating_half_life_days, 60.0);
        assert_eq!(c.max_session_cost_usd, 5.0);
        assert_eq!(c.curated_policy_max_age_days, 90.0);
    }

    #[test]
    fn partial_toml_falls_back_to_defaults() {
        let c: Config = toml::from_str("cost_sensitivity = 1.25\n").unwrap();
        assert_eq!(c.cost_sensitivity, 1.25);
        // untouched fields keep their pinned defaults
        assert_eq!(c.score_spread, 2.0);
        assert_eq!(c.curated_policy_max_age_days, 90.0);
    }

    #[test]
    fn privacy_and_flattened_attestation_parse_from_toml() {
        // The whole point of the undocumented key hinges on `#[serde(flatten)]` actually working
        // with the `toml` crate — verify the provider-named key lands in `attestations`.
        let toml_src = r#"
[[providers]]
slug = "groq"
base_url = "https://api.groq.com/openai/v1"
privacy = "groq"
groq_manual_steps_done = true

[[providers.models]]
canonical_key = "m"
real_slug = "groq/m"
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        let p = &c.providers[0];
        assert_eq!(p.privacy, Some(Privacy::Groq));
        assert_eq!(p.attestations.get("groq_manual_steps_done"), Some(&true));
    }

    #[test]
    fn privacy_kebab_value_parses() {
        let c: Config = toml::from_str(
            "[[providers]]\nslug=\"o\"\nbase_url=\"https://openrouter.ai/api/v1\"\nprivacy=\"open-router\"\n",
        )
        .unwrap();
        assert_eq!(c.providers[0].privacy, Some(Privacy::OpenRouter));
    }

    #[test]
    fn matches_endpoint_rejects_lookalike_hosts() {
        assert!(Privacy::Groq.matches_endpoint("https://api.groq.com/openai/v1"));
        assert!(Privacy::OpenRouter.matches_endpoint("https://openrouter.ai/api/v1"));
        // a real subdomain is fine
        assert!(Privacy::OpenRouter.matches_endpoint("https://gateway.openrouter.ai/v1"));
        // suffix / look-alike / infix attacks are rejected
        assert!(!Privacy::OpenRouter.matches_endpoint("https://openrouter.ai.evil.com/v1"));
        assert!(!Privacy::OpenRouter.matches_endpoint("https://evilopenrouter.ai/v1"));
        assert!(!Privacy::Groq.matches_endpoint("https://api.groq.com.attacker.net/v1"));
    }

    #[test]
    fn attestation_key_ownership() {
        assert_eq!(
            Privacy::Groq.attestation_key(),
            Some("groq_manual_steps_done")
        );
        assert_eq!(Privacy::OpenRouter.attestation_key(), None);
        assert_eq!(Privacy::NoZdr.attestation_key(), None);
        assert_eq!(
            Privacy::for_attestation_key("groq_manual_steps_done"),
            Some(Privacy::Groq)
        );
        assert_eq!(Privacy::for_attestation_key("nope"), None);
    }

    #[test]
    fn no_zdr_parses_with_its_typed_attestation_and_expiry() {
        // The literal key comes from the reveal method, so this test pins the serde rename and the
        // revealed string together — they can never drift apart. Placeholder slug only.
        let key = Privacy::NoZdr.non_zdr_attestation_key().unwrap();
        let toml_src = format!(
            r#"
[[providers]]
slug = "pwd"
base_url = "https://api.example.test/v1"
privacy = "no-zdr"
{key} = ["example/non-zdr-model"]
expires = "2026-09-01"

[[providers.models]]
canonical_key = "non-zdr-model"
real_slug = "example/non-zdr-model"
input_per_mtok = 0.1
output_per_mtok = 0.4
"#
        );
        let c: Config = toml::from_str(&toml_src).unwrap();
        let p = &c.providers[0];
        assert_eq!(p.privacy, Some(Privacy::NoZdr));
        assert_eq!(p.non_zdr_attested_models, vec!["example/non-zdr-model"]);
        assert_eq!(p.expires.as_deref(), Some("2026-09-01"));
        assert!(
            p.attestations.is_empty(),
            "typed fields never leak into the flatten map"
        );
        // The cost path sees a normally priced model — non-ZDR does not imply free.
        assert_eq!(p.models[0].input_per_mtok, Some(0.1));
    }

    #[test]
    fn no_zdr_binds_no_endpoint_host() {
        assert_eq!(Privacy::NoZdr.endpoint_host(), None);
        assert!(Privacy::NoZdr.matches_endpoint("https://anything.example/v1"));
        // The bound protocols keep their binding (Option-wrapped, same hosts).
        assert_eq!(Privacy::Groq.endpoint_host(), Some("api.groq.com"));
        assert_eq!(Privacy::OpenRouter.endpoint_host(), Some("openrouter.ai"));
    }

    #[test]
    fn date_to_epoch_days_is_exact_and_strict() {
        assert_eq!(date_to_epoch_days("1970-01-01"), Some(0));
        assert_eq!(date_to_epoch_days("1970-01-02"), Some(1));
        assert_eq!(date_to_epoch_days("2000-01-01"), Some(10_957));
        assert_eq!(date_to_epoch_days("2000-03-01"), Some(11_017));
        // strict YYYY-MM-DD only
        for bad in [
            "2026-9-01",
            "2026-09-1",
            "26-09-01",
            "2026-09-01T00:00:00Z",
            "2026-13-01",
            "2026-00-10",
            "2026-01-32",
            "soon",
            "",
        ] {
            assert_eq!(date_to_epoch_days(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn parses_a_mixed_free_and_priced_pool() {
        // free-prov: free (no prices), no passthrough. paid-prov: priced, with an attribution
        // header + a provider-routing body flag. Both offer the same model under different slugs.
        let toml_src = r#"
[[providers]]
slug = "free-prov"
base_url = "http://free.test/v1"
key_env = "FREE_PROV_KEY"

[[providers.models]]
canonical_key = "model-x"
real_slug = "free-prov/model-x"

[[providers]]
slug = "paid-prov"
base_url = "http://paid.test/v1"
key_env = "PAID_PROV_KEY"
extra_headers = { "X-Title" = "blindcoder" }
extra_body = { "provider" = { "require_parameters" = true } }

[[providers.models]]
canonical_key = "model-x"
real_slug = "paid-prov/model-x"
input_per_mtok = 0.55
output_per_mtok = 2.2
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(c.providers.len(), 2);

        let free = &c.providers[0];
        assert_eq!(free.wire, "openai"); // defaulted
        assert!(free.extra_headers.is_empty() && free.extra_body.is_empty());
        assert_eq!(free.models[0].real_slug, "free-prov/model-x");
        assert!(free.models[0].input_per_mtok.is_none()); // free

        let paid = &c.providers[1];
        assert_eq!(
            paid.extra_headers.get("X-Title").map(String::as_str),
            Some("blindcoder")
        );
        assert!(paid.extra_body.contains_key("provider"));
        // Same canonical_key under both providers — the cross-provider identity the selector shares.
        assert_eq!(paid.models[0].canonical_key, free.models[0].canonical_key);
        assert_eq!(paid.models[0].output_per_mtok, Some(2.2));
    }
}
