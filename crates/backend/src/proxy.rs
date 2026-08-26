//! The M0 forwarding transport: a small streaming reverse proxy behind the `Backend`/`Session`
//! seam. It binds a localhost listener, and for each request rewrites the blind `model` to the
//! resolved real slug and applies the per-request privacy injection — both via the privacy gate
//! `VettedEndpoint::prepare`, the sole way to obtain the `VettedRequest` this transport forwards.
//! It sends to the provider endpoint with the API key and any `extra_headers`, streams the response
//! straight back to the caller, and — after each response completes — parses the `usage` block to
//! accumulate token counts. Those counts surface as `Usage` events so the router's cost cap can act.
//!
//! It is provider-blind: nothing here branches on which backend it talks to. The M1 tee grows from
//! this same shape (raw capture + type-enforced privacy) behind the unchanged trait.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    body::Body,
    extract::{OriginalUri, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use warc::{RecordBuilder, RecordType, WarcHeader, WarcWriter};

use config::Privacy;

use crate::rewrite::{mask_json_body, mask_sse_line, parse_usage};
use crate::{
    AbortReason, Backend, ErrorKind, Pick, Session, SessionEvent, SessionOutcome, UsageSnapshot,
    VettedRequest,
};

/// Map an upstream HTTP status to an [`ErrorKind`] (for the "no clean completion" case).
fn classify_http(status: StatusCode) -> ErrorKind {
    match status {
        StatusCode::TOO_MANY_REQUESTS => ErrorKind::RateLimit,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ErrorKind::Auth,
        // request too large: context window or a per-minute token cap
        StatusCode::PAYLOAD_TOO_LARGE => ErrorKind::TooLarge,
        // model/route not available to you (delisted, free-retired, ZDR-filtered)
        StatusCode::NOT_FOUND => ErrorKind::Unavailable,
        s if s.is_server_error() => ErrorKind::Http5xx,
        s if s.is_client_error() => ErrorKind::BadRequest, // other 4xx: 400/422/…
        // 1xx/2xx/3xx/≥600 shouldn't reach here (we only classify non-2xx final statuses; reqwest
        // follows redirects) — distinct Unknown bucket rather than mislabelling as bad_request.
        _ => ErrorKind::Unknown,
    }
}

/// The fail-closed accountability trail of the opt-in non-ZDR routing path: every request
/// forwarded to a `no-zdr` model appends one `<UTC hour> · <real_slug>` line to a dedicated
/// append-only log (file created `0600`, inside a `0700` directory — even the log's existence
/// admits a pay-with-data endpoint is configured, so nothing around it may widen access).
///
/// What this file guarantees is **aggregate accountability only**: which real models have been
/// sent prompts, and in which clock hours. It deliberately carries **no session identifier** —
/// the store keys ratings on `session_id`, so an id-bearing audit file would join straight onto
/// the ratings table and deblind every non-ZDR session and its rating, and could be read
/// mid-session to unmask a session before it is rated. A per-session random token fails the same
/// way: tail the file once and the freshly-appearing token's slug names the live session. For
/// the same reason the timestamp is bucketed to whole hours — minute-level times would let an
/// operator pin a just-run session's model by recalling roughly when it ran, while requests from
/// concurrent sessions within one hour are indistinguishable by construction. Unmasking a
/// specific session happens only through the reveal gate; this log is not a second path there.
///
/// Each record is formatted into one small buffer and emitted with `write_all`; what keeps
/// concurrent lines intact is **per-syscall `O_APPEND` atomicity**: every `write(2)` to a regular
/// file atomically positions at end-of-file and writes, so a record small enough to complete in
/// one syscall lands as one indivisible line even when several processes append concurrently to
/// the shared log — exactly what the concurrency test demonstrates. (`write_all` may loop over
/// partial writes; a record this size does not.)
///
/// Appends are fail-closed in both directions: any filesystem error returns and the caller must
/// refuse the request — no routing without a durable record — and so does an expired attestation.
/// The expiry is checked **per request** here, not only once at startup, because a standing proxy
/// (no launched command, ended by Ctrl-C) would otherwise outlive its window and route non-ZDR
/// traffic indefinitely past `expires`. The blocking open/write/fsync runs on tokio's blocking
/// pool via [`NonZdrAudit::append`], never on an async worker.
#[cfg(feature = "allow-non-zdr")]
#[derive(Clone, Debug)]
pub struct NonZdrAudit {
    path: PathBuf,
    /// The armed attestation's bounded lifetime in epoch days (`config::date_to_epoch_days` of
    /// the provider's `expires`). `None` records no bounded lifetime → every append refuses
    /// (fail-closed): an unbounded non-ZDR capability must never arm.
    expires_epoch_days: Option<i64>,
}

/// Why [`NonZdrAudit`] refused to witness a request. Every variant is fail-closed: the caller
/// must refuse the request — never forward unwitnessed.
#[cfg(feature = "allow-non-zdr")]
#[derive(Debug)]
enum AuditRefusal {
    /// The attestation carries no bounded lifetime — such a capability can never arm.
    Unbounded,
    /// The attestation's window ended: a standing proxy has run past `expires`.
    Expired(String),
    /// Any filesystem failure opening, tightening, writing, or syncing the record.
    Io(std::io::Error),
    /// The blocking-pool task died before proving a durable record either way.
    Join,
}

#[cfg(feature = "allow-non-zdr")]
impl std::fmt::Display for AuditRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unbounded => write!(f, "attestation carries no bounded lifetime"),
            Self::Expired(date) => write!(f, "attestation expired on {date}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Join => write!(f, "audit worker terminated"),
        }
    }
}

#[cfg(feature = "allow-non-zdr")]
impl From<std::io::Error> for AuditRefusal {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Render an epoch-day count back to `YYYY-MM-DD` for a refusal message.
#[cfg(feature = "allow-non-zdr")]
fn format_expiry(days: i64) -> String {
    chrono::DateTime::from_timestamp(days.saturating_mul(86_400), 0)
        .map_or_else(|| days.to_string(), |dt| dt.format("%Y-%m-%d").to_string())
}

#[cfg(feature = "allow-non-zdr")]
impl NonZdrAudit {
    /// Async front-end for [`Self::append_sync`]: runs the durable append on tokio's blocking
    /// pool so the open/fsync never stalls an async worker. The caller still awaits completion
    /// BEFORE forwarding — durability-before-forward and fail-closed refusal are unchanged.
    async fn append(&self, real_slug: &str) -> Result<(), AuditRefusal> {
        let audit = self.clone();
        let slug = real_slug.to_owned();
        tokio::task::spawn_blocking(move || audit.append_sync(&slug))
            .await
            .map_err(|_| AuditRefusal::Join)?
    }

    /// The blocking core: refuse if expired/unbounded, then open (creating `0600`), tighten an
    /// existing looser file, durably write one record. Sync by design — it belongs on a blocking
    /// thread.
    fn append_sync(&self, real_slug: &str) -> Result<(), AuditRefusal> {
        // Per-request expiry: startup validated the window once; this bounds a standing proxy to
        // the same window. Same predicate as startup (`expires < today` ⇒ refused), so the day
        // `expires` names stays routable until UTC midnight.
        let Some(expires_days) = self.expires_epoch_days else {
            return Err(AuditRefusal::Unbounded);
        };
        if config::today_epoch_days() > expires_days {
            return Err(AuditRefusal::Expired(format_expiry(expires_days)));
        }
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        // `.mode()` applies only at creation, so a pre-existing looser file (older build, manual
        // touch, umask) would stay loose — tighten it idempotently before anything is written
        // into it: the file-level mirror of `run::ensure_private_dir`.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        // One small record, one buffer: under `O_APPEND` each `write(2)` on a regular file
        // atomically positions at EOF and writes, so a record completing in one syscall cannot be
        // interleaved by a concurrent appender. (`writeln!` would issue several `write(2)`s whose
        // fragments CAN interleave between calls despite `O_APPEND`.)
        let line = format!(
            "{}\t{}\n",
            chrono::Utc::now().format("%Y-%m-%dT%H"),
            real_slug
        );
        f.write_all(line.as_bytes())?;
        f.sync_all()?;
        Ok(())
    }
}

/// A ready-to-run forwarding proxy. Constructed per session by the router with the picked
/// provider's credentials and passthrough hooks; the per-model target (`base_url`, `real_slug`)
/// arrives in [`Backend::start`]'s [`Pick`].
pub struct ProxyBackend {
    bind_addr: SocketAddr,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
    extra_body: serde_json::Map<String, Value>,
    /// The provider's privacy protocol, applied to every forwarded request (the pool build
    /// guarantees only an eligible, host-matched provider reaches here).
    privacy: Privacy,
    capture_path: Option<PathBuf>,
    /// `Some` when this session routes to a `no-zdr` model: the per-request audit sink.
    #[cfg(feature = "allow-non-zdr")]
    non_zdr_audit: Option<NonZdrAudit>,
    client: reqwest::Client,
}

impl ProxyBackend {
    /// Build a proxy that will listen on `bind_addr` (use port 0 for an ephemeral port; read the
    /// bound address back via [`Session::endpoint`]). `capture_path`, when set, turns on the raw
    /// four-leg WARC archive for the session (the `replay` capture level) at that file.
    pub fn new(
        bind_addr: SocketAddr,
        api_key: Option<String>,
        extra_headers: Vec<(String, String)>,
        extra_body: serde_json::Map<String, Value>,
        privacy: Privacy,
        capture_path: Option<PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            bind_addr,
            api_key,
            extra_headers,
            extra_body,
            privacy,
            capture_path,
            #[cfg(feature = "allow-non-zdr")]
            non_zdr_audit: None,
            client: reqwest::Client::builder()
                .build()
                .context("building HTTP client")?,
        })
    }

    /// Arm the fail-closed non-ZDR accountability trail: every forwarded request appends one
    /// `<UTC hour> · <real_slug>` line to `path` — or is refused, whether the append fails or
    /// the attestation's bounded lifetime has ended. `expires_epoch_days` is
    /// `config::date_to_epoch_days` of the provider's `expires`; `None` refuses every request,
    /// so an unbounded capability can never arm. The path is the whole configuration beyond
    /// that — no session identity crosses this boundary, so the log can never join onto the
    /// store's ratings; see [`NonZdrAudit`]. Only meaningful — and only compiled — on the opt-in
    /// routing path; the router calls this solely when the pick landed on a `no-zdr` model.
    #[cfg(feature = "allow-non-zdr")]
    pub fn with_non_zdr_audit(mut self, path: PathBuf, expires_epoch_days: Option<i64>) -> Self {
        self.non_zdr_audit = Some(NonZdrAudit {
            path,
            expires_epoch_days,
        });
        self
    }
}

/// The one and only outbound send. It takes a [`VettedRequest`], so no code path can forward a body
/// that skipped [`crate::VettedEndpoint::prepare`]'s privacy injection — the type is the guarantee.
async fn forward(
    req: reqwest::RequestBuilder,
    vetted: VettedRequest,
) -> reqwest::Result<reqwest::Response> {
    req.body(vetted.into_body()).send().await
}

/// One captured leg of an exchange, sent to the WARC writer task. The four legs of the spec's raw
/// archive: `cli_request` (as received), `provider_request` (as sent), `provider_response` (raw
/// upstream), `cli_response` (masked back to the client).
struct CaptureLeg {
    exchange: u64,
    leg: &'static str,
    warc_type: RecordType,
    target_uri: String,
    body: Vec<u8>,
}

/// Spawn the blocking WARC writer task: opens `path` (0600), writes each leg as a WARC record, and
/// flushes on channel close. Sync I/O runs on a blocking thread so it never stalls the async
/// runtime. Returns the sender legs are pushed to, and the task handle to await at session end.
fn spawn_warc_writer(
    path: PathBuf,
) -> (
    mpsc::UnboundedSender<CaptureLeg>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<CaptureLeg>();
    let handle = tokio::task::spawn_blocking(move || {
        let Ok(mut writer) = WarcWriter::from_path(&path) else {
            return;
        }; // capture is best-effort
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        while let Some(leg) = rx.blocking_recv() {
            let record = RecordBuilder::default()
                .warc_type(leg.warc_type)
                .header(WarcHeader::TargetURI, leg.target_uri.into_bytes())
                .header(
                    WarcHeader::Unknown("x-blindcoder-leg".into()),
                    leg.leg.as_bytes().to_vec(),
                )
                .header(
                    WarcHeader::Unknown("x-blindcoder-exchange".into()),
                    leg.exchange.to_string().into_bytes(),
                )
                .date(chrono::Utc::now())
                .body(leg.body)
                .build();
            if let Ok(record) = record {
                let _ = writer.write(&record);
            }
        }
        let _ = writer.into_inner(); // flush the buffer
    });
    (tx, handle)
}

/// Cumulative per-session usage + failure signals, shared between the request handlers and the
/// session handle. Cost is accumulated in integer nano-dollars (float-atomic-free) and surfaced
/// only when a response reported one. Failure state feeds [`error_kind`](Cumulative::error_kind).
/// The most recent transport/body failure of a session. Encoded into an [`AtomicU64`] for lock-free
/// storage inside [`Cumulative`] (`encode`/`decode` are the only place the numeric form appears): the
/// `< 100` sentinels never collide with a real HTTP status, and `0` decodes to "no failure".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Failure {
    Network,          // no response from upstream
    BodyError,        // a 2xx whose body carried an `error` payload
    Http(StatusCode), // an upstream HTTP error status
}

impl Failure {
    fn encode(self) -> u64 {
        match self {
            Failure::Network => 1,
            Failure::BodyError => 2,
            Failure::Http(status) => status.as_u16() as u64, // status >= 100 never collides with 1/2
        }
    }

    fn decode(v: u64) -> Option<Self> {
        match v {
            0 => None,
            1 => Some(Failure::Network),
            2 => Some(Failure::BodyError),
            // Only ever encoded from a real `StatusCode`, so `from_u16` round-trips; a value that
            // somehow does not is dropped to `None` (treated as "no failure") rather than panicking.
            v => StatusCode::from_u16(v as u16).ok().map(Failure::Http),
        }
    }

    /// The learned failure tag this maps to.
    fn kind(self) -> ErrorKind {
        match self {
            Failure::Network => ErrorKind::Network,
            Failure::BodyError => ErrorKind::BadRequest,
            Failure::Http(status) => classify_http(status),
        }
    }

    /// The raw upstream HTTP status, if this failure had one (network / body errors do not).
    fn status(self) -> Option<u16> {
        match self {
            Failure::Http(status) => Some(status.as_u16()),
            Failure::Network | Failure::BodyError => None,
        }
    }
}

#[derive(Default)]
struct Cumulative {
    prompt: AtomicU64,
    completion: AtomicU64,
    cached_prompt: AtomicU64,
    cost_nano: AtomicU64,
    has_cost: AtomicBool,
    // Recency of the session's outcomes. Every clean completion and every transport/body failure is
    // stamped with a monotonic `seq`; the failure tag is derived from whichever stamp is newer, so a
    // run that dies on a terminal 413/429/5xx is learned as such even after earlier requests
    // succeeded (a single earlier success no longer masks a terminal failure). Recording order
    // approximates logical order — agentic CLIs issue requests sequentially, so the newest stamp is
    // the session's last outcome.
    seq: AtomicU64,              // event counter; each stamp is a distinct value >= 1
    last_success_seq: AtomicU64, // stamp of the most recent clean completion (0 = none)
    last_failure_seq: AtomicU64, // stamp of the most recent transport/body failure (0 = none)
    last_failure: AtomicU64,     // the most recent failure, encoded via `Failure::encode` (0 = none)
    content_issue: AtomicU64,    // 0 = none, 1 = truncated (length), 2 = refused (content_filter)
}

impl Cumulative {
    /// Add one response's usage and return the new cumulative snapshot.
    fn add(&self, u: &UsageSnapshot) -> UsageSnapshot {
        let prompt = self.prompt.fetch_add(u.prompt_tokens, Ordering::Relaxed) + u.prompt_tokens;
        let completion = self
            .completion
            .fetch_add(u.completion_tokens, Ordering::Relaxed)
            + u.completion_tokens;
        let cached_prompt = self
            .cached_prompt
            .fetch_add(u.cached_prompt_tokens, Ordering::Relaxed)
            + u.cached_prompt_tokens;
        if let Some(c) = u.cost_so_far {
            self.cost_nano
                .fetch_add((c * 1e9).round() as u64, Ordering::Relaxed);
            self.has_cost.store(true, Ordering::Relaxed);
        }
        UsageSnapshot {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_prompt_tokens: cached_prompt,
            cost_so_far: self.cost(),
        }
    }

    /// Cumulative provider-reported cost in dollars, or `None` if no response reported one.
    fn cost(&self) -> Option<f64> {
        if self.has_cost.load(Ordering::Relaxed) {
            Some(self.cost_nano.load(Ordering::Relaxed) as f64 / 1e9)
        } else {
            None
        }
    }

    /// Cumulative `(prompt, completion, cached_prompt)` tokens. `cached_prompt` is the
    /// observability-only prompt-cache-hit total (a subset of `prompt`).
    fn totals(&self) -> (u64, u64, u64) {
        (
            self.prompt.load(Ordering::Relaxed),
            self.completion.load(Ordering::Relaxed),
            self.cached_prompt.load(Ordering::Relaxed),
        )
    }

    /// Allocate the next monotonic event stamp (>= 1; 0 stays reserved for "no event yet").
    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }
    /// Record the most recent failure: store its encoding *before* its stamp so a reader that
    /// observes the new stamp also observes the matching failure.
    fn note_failure(&self, failure: Failure) {
        self.last_failure.store(failure.encode(), Ordering::Relaxed);
        self.last_failure_seq.store(self.next_seq(), Ordering::Relaxed);
    }
    fn note_network(&self) {
        self.note_failure(Failure::Network);
    }
    fn note_http_error(&self, status: StatusCode) {
        self.note_failure(Failure::Http(status));
    }
    fn note_success(&self) {
        self.last_success_seq.store(self.next_seq(), Ordering::Relaxed);
    }
    fn note_body_error(&self) {
        self.note_failure(Failure::BodyError);
    }
    fn note_finish_reason(&self, reason: &str) {
        let v = match reason {
            "length" => 1,
            "content_filter" => 2,
            _ => 0,
        };
        if v != 0 {
            self.content_issue.store(v, Ordering::Relaxed);
        }
    }

    /// Derive the session's failure tag. When the session's most recent outcome was a transport or
    /// body failure, that failure tags the session — even if earlier requests completed cleanly, so
    /// a run that ends on a 413/429/5xx is learned as such. When the most recent outcome was instead
    /// a clean completion (or the failure was an earlier one the CLI recovered from), only a
    /// content-level degradation (truncated / refused) of a completion carries through.
    fn error_kind(&self) -> Option<ErrorKind> {
        if let Some(failure) = self.terminal_failure() {
            return Some(failure.kind());
        }
        match self.content_issue.load(Ordering::Relaxed) {
            1 => Some(ErrorKind::Truncated),
            2 => Some(ErrorKind::Refused),
            _ => None,
        }
    }

    /// The raw upstream HTTP status of the *terminal* failure, if it was an HTTP one. `None` when the
    /// session recovered, when the terminal failure was a network drop or a 2xx body error (neither
    /// has a failing status), or when there was no failure.
    fn error_status(&self) -> Option<u16> {
        self.terminal_failure().and_then(Failure::status)
    }

    /// The most recent transport/body failure, but only if it was the session's last outcome (no
    /// clean completion came after it). `None` when the session recovered or never failed.
    fn terminal_failure(&self) -> Option<Failure> {
        let last_failure = self.last_failure_seq.load(Ordering::Relaxed);
        if last_failure != 0 && last_failure > self.last_success_seq.load(Ordering::Relaxed) {
            Failure::decode(self.last_failure.load(Ordering::Relaxed))
        } else {
            None
        }
    }
}

/// Everything a request handler needs to forward and account one call.
struct ProxyState {
    /// The forwarding target and the sole factory for the [`VettedRequest`] the handler sends.
    endpoint: crate::VettedEndpoint,
    /// The provider's privacy basis, applied by `endpoint.prepare` to every request.
    privacy: Privacy,
    real_slug: String,
    alias: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
    extra_body: serde_json::Map<String, Value>,
    client: reqwest::Client,
    usage_tx: mpsc::UnboundedSender<UsageSnapshot>,
    cumulative: Arc<Cumulative>,
    /// `Some` at the `replay` capture level: legs are pushed here for the WARC writer task.
    capture_tx: Option<mpsc::UnboundedSender<CaptureLeg>>,
    /// Monotonic id grouping the four legs of one exchange in the archive.
    exchange_seq: AtomicU64,
    /// `Some` when this session routes to a `no-zdr` model: append before every forward, refuse on
    /// failure (fail-closed).
    #[cfg(feature = "allow-non-zdr")]
    non_zdr_audit: Option<NonZdrAudit>,
}

/// Signals read from a completed response body in one pass: token usage, the last `finish_reason`,
/// and whether an `error` object was present.
#[derive(Default)]
struct Signals {
    usage: Option<UsageSnapshot>,
    finish_reason: Option<String>,
    has_error: bool,
}

/// Extract [`Signals`] from one OpenAI-wire JSON object.
fn signals_from_value(v: &Value) -> Signals {
    Signals {
        usage: parse_usage(v),
        finish_reason: v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        has_error: v.get("error").is_some(),
    }
}

/// Read failure/usage signals from a completed response body — a plain JSON object, or an SSE stream
/// (usage + the final `finish_reason` arrive in the last `data:` frames).
fn response_signals(body: &[u8]) -> Signals {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        return signals_from_value(&v);
    }
    let mut out = Signals::default();
    if let Ok(text) = std::str::from_utf8(body) {
        for line in text.lines() {
            let Some(rest) = line.trim_start().strip_prefix("data:") else {
                continue;
            };
            let rest = rest.trim();
            if rest == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(rest) {
                let s = signals_from_value(&v);
                if s.usage.is_some() {
                    out.usage = s.usage; // keep the last — streaming usage is in the final frame
                }
                if s.finish_reason.is_some() {
                    out.finish_reason = s.finish_reason;
                }
                out.has_error |= s.has_error;
            }
        }
    }
    out
}

/// Forward one request to the upstream provider and stream the response back, rewriting the model
/// and accounting usage on the way through.
async fn proxy_handler(
    State(st): State<Arc<ProxyState>>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Blind the model catalog: this session is one picked model, so `GET …/models` returns just the
    // alias — never the provider's real model list. (Without this the CLI's model picker would show
    // real names — a deblind vector the request/response masking wouldn't otherwise cover.)
    if method == Method::GET && uri.path().ends_with("/models") {
        let list = serde_json::json!({
            "object": "list",
            "data": [{ "id": st.alias, "object": "model", "owned_by": "blindcoder" }]
        });
        return Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(list.to_string()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    // Map the caller's path onto the provider's endpoint: the provider's base_url already carries
    // its version prefix (e.g. .../v1 or .../openai/v1), so append everything after the caller's
    // own "/v1" (or the whole path if it has none).
    let path = uri.path();
    let suffix = match path.rfind("/v1") {
        Some(i) => &path[i + 3..],
        None => path,
    };
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("{}{}{}", st.endpoint.url(), suffix, query);

    // The privacy gate: rewrite the model, merge `extra_body`, and apply the per-request privacy
    // injection. This is the *only* way to obtain a `VettedRequest`, and the send below accepts only
    // one — so a body cannot reach the wire without the injection. (Non-model bodies pass through.)
    let vetted = st
        .endpoint
        .prepare(&body, &st.real_slug, &st.extra_body, st.privacy);

    // Fail-closed non-ZDR audit: the durable record is appended BEFORE anything is forwarded;
    // any refusal — a filesystem failure or an expired/unbounded attestation — refuses the
    // request. The blocking open/write/fsync runs on tokio's blocking pool, off the async
    // workers; the await still completes before anything is sent upstream.
    #[cfg(feature = "allow-non-zdr")]
    if let Some(audit) = &st.non_zdr_audit {
        if let Err(e) = audit.append(&st.real_slug).await {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "non-ZDR audit refused ({e}); refusing to forward without a durable record"
                ),
            )
                .into_response();
        }
    }

    // Capture legs 1–2 (request side) at the `replay` level, before sending.
    let exchange = st.exchange_seq.fetch_add(1, Ordering::Relaxed);
    if let Some(tx) = &st.capture_tx {
        let _ = tx.send(CaptureLeg {
            exchange,
            leg: "cli_request",
            warc_type: RecordType::Request,
            target_uri: uri.to_string(),
            body: body.to_vec(),
        });
        let _ = tx.send(CaptureLeg {
            exchange,
            leg: "provider_request",
            warc_type: RecordType::Request,
            target_uri: url.clone(),
            body: vetted.body().to_vec(),
        });
    }

    let mut req = st.client.request(method, &url);
    req = match headers.get(header::CONTENT_TYPE) {
        Some(ct) => req.header(header::CONTENT_TYPE, ct),
        None => req.header(header::CONTENT_TYPE, "application/json"),
    };
    if let Some(acc) = headers.get(header::ACCEPT) {
        req = req.header(header::ACCEPT, acc);
    }
    if let Some(key) = &st.api_key {
        req = req.bearer_auth(key);
    }
    for (k, v) in &st.extra_headers {
        req = req.header(k, v);
    }

    let upstream = match forward(req, vetted).await {
        Ok(r) => r,
        Err(e) => {
            st.cumulative.note_network();
            return (
                StatusCode::BAD_GATEWAY,
                format!("upstream request failed: {e}"),
            )
                .into_response();
        }
    };

    let status = upstream.status();
    let succeeded = status.is_success();
    if !succeeded {
        st.cumulative.note_http_error(status);
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .is_some_and(|c| c.to_str().ok().is_some_and(|s| s.contains("event-stream")));

    // Stream the response back with the model masked to the alias (SSE: per-frame, preserving
    // streaming; JSON: buffered once). We account usage from the *masked* bytes — masking only
    // touches `model`/fingerprints, never `usage`/`cost`.
    let st2 = st.clone();
    let alias = st.alias.clone();
    let real_slug = st.real_slug.clone(); // scrubbed from free text (error messages) too, not just `model`
    let target = url.clone();
    let stream = async_stream::stream! {
        let mut raw_acc: Vec<u8> = Vec::new(); // unmasked upstream bytes (provider_response leg + signals)
        let mut masked_acc: Vec<u8> = Vec::new(); // what the client received (cli_response leg)
        let mut bytes_stream = upstream.bytes_stream();
        if is_sse {
            let mut linebuf: Vec<u8> = Vec::new();
            while let Some(item) = bytes_stream.next().await {
                match item {
                    Ok(chunk) => {
                        raw_acc.extend_from_slice(&chunk);
                        linebuf.extend_from_slice(&chunk);
                        while let Some(pos) = linebuf.iter().position(|&b| b == b'\n') {
                            let raw: Vec<u8> = linebuf.drain(..=pos).collect();
                            let text = String::from_utf8_lossy(&raw);
                            let core = text.trim_end_matches('\n').trim_end_matches('\r');
                            let out = format!("{}\n", mask_sse_line(core, &real_slug, &alias));
                            masked_acc.extend_from_slice(out.as_bytes());
                            yield Ok::<Bytes, reqwest::Error>(Bytes::from(out.into_bytes()));
                        }
                    }
                    Err(e) => { yield Err(e); break; }
                }
            }
            if !linebuf.is_empty() {
                let masked = mask_sse_line(&String::from_utf8_lossy(&linebuf), &real_slug, &alias);
                masked_acc.extend_from_slice(masked.as_bytes());
                yield Ok(Bytes::from(masked.into_bytes()));
            }
        } else {
            while let Some(item) = bytes_stream.next().await {
                match item {
                    Ok(chunk) => raw_acc.extend_from_slice(&chunk),
                    Err(e) => { yield Err(e); break; }
                }
            }
            masked_acc = mask_json_body(&raw_acc, &real_slug, &alias);
            yield Ok(Bytes::from(masked_acc.clone()));
        }
        // Failure signals + usage from the raw body (masking never touches usage/error/finish).
        let sig = response_signals(&raw_acc);
        if succeeded {
            if sig.has_error {
                st2.cumulative.note_body_error(); // HTTP 200 with an `error` payload
            } else {
                st2.cumulative.note_success();
                if let Some(fr) = &sig.finish_reason {
                    st2.cumulative.note_finish_reason(fr);
                }
            }
        }
        if let Some(u) = sig.usage {
            let snapshot = st2.cumulative.add(&u);
            let _ = st2.usage_tx.send(snapshot);
        }
        // Capture legs 3–4 (response side): raw upstream + what the client received.
        if let Some(tx) = &st2.capture_tx {
            let _ = tx.send(CaptureLeg {
                exchange, leg: "provider_response", warc_type: RecordType::Response, target_uri: target.clone(), body: raw_acc,
            });
            let _ = tx.send(CaptureLeg {
                exchange, leg: "cli_response", warc_type: RecordType::Response, target_uri: target, body: masked_acc,
            });
        }
    };

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// A live forwarding session: the running server plus the usage feed the router observes.
struct ProxySession {
    usage_rx: mpsc::UnboundedReceiver<UsageSnapshot>,
    cumulative: Arc<Cumulative>,
    shutdown: Option<oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    warc_writer: Option<tokio::task::JoinHandle<()>>,
    aborted: Option<AbortReason>,
    local_addr: SocketAddr,
    ended: bool,
}

#[async_trait]
impl Backend for ProxyBackend {
    async fn start(&self, pick: &Pick, alias: &str) -> Result<Box<dyn Session>> {
        let listener = TcpListener::bind(self.bind_addr)
            .await
            .with_context(|| format!("binding proxy listener on {}", self.bind_addr))?;
        let local_addr = listener.local_addr()?;

        let (usage_tx, usage_rx) = mpsc::unbounded_channel();
        let cumulative = Arc::new(Cumulative::default());

        // At the `replay` capture level, spawn the WARC writer and hand the handlers its sender.
        let (capture_tx, warc_writer) = match &self.capture_path {
            Some(path) => {
                let (tx, handle) = spawn_warc_writer(path.clone());
                (Some(tx), Some(handle))
            }
            None => (None, None),
        };

        let state = Arc::new(ProxyState {
            endpoint: crate::VettedEndpoint::new(pick.endpoint.url().trim_end_matches('/')),
            privacy: self.privacy,
            real_slug: pick.real_slug.clone(),
            alias: alias.to_string(),
            api_key: self.api_key.clone(),
            extra_headers: self.extra_headers.clone(),
            extra_body: self.extra_body.clone(),
            client: self.client.clone(),
            usage_tx,
            cumulative: cumulative.clone(),
            capture_tx,
            exchange_seq: AtomicU64::new(0),
            #[cfg(feature = "allow-non-zdr")]
            non_zdr_audit: self.non_zdr_audit.clone(),
        });

        let app = Router::new().fallback(any(proxy_handler)).with_state(state);
        let (sd_tx, sd_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = sd_rx.await;
                })
                .await;
        });

        Ok(Box::new(ProxySession {
            usage_rx,
            cumulative,
            shutdown: Some(sd_tx),
            server: Some(server),
            warc_writer,
            aborted: None,
            local_addr,
            ended: false,
        }))
    }
}

#[async_trait]
impl Session for ProxySession {
    async fn next_event(&mut self) -> Result<SessionEvent> {
        anyhow::ensure!(!self.ended, "next_event called after Ended");
        match self.usage_rx.recv().await {
            Some(u) => Ok(SessionEvent::Usage(u)),
            None => {
                self.ended = true;
                Ok(SessionEvent::Ended)
            }
        }
    }

    fn usage(&self) -> UsageSnapshot {
        let (prompt_tokens, completion_tokens, cached_prompt_tokens) = self.cumulative.totals();
        UsageSnapshot {
            prompt_tokens,
            completion_tokens,
            cached_prompt_tokens,
            cost_so_far: self.cumulative.cost(),
        }
    }

    async fn abort(&mut self, reason: AbortReason) {
        self.aborted.get_or_insert(reason);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<SessionOutcome> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.await; // app (holding the last capture_tx) drops → writer's channel closes
        }
        if let Some(writer) = self.warc_writer.take() {
            let _ = writer.await; // wait for the WARC file to be written + flushed
        }
        let (prompt_tokens, completion_tokens, cached_prompt_tokens) = self.cumulative.totals();
        Ok(SessionOutcome {
            realized_cost: self.cumulative.cost(), // provider-reported when available, else None
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            cached_prompt_tokens: Some(cached_prompt_tokens),
            error_kind: self.cumulative.error_kind(),
            error_status: self.cumulative.error_status(),
            terminated_by: self.aborted,
        })
    }

    fn endpoint(&self) -> Option<SocketAddr> {
        Some(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use serde_json::{json, Value};
    use std::sync::Mutex;

    /// End-to-end against a mock upstream: the proxy must rewrite the blind model to the real slug,
    /// stream the response back, and surface the response's usage as a cumulative event.
    #[tokio::test]
    async fn proxies_rewrites_model_and_reports_usage() {
        // Mock upstream that records the model it received and returns a usage block.
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let up_state = captured.clone();
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(move |body: Bytes| {
                let up_state = up_state.clone();
                async move {
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    *up_state.lock().unwrap() =
                        v.get("model").and_then(Value::as_str).map(String::from);
                    axum::Json(json!({
                        "model": "prov/model-x", "provider": "AcmeProv",
                        "choices": [{"message": {"content": "ok"}}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.0012}
                    }))
                }
            }),
        );
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        // The proxy, pointed at the mock upstream's base.
        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("test-key".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::OpenRouter,
            None,
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "model-x".into(),
            real_slug: "prov/model-x".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let mut sess = backend.start(&pick, "al:al").await.unwrap();
        let proxy_addr = sess.endpoint().unwrap();

        // A CLI-style request carrying the BLIND alias as the model.
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{proxy_addr}/v1/chat/completions"))
            .json(&json!({"model": "al:al", "messages": []}))
            .send()
            .await
            .unwrap();
        let text = resp.text().await.unwrap();
        assert!(text.contains("ok"), "response body streamed back: {text}");
        // Response is masked: the real slug/provider must NOT leak; the model reads as the alias.
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["model"], "al:al", "response model masked to the alias");
        assert!(
            body.get("provider").is_none(),
            "provider fingerprint stripped"
        );
        assert!(
            !text.contains("prov/model-x"),
            "real slug must not appear in the response"
        );

        // Upstream saw the real slug in the *request*, never the alias — the request rewrite happened.
        assert_eq!(captured.lock().unwrap().as_deref(), Some("prov/model-x"));

        // The response's usage + provider-reported cost surfaced as a cumulative event.
        match sess.next_event().await.unwrap() {
            SessionEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.completion_tokens, 5);
                assert_eq!(u.cost_so_far, Some(0.0012)); // captured from usage.cost
            }
            other => panic!("expected a Usage event, got {other:?}"),
        }

        // finish reports the accumulated totals + the real cost; no abort → natural end.
        let outcome = sess.finish().await.unwrap();
        assert_eq!(outcome.prompt_tokens, Some(10));
        assert_eq!(outcome.completion_tokens, Some(5));
        assert_eq!(outcome.realized_cost, Some(0.0012));
        assert_eq!(outcome.terminated_by, None);
    }

    /// A two-request session whose first call succeeds and whose second dies on a 413 must tag the
    /// whole session `too_large`/413 — the on-the-wire counterpart of the unit test, exercising the
    /// real stream path where `note_success` fires *after* the body streams and `note_http_error`
    /// fires *before* it. Draining each response before sending the next fixes the ordering the way
    /// a sequential agentic CLI does.
    #[tokio::test]
    async fn terminal_413_after_a_success_tags_too_large_end_to_end() {
        let calls = Arc::new(AtomicU64::new(0));
        let up_calls = calls.clone();
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let up_calls = up_calls.clone();
                async move {
                    if up_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                            })),
                        )
                    } else {
                        (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            axum::Json(json!({"error": {"message": "request too large"}})),
                        )
                    }
                }
            }),
        );
        let outcome = run_two_request_session(up_app).await;
        assert_eq!(outcome.error_kind, Some(ErrorKind::TooLarge));
        assert_eq!(outcome.error_status, Some(413)); // raw terminal status preserved
    }

    /// The mirror case: a 429 on the first request that the second request recovers from must leave
    /// the session untagged — the stray throttle is not the session's outcome.
    #[tokio::test]
    async fn a_recovered_429_does_not_tag_the_session_end_to_end() {
        let calls = Arc::new(AtomicU64::new(0));
        let up_calls = calls.clone();
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let up_calls = up_calls.clone();
                async move {
                    if up_calls.fetch_add(1, Ordering::Relaxed) == 0 {
                        (
                            StatusCode::TOO_MANY_REQUESTS,
                            axum::Json(json!({"error": {"message": "slow down"}})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                            })),
                        )
                    }
                }
            }),
        );
        let outcome = run_two_request_session(up_app).await;
        assert_eq!(outcome.error_kind, None);
        assert_eq!(outcome.error_status, None);
    }

    /// Spawn `up_app` as the upstream, point a fresh proxy session at it, drive two sequential
    /// requests (draining each response so the stream's trailing success/finish signals land in
    /// order), and return the finished session outcome.
    async fn run_two_request_session(up_app: Router) -> SessionOutcome {
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("k".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::OpenRouter,
            None,
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "m".into(),
            real_slug: "prov/m".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let sess = backend.start(&pick, "al:al").await.unwrap();
        let proxy_addr = sess.endpoint().unwrap();

        let client = reqwest::Client::new();
        for _ in 0..2 {
            let resp = client
                .post(format!("http://{proxy_addr}/v1/chat/completions"))
                .json(&json!({"model": "al:al", "messages": []}))
                .send()
                .await
                .unwrap();
            // Draining the body runs the stream generator to completion, including the trailing
            // note_success / note_finish_reason; the next request is issued only afterwards.
            let _ = resp.text().await.unwrap();
        }
        sess.finish().await.unwrap()
    }

    #[test]
    fn classify_http_separates_too_large_from_bad_request_and_rate_limit() {
        // 413 is its own signal (request too large / TPM cap), NOT a malformed 400 or a 429 throttle.
        assert_eq!(classify_http(StatusCode::PAYLOAD_TOO_LARGE), ErrorKind::TooLarge);
        assert_eq!(classify_http(StatusCode::TOO_MANY_REQUESTS), ErrorKind::RateLimit);
        // 404 is its own persistent avoid-signal (model/route unavailable to you), NOT a malformed 400.
        assert_eq!(classify_http(StatusCode::NOT_FOUND), ErrorKind::Unavailable);
        assert_eq!(classify_http(StatusCode::BAD_REQUEST), ErrorKind::BadRequest);
        assert_eq!(
            classify_http(StatusCode::UNPROCESSABLE_ENTITY),
            ErrorKind::BadRequest
        );
        assert_eq!(classify_http(StatusCode::UNAUTHORIZED), ErrorKind::Auth);
        assert_eq!(
            classify_http(StatusCode::SERVICE_UNAVAILABLE),
            ErrorKind::Http5xx
        );
        // Unavailable is a full-weight avoid-signal (like TooLarge), and round-trips through the wire.
        assert_eq!(ErrorKind::Unavailable.loss_weight(), 1.0);
        assert_eq!(
            ErrorKind::from_wire("unavailable"),
            Some(ErrorKind::Unavailable)
        );
        assert_eq!(ErrorKind::Unavailable.as_str(), "unavailable");
    }

    #[test]
    fn failure_encoding_round_trips_and_never_collides_with_a_status() {
        for f in [
            Failure::Network,
            Failure::BodyError,
            Failure::Http(StatusCode::PAYLOAD_TOO_LARGE),
            Failure::Http(StatusCode::TOO_MANY_REQUESTS),
        ] {
            assert_eq!(Failure::decode(f.encode()), Some(f));
        }
        // 0 is the "no failure" sentinel; the < 100 sentinels can never be a real HTTP status.
        assert_eq!(Failure::decode(0), None);
        assert!(Failure::Http(StatusCode::CONTINUE).encode() >= 100);
    }

    /// A terminal transport failure tags the session even when earlier requests completed cleanly —
    /// the case that previously mislabelled a session (`truncated`/413) or dropped the kind entirely
    /// (`NULL`/429) because a single earlier success suppressed the HTTP failure.
    #[test]
    fn terminal_failure_outranks_an_earlier_success() {
        let cum = Cumulative::default();
        cum.note_success(); // an earlier request completed cleanly
        cum.note_http_error(StatusCode::PAYLOAD_TOO_LARGE); // …then the session died on a 413
        assert_eq!(cum.error_kind(), Some(ErrorKind::TooLarge));
        assert_eq!(cum.error_status(), Some(413));

        let cum = Cumulative::default();
        cum.note_success();
        cum.note_http_error(StatusCode::TOO_MANY_REQUESTS); // …then a terminal 429
        assert_eq!(cum.error_kind(), Some(ErrorKind::RateLimit));
        assert_eq!(cum.error_status(), Some(429));
    }

    /// A failure the CLI recovered from (a later request succeeded) does not tag the session: the
    /// stray status is dropped rather than recorded as the session's outcome.
    #[test]
    fn a_recovered_failure_does_not_tag_the_session() {
        let cum = Cumulative::default();
        cum.note_http_error(StatusCode::TOO_MANY_REQUESTS); // a mid-session throttle…
        cum.note_success(); // …that the next request recovered from
        assert_eq!(cum.error_kind(), None);
        assert_eq!(cum.error_status(), None);
    }

    /// A terminal truncation (finish_reason=length) on the last completion still tags the session,
    /// and the content-level tag only applies when no transport failure came after it.
    #[test]
    fn terminal_truncation_tags_when_no_later_failure() {
        let cum = Cumulative::default();
        cum.note_success();
        cum.note_finish_reason("length");
        assert_eq!(cum.error_kind(), Some(ErrorKind::Truncated));
        assert_eq!(cum.error_status(), None); // no HTTP failure

        // A transport failure after the truncated completion outranks the content tag.
        cum.note_http_error(StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(cum.error_kind(), Some(ErrorKind::Http5xx));
        assert_eq!(cum.error_status(), Some(503));
    }

    /// A session whose requests all fail upstream is tagged with the derived error_kind and the raw
    /// HTTP status (never-guess ground truth).
    #[tokio::test]
    async fn failed_session_tags_error_kind_and_status() {
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    axum::Json(json!({"error": {"message": "rate limited"}})),
                )
            }),
        );
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("k".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::OpenRouter,
            None,
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "m".into(),
            real_slug: "prov/m".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let sess = backend.start(&pick, "al:al").await.unwrap();
        let addr = sess.endpoint().unwrap();
        let _ = reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&json!({"model": "al:al", "messages": []})).unwrap())
            .send()
            .await
            .unwrap();

        let outcome = sess.finish().await.unwrap();
        assert_eq!(outcome.error_kind, Some(ErrorKind::RateLimit));
        assert_eq!(outcome.error_status, Some(429)); // raw status preserved
    }

    /// `GET /v1/models` is served locally as just the alias — the provider's real catalog is never
    /// forwarded, so a CLI's model list can't deblind the session.
    #[tokio::test]
    async fn models_list_returns_only_the_alias() {
        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("test-key".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::OpenRouter,
            None,
        )
        .unwrap();
        // base_url points nowhere reachable — the intercept must answer without forwarding upstream.
        let pick = Pick {
            canonical_key: "model-x".into(),
            real_slug: "prov/model-x".into(),
            endpoint: crate::VettedEndpoint::new("http://127.0.0.1:1/v1"),
        };
        let sess = backend.start(&pick, "al:al").await.unwrap();
        let addr = sess.endpoint().unwrap();

        let text = reqwest::Client::new()
            .get(format!("http://{addr}/v1/models"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
        assert_eq!(v["data"][0]["id"], "al:al");
        assert!(
            !text.contains("prov/model-x"),
            "real slug must not leak in the model list"
        );
    }

    /// The non-ZDR accountability trail (feature-gated): every forwarded request appends one
    /// `<UTC hour> \t real_slug` line to the 0600 audit file. Aggregate accountability only:
    /// no session id (the store keys ratings on it — an id would join the file onto the ratings
    /// table and deblind), no alias, and no sub-hour time precision.
    #[cfg(feature = "allow-non-zdr")]
    #[tokio::test]
    async fn non_zdr_audit_appends_one_line_per_forwarded_request() {
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                axum::Json(json!({
                    "choices": [{"message": {"content": "ok"}}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                }))
            }),
        );
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let audit_path = tmp.path().join("non-zdr-audit.log");
        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("k".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::NoZdr,
            None,
        )
        .unwrap()
        .with_non_zdr_audit(audit_path.clone(), Some(config::today_epoch_days() + 1));
        let pick = Pick {
            canonical_key: "non-zdr-model".into(),
            real_slug: "example/non-zdr-model".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let sess = backend.start(&pick, "al:al").await.unwrap();
        let addr = sess.endpoint().unwrap();

        let client = reqwest::Client::new();
        for _ in 0..2 {
            let resp = client
                .post(format!("http://{addr}/v1/chat/completions"))
                .json(&json!({"model": "al:al", "messages": []}))
                .send()
                .await
                .unwrap();
            assert!(resp.status().is_success());
            let _ = resp.text().await;
        }
        let _ = sess.finish().await.unwrap();

        let text = std::fs::read_to_string(&audit_path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per forwarded request: {text:?}");
        for line in lines {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 2, "hour bucket + slug only: {line:?}");
            // Whole-hour UTC bucket: exactly `YYYY-MM-DDTHH` — no minutes or seconds to pin a
            // session by. (`NaiveDateTime::parse_from_str` needs a fully-determined datetime,
            // so validate the date part and the two-digit hour separately.)
            let (day, hour) = fields[0]
                .split_once('T')
                .unwrap_or_else(|| panic!("bucket must carry a T separator: {line:?}"));
            assert!(
                chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok(),
                "bucket date must be YYYY-MM-DD: {line:?}"
            );
            assert!(
                hour.len() == 2 && hour.parse::<u8>().is_ok_and(|h| h < 24),
                "bucket time must be a two-digit hour 00-23: {line:?}"
            );
            assert_eq!(fields[1], "example/non-zdr-model");
            assert!(
                !line.contains("al:al"),
                "the alias must never enter the audit file"
            );
        }
        let mode = std::fs::metadata(&audit_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "audit file must be 0600");
    }

    /// Fail-closed: when the audit record cannot be written the request is REFUSED — the upstream
    /// must never see it. (The audit path is a directory, so every append fails.)
    #[cfg(feature = "allow-non-zdr")]
    #[tokio::test]
    async fn non_zdr_audit_failure_refuses_the_request() {
        let hit = Arc::new(AtomicBool::new(false));
        let up_hit = hit.clone();
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let up_hit = up_hit.clone();
                async move {
                    up_hit.store(true, Ordering::Relaxed);
                    axum::Json(json!({"choices": []}))
                }
            }),
        );
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("k".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::NoZdr,
            None,
        )
        .unwrap()
        .with_non_zdr_audit(
            tmp.path().to_path_buf(), // a directory: append must fail
            Some(config::today_epoch_days() + 1),
        );
        let pick = Pick {
            canonical_key: "non-zdr-model".into(),
            real_slug: "example/non-zdr-model".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let sess = backend.start(&pick, "al:al").await.unwrap();
        let addr = sess.endpoint().unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&json!({"model": "al:al", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503, "refused, not forwarded");
        let _ = sess.finish().await.unwrap();
        assert!(
            !hit.load(Ordering::Relaxed),
            "upstream must never see the request"
        );
    }

    /// The per-request expiry bound (feature-gated): once the armed attestation's `expires`
    /// date has passed, a STANDING proxy refuses further non-ZDR routing — fail-closed, the
    /// upstream never sees anything. This is what makes the 30-day cap a true maximum even when
    /// the proxy outlives the window it was started inside.
    #[cfg(feature = "allow-non-zdr")]
    #[tokio::test]
    async fn non_zdr_expiry_refuses_a_standing_proxies_requests() {
        let hit = Arc::new(AtomicBool::new(false));
        let up_hit = hit.clone();
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let up_hit = up_hit.clone();
                async move {
                    up_hit.store(true, Ordering::Relaxed);
                    axum::Json(json!({"choices": []}))
                }
            }),
        );
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        // Expired yesterday relative to this machine's clock: always past, whatever today is.
        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("k".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::NoZdr,
            None,
        )
        .unwrap()
        .with_non_zdr_audit(
            tmp.path().join("non-zdr-audit.log"),
            Some(config::today_epoch_days() - 1),
        );
        let pick = Pick {
            canonical_key: "non-zdr-model".into(),
            real_slug: "example/non-zdr-model".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let sess = backend.start(&pick, "al:al").await.unwrap();
        let addr = sess.endpoint().unwrap();

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .json(&json!({"model": "al:al", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503, "refused once expired");
        let text = resp.text().await.unwrap();
        assert!(text.contains("expired"), "refusal names the cause: {text}");
        let _ = sess.finish().await.unwrap();
        assert!(
            !hit.load(Ordering::Relaxed),
            "upstream must never see a request past the attestation's window"
        );
    }

    /// File-level mirror of run.rs's dir-tightening test: `.mode(0o600)` applies only at
    /// creation, so a pre-existing looser audit file (older build, manual touch, umask) must be
    /// tightened idempotently before records land in it.
    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn non_zdr_audit_file_gets_0600_even_when_it_pre_exists_looser() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("non-zdr-audit.log");

        let audit = NonZdrAudit {
            path: path.clone(),
            expires_epoch_days: Some(config::today_epoch_days() + 1),
        };
        // Fresh creation must be 0600 outright.
        audit.append_sync("example/non-zdr-model").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "fresh file must be 0600");

        // A file left looser by an older build / manual touch must be tightened: a loose audit
        // file discloses the pay-with-data configuration to every local user who can read it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        audit.append_sync("example/non-zdr-model").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "pre-existing loose file must be tightened"
        );

        // Idempotent.
        audit.append_sync("example/non-zdr-model").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// Expiry refusals are fail-closed and write nothing: an attestation whose window ended — or
    /// that carries no bounded lifetime at all — refuses the append itself.
    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn non_zdr_audit_refuses_expired_or_unbounded_attestations() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("non-zdr-audit.log");
        let today = config::today_epoch_days();

        // Live through today (+1 stays valid even if the UTC day flips mid-test): appends work.
        let live = NonZdrAudit {
            path: path.clone(),
            expires_epoch_days: Some(today + 1),
        };
        live.append_sync("example/non-zdr-model").unwrap();

        // Expired yesterday: refused.
        let dead = NonZdrAudit {
            path: path.clone(),
            expires_epoch_days: Some(today - 1),
        };
        let err = dead.append_sync("example/non-zdr-model").unwrap_err();
        assert!(matches!(err, AuditRefusal::Expired(_)), "{err:?}");

        // No bounded lifetime recorded: refused — an unbounded capability never arms.
        let unbounded = NonZdrAudit {
            path: path.clone(),
            expires_epoch_days: None,
        };
        let err = unbounded.append_sync("example/non-zdr-model").unwrap_err();
        assert!(matches!(err, AuditRefusal::Unbounded), "{err:?}");

        // Only the live record reached disk.
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "refusals must not write: {text:?}");
    }

    /// Concurrency safety of the audit log: each record is one small buffer written under
    /// `O_APPEND`, whose per-syscall atomicity on regular files lands it as an indivisible line.
    /// Many threads appending to the SAME path (the shared-log situation two blindcoder processes
    /// would hit) must therefore produce exactly one intact, parseable line per append — never
    /// interleaved fragments.
    #[cfg(feature = "allow-non-zdr")]
    #[test]
    fn non_zdr_audit_concurrent_appends_stay_whole_lines() {
        const THREADS: usize = 8;
        const APPENDS_PER_THREAD: usize = 64;
        let tmp = tempfile::tempdir().unwrap();
        let audit_path = tmp.path().join("non-zdr-audit.log");

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = audit_path.clone();
                std::thread::spawn(move || {
                    let audit = NonZdrAudit {
                        path,
                        // Valid all day: these threads exercise atomicity, not expiry.
                        expires_epoch_days: Some(config::today_epoch_days() + 1),
                    };
                    let slug = format!("example/concurrent-{t}");
                    for _ in 0..APPENDS_PER_THREAD {
                        audit.append_sync(&slug).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let text = std::fs::read_to_string(&audit_path).unwrap();
        assert_eq!(
            text.lines().count(),
            THREADS * APPENDS_PER_THREAD,
            "every append landed as exactly one line: {text:?}"
        );
        let mut per_slug = std::collections::HashMap::<String, usize>::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 2, "each record is one intact line: {line:?}");
            assert!(
                {
                    let (day, hour) = fields[0]
                        .split_once('T')
                        .unwrap_or_else(|| panic!("fragmented bucket: {line:?}"));
                    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok()
                        && hour.len() == 2
                        && hour.parse::<u8>().is_ok_and(|h| h < 24)
                },
                "intact YYYY-MM-DDTHH hour bucket: {line:?}"
            );
            *per_slug.entry(fields[1].to_string()).or_insert(0) += 1;
        }
        for t in 0..THREADS {
            assert_eq!(
                per_slug.get(&format!("example/concurrent-{t}")),
                Some(&APPENDS_PER_THREAD),
                "thread {t}: every record arrived uncorrupted"
            );
        }
    }
    /// At the `replay` capture level, a completed exchange writes all four legs (cli_request,
    /// provider_request, provider_response, cli_response) byte-exact to the WARC archive — with the
    /// raw upstream body kept unmasked and the CLI-facing body masked.
    #[tokio::test]
    async fn replay_capture_writes_four_legs_raw_and_masked() {
        let up_app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                axum::Json(json!({
                    "model": "prov/model-x", "provider": "AcmeProv",
                    "choices": [{"message": {"content": "ok"}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.0012}
                }))
            }),
        );
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(up_listener, up_app).await.unwrap();
        });

        let tmp = tempfile::tempdir().unwrap();
        let warc_path = tmp.path().join("sess.warc");
        let backend = ProxyBackend::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("test-key".into()),
            vec![],
            serde_json::Map::new(),
            Privacy::OpenRouter,
            Some(warc_path.clone()),
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "model-x".into(),
            real_slug: "prov/model-x".into(),
            endpoint: crate::VettedEndpoint::new(format!("http://{up_addr}/v1")),
        };
        let mut sess = backend.start(&pick, "al:al").await.unwrap();
        let proxy_addr = sess.endpoint().unwrap();

        let _ = reqwest::Client::new()
            .post(format!("http://{proxy_addr}/v1/chat/completions"))
            .json(&json!({"model": "al:al", "messages": []}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // Drain the usage event, then finish — finish awaits the writer so the file is flushed.
        let _ = sess.next_event().await;
        let _ = sess.finish().await.unwrap();

        // The archive holds the four legs, tagged and grouped into one exchange. (Open the file
        // ourselves — warc 0.4's `WarcReader::from_path` passes an invalid `create`-without-`write`
        // OpenOptions combo and errors on a read-only open.)
        let file = std::io::BufReader::new(std::fs::File::open(&warc_path).unwrap());
        let records: Vec<_> = warc::WarcReader::new(file)
            .iter_records()
            .map(|r| r.unwrap())
            .collect();
        let leg = |name: &str| {
            records
                .iter()
                .find(|r| {
                    r.header(WarcHeader::Unknown("x-blindcoder-leg".into()))
                        .as_deref()
                        == Some(name)
                })
                .unwrap_or_else(|| panic!("missing leg {name}"))
        };
        assert_eq!(records.len(), 4, "four legs archived");
        for name in [
            "cli_request",
            "provider_request",
            "provider_response",
            "cli_response",
        ] {
            assert_eq!(
                leg(name)
                    .header(WarcHeader::Unknown("x-blindcoder-exchange".into()))
                    .as_deref(),
                Some("0"),
                "{name} grouped into exchange 0"
            );
        }
        // The request the CLI sent carries the blind alias; the request forwarded upstream carries the
        // real slug — the archive preserves both sides of the rewrite verbatim.
        let cli_req = std::str::from_utf8(leg("cli_request").body()).unwrap();
        let prov_req = std::str::from_utf8(leg("provider_request").body()).unwrap();
        assert!(cli_req.contains("al:al") && !cli_req.contains("prov/model-x"));
        assert!(prov_req.contains("prov/model-x") && !prov_req.contains("al:al"));
        // The upstream response is kept RAW (real slug + provider fingerprint intact); the CLI-facing
        // response is the MASKED copy (alias only, fingerprint stripped).
        let prov_resp = std::str::from_utf8(leg("provider_response").body()).unwrap();
        let cli_resp = std::str::from_utf8(leg("cli_response").body()).unwrap();
        assert!(
            prov_resp.contains("prov/model-x") && prov_resp.contains("AcmeProv"),
            "raw upstream body"
        );
        assert!(
            cli_resp.contains("al:al") && !cli_resp.contains("prov/model-x"),
            "masked CLI body"
        );
        assert!(
            !cli_resp.contains("AcmeProv"),
            "provider fingerprint stripped from the CLI leg"
        );
    }
}
