//! Test doubles for the factory-constructed traits, shared by the tests that
//! build them.
//!
//! Each test module used to define its own: seven copies of one
//! `FakeProvider`, and six model doubles that differed in an instance id, a
//! `ModelType`, a repository string and whether they carried variants. One
//! definition of each lives here, so a change to `ConfigConstructable` or to
//! a domain trait edits one implementation.
//!
//! [`FakeServer`] is a provider server on the loopback address, for tests
//! that probe providers or list the models they serve.

// Standard
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// Local
use crate::models::{Model, ModelArchitecture, ModelFunction, ModelType, ModelVariant};
use crate::providers::{
    ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError, PullResult,
};
use crate::registry::{ConfigConstructable, ConstructError, Named, Secret};
use crate::utils::ui::Ui;

/*-- public --*/

/// A provider server on an ephemeral port of the loopback address. It
/// answers the health check and the model listing of one provider type
/// (`ollama`, `llama-cpp` or `lm-studio`) with the models it was given, and
/// counts the requests it receives. Any other path gets 404. The server runs
/// until the test's runtime stops.
pub(crate) struct FakeServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
}

impl FakeServer {
    /// Starts a server for `provider_type` that serves `models`, by the
    /// names the type's listing reports them under. Panics for a type it has
    /// no routes for.
    pub(crate) async fn start(provider_type: &str, models: &[&str]) -> Self {
        use axum::{Json, Router, routing::get};

        let names: Vec<String> = models.iter().map(|m| m.to_string()).collect();
        let (health, listing, body) = match provider_type {
            // Ollama's health check is its listing.
            "ollama" => (
                "/api/tags",
                "/api/tags",
                serde_json::json!({
                    "models": names
                        .iter()
                        .map(|name| serde_json::json!({ "name": name, "model": name }))
                        .collect::<Vec<_>>(),
                }),
            ),
            "llama-cpp" => (
                "/health",
                "/models",
                serde_json::json!({
                    "data": names
                        .iter()
                        .map(|id| serde_json::json!({ "id": id }))
                        .collect::<Vec<_>>(),
                }),
            ),
            "lm-studio" => (
                "/v1/models",
                "/api/v1/models",
                serde_json::json!({
                    "models": names
                        .iter()
                        .map(|key| serde_json::json!({ "key": key, "type": "llm" }))
                        .collect::<Vec<_>>(),
                }),
            ),
            other => panic!("FakeServer has no routes for provider type '{other}'"),
        };

        let requests = Arc::new(AtomicUsize::new(0));
        let mut app = Router::new().route(
            listing,
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        if health != listing {
            app = app.route(
                health,
                get(|| async { Json(serde_json::json!({ "status": "ok" })) }),
            );
        }
        let counter = requests.clone();
        let app = app.layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                counter.fetch_add(1, Ordering::SeqCst);
                next.run(request)
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base_url: format!("http://{addr}"),
            requests,
        }
    }

    /// The URL the server answers on, `http://127.0.0.1:<port>`.
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The URL the server answers on, with `host` in place of `127.0.0.1`.
    /// `localhost` reaches the same server, which lets one test stand for
    /// two hosts.
    pub(crate) fn base_url_at(&self, host: &str) -> String {
        self.base_url.replace("127.0.0.1", host)
    }

    /// How many requests the server has received.
    pub(crate) fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// A loopback URL where nothing listens: the port of a listener that was
/// bound and closed, so a connection to it is refused.
pub(crate) async fn closed_base_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// A provider whose connection details and endpoint table the test states
/// outright, so a binding can be checked without a live upstream.
#[derive(Clone, Default)]
pub(crate) struct FakeProvider {
    pub(crate) instance_id: String,
    pub(crate) base_url: String,
    pub(crate) api_key: Option<Secret>,
    pub(crate) verify_ssl: bool,
    pub(crate) api_types: Vec<ApiType>,
    pub(crate) endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
    /// When set, `model_alias` returns this value instead of `None`.
    pub(crate) alias: Option<String>,
}

impl ConfigConstructable for FakeProvider {
    type Config = crate::registry::NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        unimplemented!("not used in tests")
    }
}

impl Named for FakeProvider {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait::async_trait]
impl Provider for FakeProvider {
    fn name(&self) -> &str {
        "Fake Provider"
    }
    fn function_endpoints(&self) -> HashMap<ModelFunction, Vec<ApiEndpoint>> {
        self.endpoints.clone()
    }
    fn supported_api_types(&self) -> Vec<ApiType> {
        self.api_types.clone()
    }
    fn base_url(&self) -> &str {
        &self.base_url
    }
    fn api_key(&self) -> Option<&Secret> {
        self.api_key.as_ref()
    }
    fn verify_ssl(&self) -> bool {
        self.verify_ssl
    }
    fn supported_formats(&self) -> Vec<ModelFormat> {
        vec![]
    }
    fn can_run_model(&self, _variant_format: &str, _variant_precision: &str) -> bool {
        true
    }
    fn custom_headers(&self) -> Option<HashMap<String, Secret>> {
        None
    }
    async fn pull_model(
        &self,
        _model: &crate::models::ModelMetadata,
        _variant: &ModelVariant,
        _ui: &dyn Ui,
    ) -> Result<PullResult, ProviderError> {
        unimplemented!("not used in tests")
    }
    fn model_alias(&self, _model_id: String, _variant: Option<&ModelVariant>) -> Option<String> {
        self.alias.clone()
    }
    async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
        unimplemented!("not used in tests")
    }
}

/// A model whose catalog answers are fixed, apart from the ones a test
/// decides: which functions it supports, and which variants it reports.
pub(crate) struct FakeModel {
    instance_id: String,
    model_type: ModelType,
    huggingface_repo: String,
    context_length: u64,
    supported_functions: Vec<ModelFunction>,
    variants: Vec<ModelVariant>,
}

impl FakeModel {
    /// A text model under the catalog id `granite-3.1-8b-instruct`, so a
    /// test that configures it names a model the registry knows.
    pub(crate) fn text(supported_functions: Vec<ModelFunction>) -> Self {
        Self {
            instance_id: "granite-3.1-8b-instruct".to_string(),
            model_type: ModelType::Text,
            huggingface_repo: "test/test".to_string(),
            context_length: 4096,
            supported_functions,
            variants: Vec::new(),
        }
    }

    /// A vision model under the id `granite-vision-test`, which is what the
    /// vision-mcp bindings carry.
    pub(crate) fn vision(supported_functions: Vec<ModelFunction>) -> Self {
        Self {
            instance_id: "granite-vision-test".to_string(),
            model_type: ModelType::Vision,
            huggingface_repo: "test/test-vision".to_string(),
            context_length: 4096,
            supported_functions,
            variants: Vec::new(),
        }
    }

    /// The variants this model reports, which decide the alias a binding
    /// carries. Empty unless a test sets them.
    pub(crate) fn with_variants(mut self, variants: Vec<ModelVariant>) -> Self {
        self.variants = variants;
        self
    }

    /// The context length this model reports. 4096 unless a test sets it.
    pub(crate) fn with_context_length(mut self, context_length: u64) -> Self {
        self.context_length = context_length;
        self
    }
}

impl ConfigConstructable for FakeModel {
    type Config = crate::registry::NoConfig;

    fn new(_instance_id: &str, _cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        unimplemented!("not used in tests")
    }
}

impl Named for FakeModel {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl Model for FakeModel {
    fn family(&self) -> &str {
        "Test"
    }
    fn version(&self) -> &str {
        "1.0"
    }
    fn size(&self) -> u64 {
        1
    }
    fn context_length(&self) -> u64 {
        self.context_length
    }
    fn model_type(&self) -> &ModelType {
        &self.model_type
    }
    fn huggingface_repo(&self) -> &str {
        &self.huggingface_repo
    }
    fn native_dtype(&self) -> &str {
        "bfloat16"
    }
    fn architecture(&self) -> &ModelArchitecture {
        unimplemented!("not used in tests")
    }
    fn variants(&self) -> &[ModelVariant] {
        &self.variants
    }
    fn description(&self) -> Option<&str> {
        None
    }
    fn tags(&self) -> &[String] {
        &[]
    }
    fn supported_functions(&self) -> &[ModelFunction] {
        &self.supported_functions
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::PROVIDER_REGISTRY;

    /// A provider of `provider_type` with its default configuration at
    /// `base_url`.
    fn provider_at(provider_type: &str, base_url: &str) -> Box<dyn Provider> {
        let mut config = PROVIDER_REGISTRY.default_config(provider_type);
        config["base_url"] = base_url.into();
        PROVIDER_REGISTRY
            .construct(provider_type, provider_type, &config)
            .unwrap()
    }

    #[tokio::test]
    async fn each_type_answers_its_health_check_and_lists_its_models() {
        for provider_type in ["ollama", "llama-cpp", "lm-studio"] {
            let server = FakeServer::start(provider_type, &["m1", "m2"]).await;
            let provider = provider_at(provider_type, server.base_url());

            let health = provider.health_check().await.unwrap();
            let listed: Vec<String> = provider
                .list_models()
                .await
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect();

            assert!(health.healthy, "{provider_type}: {:?}", health.error);
            assert_eq!(listed, vec!["m1", "m2"], "{provider_type}");
        }
    }

    #[tokio::test]
    async fn requests_are_counted() {
        let server = FakeServer::start("llama-cpp", &[]).await;
        let provider = provider_at("llama-cpp", server.base_url());
        assert_eq!(server.requests(), 0);

        provider.health_check().await.unwrap();
        provider.list_models().await.unwrap();

        assert_eq!(server.requests(), 2);
    }

    #[tokio::test]
    async fn localhost_reaches_the_same_server() {
        let server = FakeServer::start("ollama", &["m1"]).await;
        let provider = provider_at("ollama", &server.base_url_at("localhost"));

        assert!(provider.health_check().await.unwrap().healthy);
        assert_eq!(server.requests(), 1);
    }

    #[tokio::test]
    async fn a_closed_port_refuses_the_connection() {
        let provider = provider_at("ollama", &closed_base_url().await);

        let health = provider.health_check().await.unwrap();

        assert!(!health.healthy);
        assert!(
            health
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("Connection failed")),
            "{:?}",
            health.error
        );
    }
}
