//! blindcoder **backend** — the load-bearing seam.
//!
//! Everything *above* this trait (selector, store, config, aliasing, the CLI) is real from day
//! one and never touched again. Everything *below* it grows inside this one crate: at M0 the
//! transport is a trivial model-rewrite proxy; M1 adds the raw-capture tee and the fail-closed
//! per-request privacy flags behind the *same* session lifecycle — no rewrite.
//!
//! The seam is a **session lifecycle**, not a single blocking call: the router starts a session,
//! blocks on its events, and can abort it mid-flight. That is what makes `max_session_cost_usd`
//! enforceable — a one-shot call could only report cost after the money was spent. Mechanism
//! (observe/abort) lives here; policy (the cap threshold, the token→cost estimate) lives in the
//! router's driver loop.

use anyhow::Result;
use async_trait::async_trait;

pub mod proxy;
pub mod rewrite;

pub use proxy::ProxyBackend;

/// The chosen candidate for a session. The real slug is present here because the transport needs
/// it to route; it reaches this struct only via the alias reveal gate (reason: routing).
#[derive(Clone, Debug)]
pub struct Pick {
    pub canonical_key: String,
    pub real_slug: String,
    pub base_url: String,
}

/// Cumulative usage so far — cheap to read, updated as the transport streams.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageSnapshot {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Best-effort priced cost so far; `None` when the transport can't price mid-stream.
    /// The router falls back to tokens × the pick's unit price so the cost cap still fires.
    pub cost_so_far: Option<f64>,
}

/// A lifecycle event the driver blocks on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SessionEvent {
    /// A stream tick: cumulative usage advanced. Transports that can't stream never emit this.
    Usage(UsageSnapshot),
    /// The session finished on its own (model done / user quit / transport error).
    Ended,
}

/// Why a session was stopped early. Recorded on the terminal event (`session_end.terminated_by`);
/// distinct from `error_kind` — a deliberate policy stop is not a backend failure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AbortReason {
    CostCap,
    User,
}

impl AbortReason {
    /// Stable string form written to `session_end.terminated_by`.
    pub fn as_str(&self) -> &'static str {
        match self {
            AbortReason::CostCap => "cost_cap",
            AbortReason::User => "user",
        }
    }
}

/// How a session failed, when it did. Derived live from the upstream status / finish-reason (the raw
/// truth is in the WARC archive at `replay`); stored on `session_end.error_kind` so the selector has
/// the failure signal even at the `metadata` floor. A closed set — the only values `error_kind` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// 429 / provider rate or quota limit (e.g. tokens-per-minute).
    RateLimit,
    /// Upstream 5xx.
    Http5xx,
    /// 401 / 403 — bad or missing credentials.
    Auth,
    /// Other 4xx (malformed, too large, unknown model, …) or an HTTP-200 body carrying an `error`.
    BadRequest,
    /// No HTTP response at all (connection failure / timeout).
    Network,
    /// A completion cut off at the output-token limit (`finish_reason = "length"`).
    Truncated,
    /// The model declined (`finish_reason = "content_filter"`).
    Refused,
    /// A response we couldn't classify into the above — an unexpected status (1xx/3xx/≥600) or an
    /// unrecognised failure. The raw status/body is in the WARC capture at `replay`.
    Unknown,
}

impl ErrorKind {
    /// Stable string form written to `session_end.error_kind`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::RateLimit => "rate_limit",
            ErrorKind::Http5xx => "http_5xx",
            ErrorKind::Auth => "auth",
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::Network => "network",
            ErrorKind::Truncated => "truncated",
            ErrorKind::Refused => "refused",
            ErrorKind::Unknown => "unknown",
        }
    }
}

/// What a finished session reports back — the `metadata`-floor signal the selector learns from.
/// No prompt/code: just tokens, realized cost, an optional error tag, and how it ended.
#[derive(Clone, Debug, Default)]
pub struct SessionOutcome {
    pub realized_cost: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    /// Set when the session failed in a way worth tagging; `None` on clean success.
    pub error_kind: Option<ErrorKind>,
    /// The raw upstream HTTP status of an HTTP-level failure (the ground truth `error_kind` is
    /// derived from). `None` for a network failure (no status), a content issue, or clean success.
    pub error_status: Option<u16>,
    /// `Some` = aborted (by whom); `None` = natural completion.
    pub terminated_by: Option<AbortReason>,
}

/// A live session the router observes and can stop.
///
/// Contract: `next_event` yields `Ended` exactly once; calling it again afterwards is an error.
/// After an `abort`, the transport tears down at its next boundary and `next_event` still drains
/// to `Ended`; the driver then calls `finish` for the terminal metadata.
#[async_trait]
pub trait Session: Send {
    /// Wait for the next lifecycle event.
    async fn next_event(&mut self) -> Result<SessionEvent>;

    /// Latest cumulative usage without blocking — out-of-band cap checks, status line, tests.
    /// Kept sync deliberately: implementations back it with atomics, not the event stream.
    fn usage(&self) -> UsageSnapshot;

    /// Cooperatively stop. Idempotent; the transport tears down at its next boundary.
    async fn abort(&mut self, reason: AbortReason);

    /// Wait for teardown to complete and return the terminal metadata.
    async fn finish(self: Box<Self>) -> Result<SessionOutcome>;

    /// The local address the transport is serving on, if it runs a proxy the CLI points at.
    /// `None` for transports with no listener (e.g. test fakes). Default `None`.
    fn endpoint(&self) -> Option<std::net::SocketAddr> {
        None
    }
}

/// A session's transport.
///
/// M0: the streaming forwarding proxy in [`mod@proxy`]. M1+: the same impl flowered into a full tee
/// + fail-closed `VettedEndpoint` privacy proxy. The signature is stable across all.
#[async_trait]
pub trait Backend {
    /// Begin a session; returns with a live handle while the transport runs underneath.
    async fn start(&self, pick: &Pick, alias: &str) -> Result<Box<dyn Session>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A scripted transport: emits a fixed sequence of usage ticks, honors abort by cutting
    /// the remaining script short, and reports terminal metadata from whatever it saw.
    struct FakeSession {
        script: Vec<UsageSnapshot>,
        cursor: usize,
        ended: bool,
        aborted: Option<AbortReason>,
        completion_tokens: Arc<AtomicU64>,
    }

    impl FakeSession {
        fn new(script: Vec<UsageSnapshot>) -> Self {
            Self {
                script,
                cursor: 0,
                ended: false,
                aborted: None,
                completion_tokens: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    #[async_trait]
    impl Session for FakeSession {
        async fn next_event(&mut self) -> Result<SessionEvent> {
            anyhow::ensure!(!self.ended, "next_event called after Ended");
            if self.aborted.is_some() || self.cursor >= self.script.len() {
                self.ended = true;
                return Ok(SessionEvent::Ended);
            }
            let snap = self.script[self.cursor];
            self.cursor += 1;
            self.completion_tokens
                .store(snap.completion_tokens, Ordering::Relaxed);
            Ok(SessionEvent::Usage(snap))
        }

        fn usage(&self) -> UsageSnapshot {
            UsageSnapshot {
                prompt_tokens: 0,
                completion_tokens: self.completion_tokens.load(Ordering::Relaxed),
                cost_so_far: None,
            }
        }

        async fn abort(&mut self, reason: AbortReason) {
            self.aborted.get_or_insert(reason);
        }

        async fn finish(self: Box<Self>) -> Result<SessionOutcome> {
            Ok(SessionOutcome {
                realized_cost: None,
                prompt_tokens: None,
                completion_tokens: Some(self.completion_tokens.load(Ordering::Relaxed)),
                error_kind: None,
                error_status: None,
                terminated_by: self.aborted,
            })
        }
    }

    fn tick(completion_tokens: u64, cost: Option<f64>) -> UsageSnapshot {
        UsageSnapshot {
            prompt_tokens: 100,
            completion_tokens,
            cost_so_far: cost,
        }
    }

    /// The router's driver loop in miniature: the cap is POLICY and lives with the driver,
    /// falling back to tokens × unit price when the transport can't price mid-stream.
    async fn drive_with_cap(
        mut sess: Box<dyn Session>,
        cap: f64,
        unit_price_per_token: f64,
    ) -> Result<SessionOutcome> {
        loop {
            match sess.next_event().await? {
                SessionEvent::Usage(u) => {
                    let spent = u.cost_so_far.unwrap_or_else(|| {
                        (u.prompt_tokens + u.completion_tokens) as f64 * unit_price_per_token
                    });
                    if cap > 0.0 && spent >= cap {
                        sess.abort(AbortReason::CostCap).await;
                    }
                }
                SessionEvent::Ended => break,
            }
        }
        sess.finish().await
    }

    /// Usage → over-cap → abort → Ended → finish: the kill-switch path, priced transport.
    #[tokio::test]
    async fn cost_cap_aborts_mid_session() {
        let sess = FakeSession::new(vec![
            tick(10, Some(1.0)),
            tick(20, Some(4.0)),
            tick(30, Some(9.0)), // over the 5.0 cap — must abort here
            tick(40, Some(16.0)),
        ]);
        let outcome = drive_with_cap(Box::new(sess), 5.0, 0.0).await.unwrap();
        assert_eq!(outcome.terminated_by, Some(AbortReason::CostCap));
        // aborted on the third tick; the fourth was never consumed
        assert_eq!(outcome.completion_tokens, Some(30));
    }

    /// Decision 3: the cap still fires when `cost_so_far` is None, via the token estimate.
    #[tokio::test]
    async fn cost_cap_fires_on_unpriced_transport() {
        let sess = FakeSession::new(vec![
            tick(1_000, None),  // (100 + 1000) * 0.001 = 1.1
            tick(10_000, None), // (100 + 10000) * 0.001 = 10.1 > 5.0 — abort
            tick(100_000, None),
        ]);
        let outcome = drive_with_cap(Box::new(sess), 5.0, 0.001).await.unwrap();
        assert_eq!(outcome.terminated_by, Some(AbortReason::CostCap));
        assert_eq!(outcome.completion_tokens, Some(10_000));
    }

    /// cap = 0.0 disables enforcement; natural completion has terminated_by = None.
    #[tokio::test]
    async fn cap_zero_disables_and_natural_end_is_unterminated() {
        let sess = FakeSession::new(vec![tick(10, Some(100.0)), tick(20, Some(200.0))]);
        let outcome = drive_with_cap(Box::new(sess), 0.0, 0.0).await.unwrap();
        assert_eq!(outcome.terminated_by, None);
        assert_eq!(outcome.completion_tokens, Some(20));
    }

    /// abort is idempotent and out-of-band `usage()` reflects progress without blocking.
    #[tokio::test]
    async fn abort_idempotent_and_usage_readable() {
        let mut sess = FakeSession::new(vec![tick(10, None), tick(20, None)]);
        assert_eq!(sess.usage().completion_tokens, 0);
        sess.next_event().await.unwrap();
        assert_eq!(sess.usage().completion_tokens, 10);
        sess.abort(AbortReason::User).await;
        sess.abort(AbortReason::CostCap).await; // first reason wins
        assert_eq!(sess.next_event().await.unwrap(), SessionEvent::Ended);
        let outcome = Box::new(sess).finish().await.unwrap();
        assert_eq!(outcome.terminated_by, Some(AbortReason::User));
    }
}
