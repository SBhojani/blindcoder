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
use std::sync::atomic::{AtomicU64, Ordering};
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

use crate::rewrite::{parse_usage, rewrite_request};
use crate::{AbortReason, Backend, Pick, Session, SessionEvent, SessionOutcome, UsageSnapshot};

/// A ready-to-run forwarding proxy. Constructed per session by the router with the picked
/// provider's credentials and passthrough hooks; the per-model target (`base_url`, `real_slug`)
/// arrives in [`Backend::start`]'s [`Pick`].
pub struct ProxyBackend {
    bind_addr: SocketAddr,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
    extra_body: serde_json::Map<String, Value>,
    client: reqwest::Client,
}

impl ProxyBackend {
    /// Build a proxy that will listen on `bind_addr` (use port 0 for an ephemeral port; read the
    /// bound address back via [`Session::endpoint`]).
    pub fn new(
        bind_addr: SocketAddr,
        api_key: Option<String>,
        extra_headers: Vec<(String, String)>,
        extra_body: serde_json::Map<String, Value>,
    ) -> Result<Self> {
        Ok(Self {
            bind_addr,
            api_key,
            extra_headers,
            extra_body,
            client: reqwest::Client::builder().build().context("building HTTP client")?,
        })
    }
}

/// Cumulative per-session token usage, shared between the request handlers and the session handle.
#[derive(Default)]
struct Cumulative {
    prompt: AtomicU64,
    completion: AtomicU64,
}

impl Cumulative {
    /// Add one response's usage and return the new cumulative snapshot.
    fn add(&self, u: &UsageSnapshot) -> UsageSnapshot {
        let prompt = self.prompt.fetch_add(u.prompt_tokens, Ordering::Relaxed) + u.prompt_tokens;
        let completion =
            self.completion.fetch_add(u.completion_tokens, Ordering::Relaxed) + u.completion_tokens;
        UsageSnapshot { prompt_tokens: prompt, completion_tokens: completion, cost_so_far: None }
    }

    fn totals(&self) -> (u64, u64) {
        (self.prompt.load(Ordering::Relaxed), self.completion.load(Ordering::Relaxed))
    }
}

/// Everything a request handler needs to forward and account one call.
struct ProxyState {
    base_url: String,
    real_slug: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
    extra_body: serde_json::Map<String, Value>,
    client: reqwest::Client,
    usage_tx: mpsc::UnboundedSender<UsageSnapshot>,
    cumulative: Arc<Cumulative>,
}

/// Pull cumulative token usage out of a completed response body — either a plain JSON object or an
/// SSE stream whose final `data:` frame carries `usage` (OpenAI streaming with `include_usage`).
fn extract_usage(body: &[u8]) -> Option<UsageSnapshot> {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(u) = parse_usage(&v) {
            return Some(u);
        }
    }
    let text = std::str::from_utf8(body).ok()?;
    let mut found = None;
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("data:") else { continue };
        let rest = rest.trim();
        if rest == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(rest) {
            if let Some(u) = parse_usage(&v) {
                found = Some(u); // keep the last one — streaming usage arrives in the final frame
            }
        }
    }
    found
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
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream request failed: {e}")).into_response(),
    };

    let status = upstream.status();
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();

    // Stream chunks straight back while accumulating the full body, then account usage from it.
    let st2 = st.clone();
    let stream = async_stream::stream! {
        let mut acc: Vec<u8> = Vec::new();
        let mut bytes_stream = upstream.bytes_stream();
        while let Some(item) = bytes_stream.next().await {
            match item {
                Ok(chunk) => {
                    acc.extend_from_slice(&chunk);
                    yield Ok::<Bytes, std::io::Error>(chunk);
                }
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                    break;
                }
            }
        }
        if let Some(u) = extract_usage(&acc) {
            let snapshot = st2.cumulative.add(&u);
            let _ = st2.usage_tx.send(snapshot);
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
    aborted: Option<AbortReason>,
    local_addr: SocketAddr,
    ended: bool,
}

#[async_trait]
impl Backend for ProxyBackend {
    async fn start(&self, pick: &Pick, _alias: &str) -> Result<Box<dyn Session>> {
        let listener = TcpListener::bind(self.bind_addr)
            .await
            .with_context(|| format!("binding proxy listener on {}", self.bind_addr))?;
        let local_addr = listener.local_addr()?;

        let (usage_tx, usage_rx) = mpsc::unbounded_channel();
        let cumulative = Arc::new(Cumulative::default());
        let state = Arc::new(ProxyState {
            base_url: pick.base_url.trim_end_matches('/').to_string(),
            real_slug: pick.real_slug.clone(),
            api_key: self.api_key.clone(),
            extra_headers: self.extra_headers.clone(),
            extra_body: self.extra_body.clone(),
            client: self.client.clone(),
            usage_tx,
            cumulative: cumulative.clone(),
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
        UsageSnapshot { prompt_tokens, completion_tokens, cost_so_far: None }
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
            let _ = server.await;
        }
        let (prompt_tokens, completion_tokens) = self.cumulative.totals();
        Ok(SessionOutcome {
            realized_cost: None,
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            error_kind: None,
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
    use serde_json::json;
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
                        "choices": [{"message": {"content": "ok"}}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
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

        // Upstream saw the real slug, never the alias — the rewrite happened.
        assert_eq!(captured.lock().unwrap().as_deref(), Some("prov/model-x"));

        // The response's usage surfaced as a cumulative event.
        match sess.next_event().await.unwrap() {
            SessionEvent::Usage(u) => {
                assert_eq!(u.prompt_tokens, 10);
                assert_eq!(u.completion_tokens, 5);
            }
            other => panic!("expected a Usage event, got {other:?}"),
        }

        // finish reports the accumulated totals; no abort → natural end.
        let outcome = sess.finish().await.unwrap();
        assert_eq!(outcome.prompt_tokens, Some(10));
        assert_eq!(outcome.completion_tokens, Some(5));
        assert_eq!(outcome.terminated_by, None);
    }
}
