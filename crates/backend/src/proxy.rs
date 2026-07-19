//! The M0 forwarding transport: a small streaming reverse proxy behind the `Backend`/`Session`
//! seam. It binds a localhost listener, and for each request rewrites the blind `model` to the
//! resolved real slug (via [`rewrite_request`]), forwards to the provider endpoint with the API
//! key and any `extra_headers`, streams the response straight back to the caller, and — after each
//! response completes — parses the `usage` block to accumulate token counts. Those counts surface
//! as `Usage` events so the router's cost cap can act on them.
//!
//! It is provider-blind: nothing here branches on which backend it talks to. The M1 tee grows from
//! this same shape (raw capture + fail-closed privacy) behind the unchanged trait.

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

use crate::rewrite::{mask_json_body, mask_sse_line, parse_usage, rewrite_request};
use crate::{
    AbortReason, Backend, ErrorKind, Pick, Session, SessionEvent, SessionOutcome, UsageSnapshot,
};

/// Map an upstream HTTP status to an [`ErrorKind`] (for the "no clean completion" case).
fn classify_http(code: u16) -> ErrorKind {
    match code {
        429 => ErrorKind::RateLimit,
        401 | 403 => ErrorKind::Auth,
        413 => ErrorKind::TooLarge, // request too large: context window or a per-minute token cap
        400..=499 => ErrorKind::BadRequest, // other 4xx: 400/404/422/…
        500..=599 => ErrorKind::Http5xx,
        // 1xx/3xx/≥600 shouldn't reach here (we only classify non-2xx final statuses; reqwest
        // follows redirects) — distinct Unknown bucket rather than mislabelling as bad_request.
        _ => ErrorKind::Unknown,
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
    capture_path: Option<PathBuf>,
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
        capture_path: Option<PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            bind_addr,
            api_key,
            extra_headers,
            extra_body,
            capture_path,
            client: reqwest::Client::builder()
                .build()
                .context("building HTTP client")?,
        })
    }
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
        let mut writer = match WarcWriter::from_path(&path) {
            Ok(w) => w,
            Err(_) => return, // capture is best-effort: a write failure never breaks a session
        };
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
#[derive(Default)]
struct Cumulative {
    prompt: AtomicU64,
    completion: AtomicU64,
    cost_nano: AtomicU64,
    has_cost: AtomicBool,
    any_success: AtomicBool,
    http_error: AtomicU64, // 0 = none, 1 = network (no response), else the HTTP status
    content_issue: AtomicU64, // 0 = none, 1 = truncated (length), 2 = refused (content_filter)
    body_error: AtomicBool, // a 2xx response whose body carried an `error`
}

impl Cumulative {
    /// Add one response's usage and return the new cumulative snapshot.
    fn add(&self, u: &UsageSnapshot) -> UsageSnapshot {
        let prompt = self.prompt.fetch_add(u.prompt_tokens, Ordering::Relaxed) + u.prompt_tokens;
        let completion = self
            .completion
            .fetch_add(u.completion_tokens, Ordering::Relaxed)
            + u.completion_tokens;
        if let Some(c) = u.cost_so_far {
            self.cost_nano
                .fetch_add((c * 1e9).round() as u64, Ordering::Relaxed);
            self.has_cost.store(true, Ordering::Relaxed);
        }
        UsageSnapshot {
            prompt_tokens: prompt,
            completion_tokens: completion,
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

    fn totals(&self) -> (u64, u64) {
        (
            self.prompt.load(Ordering::Relaxed),
            self.completion.load(Ordering::Relaxed),
        )
    }

    fn note_network(&self) {
        self.http_error.store(1, Ordering::Relaxed);
    }
    fn note_http_error(&self, code: u16) {
        self.http_error.store(code as u64, Ordering::Relaxed);
    }
    fn note_success(&self) {
        self.any_success.store(true, Ordering::Relaxed);
    }
    fn note_body_error(&self) {
        self.body_error.store(true, Ordering::Relaxed);
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

    /// Derive the session's failure tag: a transport-level failure when nothing completed cleanly,
    /// otherwise a content-level degradation (truncated / refused) from the last completion.
    fn error_kind(&self) -> Option<ErrorKind> {
        if !self.any_success.load(Ordering::Relaxed) {
            let http = self.http_error.load(Ordering::Relaxed);
            if http == 1 {
                return Some(ErrorKind::Network);
            }
            if http != 0 {
                return Some(classify_http(http as u16));
            }
            if self.body_error.load(Ordering::Relaxed) {
                return Some(ErrorKind::BadRequest);
            }
        }
        match self.content_issue.load(Ordering::Relaxed) {
            1 => Some(ErrorKind::Truncated),
            2 => Some(ErrorKind::Refused),
            _ => None,
        }
    }

    /// The raw upstream HTTP status of a failure, if there was an HTTP one. `None` for a network
    /// failure (sentinel `1`, no status) or no failure.
    fn error_status(&self) -> Option<u16> {
        match self.http_error.load(Ordering::Relaxed) {
            0 | 1 => None,
            code => Some(code as u16),
        }
    }
}

/// Everything a request handler needs to forward and account one call.
struct ProxyState {
    base_url: String,
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
    let url = format!("{}{}{}", st.base_url, suffix, query);

    // Rewrite only a JSON body that carries a model field; forward anything else untouched.
    let out_body: Vec<u8> = match serde_json::from_slice::<Value>(&body) {
        Ok(mut v) if v.get("model").is_some() => {
            let _ = rewrite_request(&mut v, &st.real_slug, &st.extra_body);
            serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
        }
        _ => body.to_vec(),
    };

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
            body: out_body.clone(),
        });
    }

    let mut req = st.client.request(method, &url).body(out_body);
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

    let upstream = match req.send().await {
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
        st.cumulative.note_http_error(status.as_u16());
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|c| c.to_str().ok())
        .map(|s| s.contains("event-stream"))
        .unwrap_or(false);

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
                            yield Ok::<Bytes, std::io::Error>(Bytes::from(out.into_bytes()));
                        }
                    }
                    Err(e) => { yield Err(std::io::Error::new(std::io::ErrorKind::Other, e)); break; }
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
                    Err(e) => { yield Err(std::io::Error::new(std::io::ErrorKind::Other, e)); break; }
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
            base_url: pick.base_url.trim_end_matches('/').to_string(),
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
        let (prompt_tokens, completion_tokens) = self.cumulative.totals();
        UsageSnapshot {
            prompt_tokens,
            completion_tokens,
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
        let (prompt_tokens, completion_tokens) = self.cumulative.totals();
        Ok(SessionOutcome {
            realized_cost: self.cumulative.cost(), // provider-reported when available, else None
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
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
            None,
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "model-x".into(),
            real_slug: "prov/model-x".into(),
            base_url: format!("http://{up_addr}/v1"),
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

    #[test]
    fn classify_http_separates_too_large_from_bad_request_and_rate_limit() {
        // 413 is its own signal (request too large / TPM cap), NOT a malformed 400 or a 429 throttle.
        assert_eq!(classify_http(413), ErrorKind::TooLarge);
        assert_eq!(classify_http(429), ErrorKind::RateLimit);
        assert_eq!(classify_http(400), ErrorKind::BadRequest);
        assert_eq!(classify_http(422), ErrorKind::BadRequest);
        assert_eq!(classify_http(401), ErrorKind::Auth);
        assert_eq!(classify_http(503), ErrorKind::Http5xx);
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
            None,
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "m".into(),
            real_slug: "prov/m".into(),
            base_url: format!("http://{up_addr}/v1"),
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
            None,
        )
        .unwrap();
        // base_url points nowhere reachable — the intercept must answer without forwarding upstream.
        let pick = Pick {
            canonical_key: "model-x".into(),
            real_slug: "prov/model-x".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
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
            Some(warc_path.clone()),
        )
        .unwrap();
        let pick = Pick {
            canonical_key: "model-x".into(),
            real_slug: "prov/model-x".into(),
            base_url: format!("http://{up_addr}/v1"),
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
