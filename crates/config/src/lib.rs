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
        Self { input_weight: 0.70, output_weight: 0.30 }
    }
}

/// A backend provider record. `key_env` names an environment variable holding the API key so the
/// real key need never sit in the file.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProviderConfig {
    pub slug: String,
    pub base_url: String,
    #[serde(default = "default_wire")]
    pub wire: String,
    #[serde(default)]
    pub key_env: Option<String>,
}

fn default_wire() -> String {
    "openai".to_string()
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
    pub track_market: bool,
    pub price_refresh_interval_hours: f64,
    // --- safety knobs ---
    /// Per-session spend kill-switch (USD). 0 disables it.
    pub max_session_cost_usd: f64,
    /// Freshness bound (days) on hand-maintained ZDR/data-policy entries; past this a curated
    /// entry is treated as stale and its models are excluded (fail-closed).
    pub curated_policy_max_age_days: f64,
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
            track_market: false,
            price_refresh_interval_hours: 24.0,
            max_session_cost_usd: 5.0,
            curated_policy_max_age_days: 90.0,
            providers: Vec::new(),
        }
    }
}

impl Config {
    /// Build the effective config: defaults → TOML file (if given/found) → environment overlay.
    /// CLI-flag overrides are applied by the caller afterwards (highest precedence).
    pub fn load(explicit_path: Option<&Path>) -> anyhow::Result<Config> {
        let path = explicit_path.map(PathBuf::from).or_else(default_config_path);
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

fn xdg_dir(env_key: &str, home_suffix: &str) -> Option<PathBuf> {
    if let Ok(v) = std::env::var(env_key) {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(home_suffix))
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
}
