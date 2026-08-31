//! A session-scoped localhost reverse-proxy: dispatches each request to one
//! of several upstream targets based on the top-level `"model"` field in its
//! JSON body -- a known model routes to its own resolved provider; anything
//! else (parse failure, missing field, no match) falls through to a
//! `default` target. Every response streamed back is scanned for usage
//! accounting fields and recorded into a shared `UsageTracker`, regardless of
//! which target served it -- including the `default`/passthrough leg, so the
//! main session's own traffic is tracked too, not just traffic explicitly
//! routed to a resolved model.
//!
//! Boots at most once per `granite-cli launch` invocation (see
//! `ProxyServer::start`). Models and sub-agent bindings register their own
//! routes into the already-running proxy via `ProxyHandle::register_route`/
//! `set_default` rather than each spinning up a dedicated server -- this
//! works whether the caller registers before or after the proxy starts
//! accepting connections. See `docs/specs/0020-usage-tracking-proxy.md` and
//! `docs/specs/0021-sub-agent-capability.md`.
//!
//! Deliberately not Anthropic-specific in its request-body dispatch: it only
//! reads a top-level JSON `"model"` string, a shape OpenAI/Ollama-style
//! request bodies share too.

// Standard
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::{Stream, StreamExt, stream};

// Local
use crate::proxy::usage::{self, UsageStats, UsageTracker};
use crate::registry::Secret;
use crate::utils::subserver::SubServer;

use_channel!("PRXY");

/*-- public --*/

/// One destination the proxy can forward a request to.
pub struct UpstreamTarget {
    pub base_url: String,
    pub verify_ssl: bool,
    pub auth: UpstreamAuth,
}

/// How the proxy should handle auth headers when forwarding to a target.
#[derive(Clone)]
pub enum UpstreamAuth {
    /// Strip whatever auth the client sent and inject this instead (or send
    /// no auth at all if `None`) -- for a known granite-cli-resolved
    /// provider, whose credentials are unrelated to whatever the client
    /// attached.
    Inject(Option<Secret>),
    /// Forward the client's auth headers byte-for-byte -- for the real
    /// upstream, so the client's own credential precedence (subscription
    /// OAuth, API key, bearer token, etc.) keeps working untouched.
    Passthrough,
}

/// A cheap, `Clone`-able handle to a running session proxy. `local_base_url`
/// is constant for the whole session -- every registered route shares the
/// one listening port; dispatch happens purely by the request's own
/// `"model"` field.
#[derive(Clone)]
pub struct ProxyHandle {
    pub local_base_url: String,
    state: ProxyState,
}

impl std::fmt::Debug for ProxyHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHandle")
            .field("local_base_url", &self.local_base_url)
            .finish_non_exhaustive()
    }
}

impl ProxyHandle {
    /// Registers (or replaces) the target for one model-name route, and
    /// records usage under `label` for traffic dispatched to it. Works
    /// whether the proxy is already serving traffic or not. Fails only if
    /// `target`'s TLS client can't be built.
    pub fn register_route(
        &self,
        model_name: String,
        target: UpstreamTarget,
        label: String,
    ) -> anyhow::Result<()> {
        let resolved = ResolvedTarget::build(target)?;
        let mut table = self.state.routing.write().unwrap();
        table.routes.insert(model_name.clone(), resolved);
        table.labels.insert(model_name, label);
        Ok(())
    }

    /// Registers a provider with its known model names and a prefix-based
    /// fallback route. When a request's `"model"` field doesn't match any
    /// exact route, the proxy checks if the model name starts with the
    /// provider's prefix (e.g. "gpt" for OpenAI, "claude" for Anthropic) and
    /// routes to this provider's target. This ensures traffic for unknown
    /// models from a known provider still reaches the right upstream and gets
    /// tracked. Works whether the proxy is already serving traffic or not.
    pub fn register_provider(
        &self,
        provider_name: &str,
        base_url: String,
        models: Vec<String>,
        label: String,
    ) -> anyhow::Result<()> {
        let resolved = ResolvedTarget::build(UpstreamTarget {
            base_url,
            verify_ssl: true,
            auth: UpstreamAuth::Passthrough,
        })?;
        let mut table = self.state.routing.write().unwrap();
        // Register known models as exact routes
        for model in &models {
            table.routes.insert(model.clone(), resolved.clone());
            table.labels.insert(model.clone(), label.clone());
        }
        // Register the provider prefix for fallback matching
        table
            .provider_prefixes
            .push((provider_name.to_string(), resolved, label));
        Ok(())
    }

    /// Sets (or replaces) the fallback target used for requests whose
    /// `"model"` field doesn't match any registered route or provider prefix.
    pub fn set_default(&self, target: UpstreamTarget, label: String) -> anyhow::Result<()> {
        let resolved = ResolvedTarget::build(target)?;
        let mut table = self.state.routing.write().unwrap();
        table.default = resolved;
        table.default_label = label;
        Ok(())
    }

    /// Points the default (fallback) target at whatever is already
    /// registered under `model_name`, rather than needing fresh connection
    /// details. Use this when the caller's only view of a model's
    /// connection info may itself already be wrapped to point at this same
    /// proxy (e.g. a capability whose model went through
    /// `ModelSource::take`) -- reusing the real target that was registered
    /// eagerly at that time avoids re-deriving (and getting wrong) an
    /// `UpstreamTarget` from already-proxied data. Fails if `model_name`
    /// has no registered route.
    pub fn set_default_from_route(&self, model_name: &str) -> anyhow::Result<()> {
        let mut table = self.state.routing.write().unwrap();
        let target = table
            .routes
            .get(model_name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no route registered for model '{model_name}'"))?;
        let label = table
            .labels
            .get(model_name)
            .cloned()
            .unwrap_or_else(|| model_name.to_string());
        table.default = target;
        table.default_label = label;
        Ok(())
    }

    /// The shared tracker every route (including the default target)
    /// records usage into.
    pub fn tracker(&self) -> Arc<UsageTracker> {
        Arc::clone(&self.state.tracker)
    }
}

/// A running session proxy. Bind an ephemeral localhost port and start
/// dispatching/tracking immediately; routes and the default target may be
/// registered at any time via `handle`, before or after real traffic starts
/// arriving.
pub struct ProxyServer {
    pub handle: ProxyHandle,
    inner: SubServer,
}

impl ProxyServer {
    /// The built-in default target forwards to the ambient
    /// `ANTHROPIC_BASE_URL` (or the well-known Anthropic API if unset) with
    /// the client's own auth passed through untouched -- callers override
    /// this via `handle.set_default` once a specific main model is known.
    ///
    /// Synchronous -- see `SubServer::spawn` -- so this can be called from
    /// inside sync code as long as a Tokio runtime is already running
    /// somewhere up the call stack.
    pub fn start() -> anyhow::Result<Self> {
        let ambient_base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let default = ResolvedTarget::build(UpstreamTarget {
            base_url: ambient_base_url,
            verify_ssl: true,
            auth: UpstreamAuth::Passthrough,
        })?;
        let routing = Arc::new(RwLock::new(RoutingTable {
            default,
            default_label: "default".to_string(),
            routes: HashMap::new(),
            labels: HashMap::new(),
            provider_prefixes: Vec::new(),
        }));
        let tracker = Arc::new(UsageTracker::new());
        let state = ProxyState { routing, tracker };

        let app = Router::new()
            .fallback(any(proxy_handler))
            .with_state(state.clone());
        let inner = SubServer::spawn(app, "session proxy")?;
        let local_base_url = format!("http://{}", inner.local_addr);

        Ok(Self {
            handle: ProxyHandle {
                local_base_url,
                state,
            },
            inner,
        })
    }

    /// Signal the server to stop accepting new connections and wait for it
    /// to finish draining in-flight ones.
    pub async fn shutdown(self) {
        self.inner.shutdown().await;
    }
}

/*-- private --*/

struct ResolvedTarget {
    base_url: String,
    auth: UpstreamAuth,
    client: reqwest::Client,
}

impl Clone for ResolvedTarget {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            auth: self.auth.clone(),
            client: self.client.clone(),
        }
    }
}

impl ResolvedTarget {
    fn build(target: UpstreamTarget) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(!target.verify_ssl)
            .build()?;
        Ok(Self {
            base_url: target.base_url,
            auth: target.auth,
            client,
        })
    }
}

struct RoutingTable {
    default: ResolvedTarget,
    default_label: String,
    routes: HashMap<String, ResolvedTarget>,
    labels: HashMap<String, String>,
    /// Provider prefixes for fallback routing: when a request's model doesn't
    /// match any exact route, check if it starts with a registered provider's
    /// prefix (e.g. "gpt" matches OpenAI, "claude" matches Anthropic). The
    /// first matching provider in insertion order is used.
    provider_prefixes: Vec<(String, ResolvedTarget, String)>,
}

impl RoutingTable {
    /// Picks the target and tracking label for one request body. Returns
    /// owned values so the caller can drop the read lock before doing any
    /// `.await`-ing forward work.
    ///
    /// A matched route uses its own registered label (typically a
    /// sub-agent/capability name). Traffic that falls through to the
    /// default target is labeled by the request's own `"model"` field when
    /// one is present, rather than the generic `default_label` -- so
    /// several distinct upstream models sharing the default/passthrough
    /// target (e.g. the main session's model plus Claude Code's own
    /// background-model calls) still show up as separate rows in the usage
    /// summary instead of being lumped into one "default" bucket.
    /// `default_label` is used only when the body has no identifiable
    /// model name at all (non-JSON body, or a missing/non-string `"model"`
    /// field).
    fn target_and_label_for(&self, body: &[u8]) -> (ResolvedTarget, String) {
        let model = model_from_body(body);
        if let Some(model) = &model
            && let Some(target) = self.routes.get(model)
        {
            let label = self
                .labels
                .get(model)
                .cloned()
                .unwrap_or_else(|| model.clone());
            return (target.clone(), label);
        }
        // No exact model match — check provider prefixes as fallback.
        // E.g. "gpt-4o" matches the "gpt" prefix for OpenAI providers,
        // "claude-sonnet-4-5" matches "claude" for Anthropic, etc.
        if let Some(model) = &model {
            for (prefix, target, label) in &self.provider_prefixes {
                if model.starts_with(prefix) {
                    return (target.clone(), label.clone());
                }
            }
        }
        let label = model.unwrap_or_else(|| self.default_label.clone());
        (self.default.clone(), label)
    }
}

/// Reads the top-level `"model"` string out of a JSON request body. A
/// non-JSON body, or one without a string `"model"` field, yields `None` --
/// not an error -- so the caller falls through to the default target.
fn model_from_body(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let model = value.get("model")?.as_str().map(str::to_string);
    alog_channel!(MessageLevel::Debug4, "Found model: {:#?}", model);
    model
}

/// Headers that must not be blindly forwarded in either direction:
/// connection-specific framing that's re-derived for the new connection.
/// Auth headers (`authorization`/`x-api-key`) are handled separately per
/// `UpstreamAuth`, not covered by this list.
fn is_hop_by_hop_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authenticate"
            | "proxy-authorization"
    )
}

fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key"
    )
}

#[derive(Clone)]
struct ProxyState {
    routing: Arc<RwLock<RoutingTable>>,
    tracker: Arc<UsageTracker>,
}

async fn proxy_handler(
    State(state): State<ProxyState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match forward(&state, method, uri, headers, body).await {
        Ok(response) => response,
        Err(e) => {
            alog_channel!(MessageLevel::Warning, "session proxy forward failed: {e}");
            (StatusCode::BAD_GATEWAY, format!("proxy error: {e}")).into_response()
        }
    }
}

async fn forward(
    state: &ProxyState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> anyhow::Result<Response> {
    let body_bytes = axum::body::to_bytes(body, usize::MAX).await?;
    let (target, label) = {
        let table = state.routing.read().unwrap();
        table.target_and_label_for(&body_bytes)
    };

    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!(
        "{}{}",
        target.base_url.trim_end_matches('/'),
        path_and_query
    );

    let outbound_method = reqwest::Method::from_bytes(method.as_str().as_bytes())?;
    alog_channel!(
        MessageLevel::Debug4,
        "Routing to {:#?} (label {:#?})",
        &url,
        &label
    );
    let mut outbound = target.client.request(outbound_method, &url);
    let strip_client_auth = matches!(target.auth, UpstreamAuth::Inject(_));
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name.as_str()) {
            continue;
        }
        if strip_client_auth && is_auth_header(name.as_str()) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            outbound = outbound.header(name.as_str(), v);
        }
    }
    if let UpstreamAuth::Inject(Some(key)) = &target.auth {
        // No `ApiType` is available at this layer, so send both header
        // schemes a provider might expect -- harmless, since a real
        // upstream only reads the one it understands.
        outbound = outbound.header("x-api-key", &key.0).bearer_auth(&key.0);
    }
    let upstream_resp = outbound.body(body_bytes).send().await?;

    let status = StatusCode::from_u16(upstream_resp.status().as_u16())?;
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop_header(name.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }

    let body = Body::from_stream(scan_and_forward(
        upstream_resp.bytes_stream(),
        Arc::clone(&state.tracker),
        label,
    ));
    Ok(builder.body(body)?)
}

struct ScanState<S> {
    inner: std::pin::Pin<Box<S>>,
    buffer: String,
    running: UsageStats,
    tracker: Arc<UsageTracker>,
    label: String,
    /// Set once the inner stream has ended or errored, so a stray extra
    /// poll (permitted, if unusual, by the `Stream` contract) doesn't
    /// re-touch a spent inner stream or double-record usage.
    ended: bool,
}

/// Wrap `inner` so that as bytes flow through unchanged to the client, any
/// usage-accounting fields visible in them (streamed SSE/NDJSON events, or
/// -- once the stream ends -- a single buffered JSON body) are recorded into
/// `tracker`. Never fails the forwarded response due to a parse miss.
fn scan_and_forward<S>(
    inner: S,
    tracker: Arc<UsageTracker>,
    label: String,
) -> impl Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
{
    let state = ScanState {
        inner: Box::pin(inner),
        buffer: String::new(),
        running: UsageStats::default(),
        tracker,
        label,
        ended: false,
    };

    stream::unfold(state, |mut st| async move {
        if st.ended {
            return None;
        }
        match st.inner.next().await {
            Some(Ok(chunk)) => {
                if let Ok(text) = std::str::from_utf8(&chunk) {
                    st.buffer.push_str(text);
                }
                scan_buffered_lines(&mut st.buffer, &mut st.running);
                Some((Ok(chunk), st))
            }
            Some(Err(e)) => {
                st.ended = true;
                Some((Err(std::io::Error::other(e)), st))
            }
            None => {
                finalize_leftover(&st.buffer, &mut st.running);
                st.tracker.record(&st.label, st.running);
                None
            }
        }
    })
}

/// Drain every complete line out of `buffer`, feeding each to `scan_line`.
/// Any trailing partial line is left in `buffer` for the next chunk.
fn scan_buffered_lines(buffer: &mut String, running: &mut UsageStats) {
    while let Some(idx) = buffer.find('\n') {
        let line = buffer[..idx].trim_end_matches('\r').to_string();
        buffer.drain(..=idx);
        scan_line(&line, running);
    }
}

/// Recognize one streaming-framing line: an SSE `data:` payload (Anthropic /
/// OpenAI) or a raw NDJSON object (Ollama). Both shapes are attempted by
/// `usage::parse_usage`, so no `ApiType` is needed to pick between them.
fn scan_line(line: &str, running: &mut UsageStats) {
    let trimmed = line.trim();
    let json_str = if let Some(rest) = trimmed.strip_prefix("data:") {
        rest.trim()
    } else if trimmed.starts_with('{') {
        trimmed
    } else {
        return;
    };
    if json_str.is_empty() || json_str == "[DONE]" {
        return;
    }
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(json_str)
        && let Some(delta) = usage::parse_usage(&json)
    {
        running.merge_max(&delta);
    }
}

/// Whatever is left in `buffer` once the response body is exhausted covers
/// the non-streaming case: the entire body is one JSON document.
fn finalize_leftover(buffer: &str, running: &mut UsageStats) {
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return;
    }
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(delta) = usage::parse_usage(&json)
    {
        running.merge_max(&delta);
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use std::net::SocketAddr;

    async fn echo_model_and_auth(headers: HeaderMap, body: axum::body::Bytes) -> Response {
        let model = model_from_body(&body).unwrap_or_default();
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let api_key = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        axum::Json(
            serde_json::json!({ "model": model, "authorization": auth, "x_api_key": api_key }),
        )
        .into_response()
    }

    async fn echo_usage(body: axum::body::Bytes) -> Response {
        let model = model_from_body(&body).unwrap_or_default();
        axum::Json(serde_json::json!({
            "model": model,
            "usage": { "input_tokens": 3, "output_tokens": 5 }
        }))
        .into_response()
    }

    async fn spawn_echo_server() -> SocketAddr {
        let app = Router::new()
            .route("/v1/messages", post(echo_model_and_auth))
            .route("/v1/usage", post(echo_usage));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    fn target(base_url: String, auth: UpstreamAuth) -> UpstreamTarget {
        UpstreamTarget {
            base_url,
            verify_ssl: true,
            auth,
        }
    }

    #[tokio::test]
    async fn dispatches_known_model_to_its_own_route_with_injected_auth() {
        let known_addr = spawn_echo_server().await;
        let default_addr = spawn_echo_server().await;

        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();
        server
            .handle
            .register_route(
                "granite-4.1-8b".to_string(),
                target(
                    format!("http://{known_addr}"),
                    UpstreamAuth::Inject(Some(Secret("known-key".to_string()))),
                ),
                "sub-agent".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .header("authorization", "Bearer client-token")
            .json(&serde_json::json!({ "model": "granite-4.1-8b" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "granite-4.1-8b");
        // Client auth was stripped and replaced with the route's own key.
        assert_eq!(body["authorization"], "Bearer known-key");
        assert_eq!(body["x_api_key"], "known-key");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn falls_back_to_default_for_unknown_model() {
        let default_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "claude-sonnet-5" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "claude-sonnet-5");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn passthrough_default_leaves_client_auth_headers_untouched() {
        let default_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .header("authorization", "Bearer subscription-session-token")
            .json(&serde_json::json!({ "model": "claude-sonnet-5" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["authorization"], "Bearer subscription-session-token");
        assert_eq!(body["x_api_key"], "");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn falls_back_to_default_for_non_json_body() {
        let default_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        server.shutdown().await;
    }

    #[tokio::test]
    async fn register_route_after_boot_is_immediately_live() {
        let default_addr = spawn_echo_server().await;
        let known_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        // Before registration, this model falls through to default.
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "granite-4.1-8b" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // Register the route while the server is already accepting traffic.
        server
            .handle
            .register_route(
                "granite-4.1-8b".to_string(),
                target(
                    format!("http://{known_addr}"),
                    UpstreamAuth::Inject(Some(Secret("known-key".to_string()))),
                ),
                "sub-agent".to_string(),
            )
            .unwrap();

        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "granite-4.1-8b" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["x_api_key"], "known-key");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn set_default_after_boot_overrides_routing_for_unmatched_models() {
        let real_addr = spawn_echo_server().await;
        let overridden_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        // The overridden default won't be used once we call set_default.
        server
            .handle
            .set_default(
                target(
                    format!("http://{overridden_addr}"),
                    UpstreamAuth::Passthrough,
                ),
                "default".to_string(),
            )
            .unwrap();
        server
            .handle
            .set_default(
                target(
                    format!("http://{real_addr}"),
                    UpstreamAuth::Inject(Some(Secret("main-key".to_string()))),
                ),
                "main".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "some-internal-model" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["x_api_key"], "main-key");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn set_default_from_route_aliases_default_to_an_already_registered_route() {
        let main_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .register_route(
                "main-model".to_string(),
                target(
                    format!("http://{main_addr}"),
                    UpstreamAuth::Inject(Some(Secret("main-key".to_string()))),
                ),
                "main".to_string(),
            )
            .unwrap();

        server.handle.set_default_from_route("main-model").unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "some-unregistered-internal-model" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["x_api_key"], "main-key");

        server.shutdown().await;
    }

    #[tokio::test]
    async fn set_default_from_route_fails_for_an_unregistered_model_name() {
        let server = ProxyServer::start().unwrap();
        assert!(
            server
                .handle
                .set_default_from_route("no-such-model")
                .is_err()
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn usage_is_tracked_for_both_routed_and_default_traffic() {
        let default_addr = spawn_echo_server().await;
        let routed_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();
        server
            .handle
            .register_route(
                "granite-4.1-8b".to_string(),
                target(format!("http://{routed_addr}"), UpstreamAuth::Passthrough),
                "reviewer".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "claude-sonnet-5" }))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "granite-4.1-8b" }))
            .send()
            .await
            .unwrap();

        let snapshot = server.handle.tracker().snapshot();
        // Default traffic is labeled by its own observed model name, not the
        // generic default label.
        assert_eq!(snapshot.get("claude-sonnet-5").unwrap().input_tokens, 3);
        assert_eq!(snapshot.get("reviewer").unwrap().input_tokens, 3);
        assert!(!snapshot.contains_key("default"));

        server.shutdown().await;
    }

    #[tokio::test]
    async fn default_traffic_naming_several_upstream_models_is_tracked_per_model_name() {
        let default_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        for model in ["claude-sonnet-5", "claude-haiku-4-5", "claude-sonnet-5"] {
            client
                .post(format!("{}/v1/usage", server.handle.local_base_url))
                .json(&serde_json::json!({ "model": model }))
                .send()
                .await
                .unwrap();
        }

        let snapshot = server.handle.tracker().snapshot();
        let sonnet = snapshot.get("claude-sonnet-5").unwrap();
        assert_eq!(sonnet.requests, 2);
        assert_eq!(sonnet.input_tokens, 6);
        let haiku = snapshot.get("claude-haiku-4-5").unwrap();
        assert_eq!(haiku.requests, 1);
        assert_eq!(haiku.input_tokens, 3);
        assert!(!snapshot.contains_key("default"));

        server.shutdown().await;
    }

    #[tokio::test]
    async fn default_traffic_without_an_identifiable_model_name_falls_back_to_default_label() {
        let default_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();
        client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .body("not json")
            .send()
            .await
            .unwrap();

        let snapshot = server.handle.tracker().snapshot();
        assert!(snapshot.contains_key("default"));

        server.shutdown().await;
    }

    #[test]
    fn model_from_body_reads_top_level_model_field() {
        assert_eq!(
            model_from_body(br#"{"model": "granite-4.1-8b", "messages": []}"#),
            Some("granite-4.1-8b".to_string())
        );
    }

    #[test]
    fn model_from_body_returns_none_for_missing_field() {
        assert_eq!(model_from_body(br#"{"messages": []}"#), None);
    }

    #[test]
    fn model_from_body_returns_none_for_non_json() {
        assert_eq!(model_from_body(b"not json"), None);
    }

    #[test]
    fn hop_by_hop_headers_are_filtered_but_auth_headers_are_not() {
        assert!(is_hop_by_hop_header("Host"));
        assert!(is_hop_by_hop_header("Transfer-Encoding"));
        assert!(!is_hop_by_hop_header("Authorization"));
        assert!(!is_hop_by_hop_header("Content-Type"));
        assert!(is_auth_header("X-Api-Key"));
        assert!(is_auth_header("Authorization"));
        assert!(!is_auth_header("Content-Type"));
    }

    #[test]
    fn scan_line_anthropic_sse_data_event() {
        let mut running = UsageStats::default();
        scan_line(
            r#"data: {"type":"message_delta","usage":{"output_tokens":7}}"#,
            &mut running,
        );
        assert_eq!(running.output_tokens, 7);
    }

    #[test]
    fn scan_line_ignores_done_sentinel() {
        let mut running = UsageStats::default();
        scan_line("data: [DONE]", &mut running);
        assert_eq!(running, UsageStats::default());
    }

    #[test]
    fn scan_line_ollama_ndjson_line() {
        let mut running = UsageStats::default();
        scan_line(
            r#"{"done":true,"prompt_eval_count":3,"eval_count":9}"#,
            &mut running,
        );
        assert_eq!(running.input_tokens, 3);
        assert_eq!(running.output_tokens, 9);
    }

    #[test]
    fn finalize_leftover_parses_full_non_streaming_body() {
        let mut running = UsageStats::default();
        finalize_leftover(
            r#"{"usage":{"input_tokens":11,"output_tokens":22}}"#,
            &mut running,
        );
        assert_eq!(running.input_tokens, 11);
        assert_eq!(running.output_tokens, 22);
    }

    #[tokio::test]
    async fn scan_and_forward_records_usage_once_stream_ends() {
        let tracker = Arc::new(UsageTracker::new());
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = vec![
            Ok(bytes::Bytes::from_static(
                b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n",
            )),
            Ok(bytes::Bytes::from_static(
                b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":9}}\n",
            )),
        ];
        let inner = stream::iter(chunks);
        let forwarded: Vec<_> = scan_and_forward(inner, Arc::clone(&tracker), "chat".to_string())
            .collect()
            .await;
        assert_eq!(forwarded.len(), 2);

        let snapshot = tracker.snapshot();
        let chat = snapshot.get("chat").unwrap();
        assert_eq!(chat.requests, 1);
        assert_eq!(chat.input_tokens, 5);
        assert_eq!(chat.output_tokens, 9);
    }

    #[tokio::test]
    async fn register_provider_registers_known_models_and_prefix_fallback() {
        let provider_addr = spawn_echo_server().await;
        let default_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();

        // Register a provider with known models and prefix fallback using "gpt" prefix
        server
            .handle
            .register_provider(
                "gpt",
                format!("http://{provider_addr}"),
                vec!["gpt-4o".to_string(), "o1".to_string()],
                "openai".to_string(),
            )
            .unwrap();

        // Known model routes to the provider
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "gpt-4o" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // Unknown model with matching prefix routes to the provider
        let resp = client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "gpt-4o-mini" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // Model without matching prefix falls through to default
        server
            .handle
            .set_default(
                target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
                "default".to_string(),
            )
            .unwrap();

        let resp = client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "claude-sonnet" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        let snapshot = server.handle.tracker().snapshot();
        assert!(snapshot.get("gpt-4o").is_some() || snapshot.get("openai").is_some());
        assert!(snapshot.get("gpt-4o-mini").is_some() || snapshot.get("openai").is_some());

        server.shutdown().await;
    }

    #[tokio::test]
    async fn provider_prefix_first_match_wins() {
        let addr1 = spawn_echo_server().await;
        let addr2 = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();

        // Register two providers with model-name prefixes: "gpt" matches OpenAI, "gemini" matches Google
        server
            .handle
            .register_provider(
                "gpt",
                format!("http://{addr1}"),
                vec!["gpt-4o".to_string()],
                "openai".to_string(),
            )
            .unwrap();
        server
            .handle
            .register_provider(
                "gemini",
                format!("http://{addr2}"),
                vec!["gemini-pro".to_string()],
                "google".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();

        // "gpt-4o" matches openai's known model
        let resp = client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "gpt-4o" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // "gpt-4" matches openai's prefix
        let resp = client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "gpt-4" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        // "gemini-pro" matches google's known model
        let resp = client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "gemini-pro" }))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        server.shutdown().await;
    }

    #[tokio::test]
    async fn proxy_prefix_routing_records_usage_for_fallback_matched_models() {
        let provider_addr = spawn_echo_server().await;
        let server = ProxyServer::start().unwrap();

        server
            .handle
            .register_provider(
                "gpt",
                format!("http://{provider_addr}"),
                vec!["gpt-4o".to_string()],
                "openai".to_string(),
            )
            .unwrap();

        let client = reqwest::Client::new();

        // Send a request for an unknown model that matches the prefix
        client
            .post(format!("{}/v1/usage", server.handle.local_base_url))
            .json(&serde_json::json!({ "model": "gpt-4o-turbo" }))
            .send()
            .await
            .unwrap();

        let snapshot = server.handle.tracker().snapshot();
        // Should be tracked under the provider label
        let openai_stats = snapshot.get("openai");
        assert!(
            openai_stats.is_some(),
            "expected 'openai' in snapshot: {:?}",
            snapshot
        );

        server.shutdown().await;
    }
}
