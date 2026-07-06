//! blindcoder **backend** — the load-bearing seam.
//!
//! Everything *above* this trait (selector, store, config, aliasing, the CLI) is real from day
//! one and never touched again. Everything *below* it grows inside this one crate: at M0 the
//! transport is a trivial model-rewrite proxy; M1 adds the raw-capture tee and the fail-closed
//! per-request privacy flags behind the *same* `run_session` forward path — no rewrite.

use anyhow::Result;

/// The chosen candidate for a session. The real slug is present here because the transport needs
/// it to route; it reaches this struct only via the alias reveal gate (reason: routing).
#[derive(Clone, Debug)]
pub struct Pick {
    pub canonical_key: String,
    pub real_slug: String,
    pub base_url: String,
}

/// What a finished session reports back — the `metadata`-floor signal the selector learns from.
/// No prompt/code: just tokens, realized cost, and an optional error tag.
#[derive(Clone, Debug, Default)]
pub struct SessionOutcome {
    pub realized_cost: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Set when the session failed in a way worth tagging (transient, auth, rate-limit, ...).
    pub error_kind: Option<String>,
}

/// A session's transport.
///
/// M0: a trivial rewrite-only proxy. M1+: the same impl flowered into a full tee + fail-closed
/// `VettedEndpoint` privacy proxy. The signature is stable across all of them.
pub trait Backend {
    fn run_session(&self, pick: &Pick, alias: &str) -> Result<SessionOutcome>;
}

/// The M0 placeholder. The real trivial model-rewrite proxy is filled in as the `run`
/// subcommand is built out; `simulate` (the M0 go/no-go) needs no transport at all.
pub struct ProxyBackend;

impl Backend for ProxyBackend {
    fn run_session(&self, _pick: &Pick, _alias: &str) -> Result<SessionOutcome> {
        anyhow::bail!(
            "ProxyBackend::run_session is not implemented in M0 — the M0 deliverable is `simulate`"
        )
    }
}
