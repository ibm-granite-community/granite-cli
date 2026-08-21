//! A localhost reverse-proxy that dispatches each request to one of several
//! upstream targets based on the top-level `"model"` field in its JSON body
//! -- a known model routes to its own resolved provider; anything else
//! (parse failure, missing field, no match) falls through to a `default`
//! target unchanged. First consumer: `ClaudeLauncher`, which uses this to
//! give each bound `SubAgentCapability` its own model/provider while Claude
//! Code's single `ANTHROPIC_BASE_URL` still carries the main session's own
//! traffic. See `docs/specs/0021-sub-agent-capability.md`.
//!
//! Deliberately not Anthropic-specific in its mechanics: it only reads a
//! top-level JSON `"model"` string, a shape OpenAI/Ollama-style request
//! bodies share too.

// Standard
use std::collections::HashMap;
use std::sync::Arc;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

// Local
use crate::registry::Secret;
use crate::utils::subserver::SubServer;

use_channel!("MRTR");

/*-- public --*/

/// One destination the router can forward a request to.
pub struct UpstreamTarget {
    pub base_url: String,
    pub verify_ssl: bool,
    pub auth: UpstreamAuth,
}

/// How the router should handle auth headers when forwarding to a target.
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

/// A running model-based router. Bind an ephemeral localhost port and start
/// dispatching immediately; `local_base_url` is what a launcher points its
/// tool's base-URL env var at.
pub struct ModelRouter {
    pub local_base_url: String,
    inner: SubServer,
}

impl ModelRouter {
    /// Synchronous -- see `SubServer::spawn` -- so this can be called from
    /// inside sync code as long as a Tokio runtime is already running
    /// somewhere up the call stack.
    pub fn start(
        default: UpstreamTarget,
        routes: HashMap<String, UpstreamTarget>,
    ) -> anyhow::Result<Self> {
        let default = ResolvedTarget::build(default)?;
        let routes = routes
            .into_iter()
            .map(|(model, target)| Ok((model, ResolvedTarget::build(target)?)))
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let state = Arc::new(RouterState { default, routes });

        let app = Router::new()
            .fallback(any(router_handler))
            .with_state(state);
        let inner = SubServer::spawn(app, "sub-agent model router")?;
        let local_base_url = format!("http://{}", inner.local_addr);

        Ok(Self {
            local_base_url,
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

struct RouterState {
    default: ResolvedTarget,
    routes: HashMap<String, ResolvedTarget>,
}

impl RouterState {
    fn target_for(&self, body: &[u8]) -> &ResolvedTarget {
        model_from_body(body)
            .and_then(|model| self.routes.get(&model))
            .unwrap_or(&self.default)
    }
}

/// Reads the top-level `"model"` string out of a JSON request body. A
/// non-JSON body, or one without a string `"model"` field, yields `None` --
/// not an error -- so the caller falls through to the default target.
fn model_from_body(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    value.get("model")?.as_str().map(str::to_string)
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

async fn router_handler(
    State(state): State<Arc<RouterState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match forward(&state, method, uri, headers, body).await {
        Ok(response) => response,
        Err(e) => {
            alog_channel!(
                MessageLevel::Warning,
                "sub-agent model router forward failed: {e}"
            );
            (StatusCode::BAD_GATEWAY, format!("router error: {e}")).into_response()
        }
    }
}

async fn forward(
    state: &RouterState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> anyhow::Result<Response> {
    let body_bytes = axum::body::to_bytes(body, usize::MAX).await?;
    let target = state.target_for(&body_bytes);

    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!(
        "{}{}",
        target.base_url.trim_end_matches('/'),
        path_and_query
    );

    let outbound_method = reqwest::Method::from_bytes(method.as_str().as_bytes())?;
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
        // upstream only reads the one it understands. Mirrors
        // `proxy::server::forward`'s existing convention.
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

    let body = Body::from_stream(upstream_resp.bytes_stream());
    Ok(builder.body(body)?)
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

    async fn spawn_echo_server() -> SocketAddr {
        let app = Router::new().route("/v1/messages", post(echo_model_and_auth));
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

        let mut routes = HashMap::new();
        routes.insert(
            "granite-4.1-8b".to_string(),
            target(
                format!("http://{known_addr}"),
                UpstreamAuth::Inject(Some(Secret("known-key".to_string()))),
            ),
        );
        let router = ModelRouter::start(
            target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
            routes,
        )
        .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", router.local_base_url))
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

        router.shutdown().await;
    }

    #[tokio::test]
    async fn falls_back_to_default_for_unknown_model() {
        let default_addr = spawn_echo_server().await;
        let router = ModelRouter::start(
            target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
            HashMap::new(),
        )
        .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", router.local_base_url))
            .json(&serde_json::json!({ "model": "claude-sonnet-5" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "claude-sonnet-5");

        router.shutdown().await;
    }

    #[tokio::test]
    async fn passthrough_default_leaves_client_auth_headers_untouched() {
        let default_addr = spawn_echo_server().await;
        let router = ModelRouter::start(
            target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
            HashMap::new(),
        )
        .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", router.local_base_url))
            .header("authorization", "Bearer subscription-session-token")
            .json(&serde_json::json!({ "model": "claude-sonnet-5" }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["authorization"], "Bearer subscription-session-token");
        assert_eq!(body["x_api_key"], "");

        router.shutdown().await;
    }

    #[tokio::test]
    async fn falls_back_to_default_for_non_json_body() {
        let default_addr = spawn_echo_server().await;
        let router = ModelRouter::start(
            target(format!("http://{default_addr}"), UpstreamAuth::Passthrough),
            HashMap::new(),
        )
        .unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/v1/messages", router.local_base_url))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());

        router.shutdown().await;
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
}
