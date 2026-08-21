//! `VisionMCPCapability`: exposes a vision-language model as an MCP server
//! that a launched coding agent can call as a tool (compare/analyze images).
//!
//! Like `AgentModelCapability`, this depends on a configured `Model` (which
//! in turn resolves its `Provider`) rather than taking an endpoint/api_key
//! of its own -- connection details are resolved from that dependency chain
//! at `bind()` time, the same way every other model-backed capability works.
//!
//! `bind()` always serves Streamable HTTP: a background server is started
//! in-process (via [`crate::utils::subserver::SubServer`], the same
//! mechanism the usage-tracking proxy uses) for the lifetime of this one
//! launch, and torn down in `on_shutdown`.

mod backend;
mod tools;

pub use backend::OpenAiCompatibleVlm;
pub use tools::VlmToolRegistry;

use crate::capabilities::base::{
    Binding, BindingRequest, BindingType, Capability, CapabilityMetadata, Dependency,
    HasCapabilityMetadata, LaunchContext, McpBinding, McpBindingRequest, McpTransportKind,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::models::{ConfiguredModel, ModelFunction, ModelType};
use crate::providers::ApiType;
use crate::registry::ConfigConstructable;
use crate::utils::subserver::SubServer;
use async_trait::async_trait;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde::{Deserialize, Serialize};
use serde_valid::Validate;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/*-- VisionMCPCapabilityConfig ----------------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, Validate)]
pub struct VisionMCPCapabilityConfig {
    /// Key into the configured models map (the user-chosen instance ID) for
    /// the vision-language model this capability serves.
    #[validate(min_length = 1)]
    pub model_id: String,
    /// Request timeout in seconds for calls to the model's provider.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Maximum image size (bytes) accepted from any image source (file,
    /// inline base64, or fetched URL).
    #[serde(default = "default_max_image_bytes")]
    pub max_image_bytes: u64,
    /// Extra headers sent with every request to the model's provider, on
    /// top of whatever auth the provider itself supplies.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

fn default_timeout_seconds() -> u64 {
    120
}

fn default_max_image_bytes() -> u64 {
    50 * 1024 * 1024
}

impl Default for VisionMCPCapabilityConfig {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            timeout_seconds: default_timeout_seconds(),
            max_image_bytes: default_max_image_bytes(),
            extra_headers: HashMap::new(),
        }
    }
}

/*-- VisionMCPCapability -----------------------------------------------------------*/

pub struct VisionMCPCapability {
    instance_id: String,
    config: VisionMCPCapabilityConfig,
    configured_model: ConfiguredModel,
    /// The in-process Streamable HTTP server started by `bind()`. `None`
    /// before `bind()` runs.
    http_server: Mutex<Option<SubServer>>,
}

impl ConfigConstructable for VisionMCPCapability {
    type Config = VisionMCPCapabilityConfig;

    /// Constructs the capability by resolving its model through
    /// `ConfiguredModel`, exactly like `AgentModelCapability::new` -- so
    /// `model.provider()` works at bind time and, when a usage-tracking
    /// session is active, the model is transparently tracked.
    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self {
        let config: VisionMCPCapabilityConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();
        let configured_model = ConfiguredModel::resolve(&config.model_id, global_config);
        Self {
            instance_id: instance_id.to_string(),
            config,
            configured_model,
            http_server: Mutex::new(None),
        }
    }
}

impl crate::registry::Named for VisionMCPCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Capability for VisionMCPCapability {
    fn name(&self) -> &str {
        "Vision MCP Server"
    }

    fn description(&self) -> &str {
        "Exposes a vision-language model as an MCP server (compare/analyze images) for a launched coding agent to call."
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency::Model {
            config_key: "model_id".to_string(),
            requirement: ModelRequirement {
                model_type: Some(ModelType::Vision),
                supported_functions: vec![ModelFunction::Chat, ModelFunction::ImageUnderstanding],
                ..Default::default()
            },
            resolved_id: Some(self.config.model_id.clone()),
            required: true,
        }]
    }

    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::Mcp])
    }

    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let McpBindingRequest {
            supported_transports,
        } = match request {
            BindingRequest::Mcp(request) => request,
            other => anyhow::bail!(
                "VisionMCPCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };
        anyhow::ensure!(
            supported_transports.contains(&McpTransportKind::Http),
            "no MCP transport in common with the requesting launcher (it offered {supported_transports:?}; \
             VisionMCPCapability only serves Streamable HTTP)"
        );

        let model_id = &self.config.model_id;
        // The vision backend speaks the OpenAI-compatible chat/vision
        // dialect, the one every granite-cli provider can serve -- same
        // rationale as `pi`/`opencode`'s AgentModel binding. The model must
        // support ImageUnderstanding, but the endpoint is looked up via
        // Chat, since that's the endpoint that actually serves vision
        // requests.
        let (provider, endpoint, model_name) = self.configured_model.resolve_provider_endpoint(
            model_id,
            ApiType::OpenAI,
            ModelFunction::ImageUnderstanding,
            ModelFunction::Chat,
        )?;

        let vlm = OpenAiCompatibleVlm::new(
            vlm_base_url(provider.base_url(), endpoint.path()),
            model_name,
            provider.api_key().map(|k| k.0.clone()).unwrap_or_default(),
            self.config.timeout_seconds,
            self.config.max_image_bytes,
            self.config.extra_headers.clone(),
        )?;

        let tool_registry = VlmToolRegistry::new(Arc::new(vlm));
        let service = StreamableHttpService::new(
            move || Ok(tool_registry.clone()),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        let router = axum::Router::new().route_service("/mcp", service);
        let server = SubServer::spawn(router, &format!("vision-mcp ({})", self.instance_id))?;
        let url = format!("http://{}/mcp", server.local_addr);
        *self.http_server.lock().await = Some(server);

        Ok(Binding::Mcp(McpBinding::Http {
            url,
            headers: HashMap::new(),
        }))
    }

    async fn on_shutdown(&self, _context: &LaunchContext) -> anyhow::Result<()> {
        if let Some(server) = self.http_server.lock().await.take() {
            server.shutdown().await;
        }
        Ok(())
    }
}

impl HasCapabilityMetadata for VisionMCPCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: "Vision MCP Server".to_string(),
            description: "Exposes a vision-language model as an MCP server (compare/analyze images) for a launched coding agent to call.".to_string(),
            dependencies: vec![Dependency::Model {
                config_key: "model_id".to_string(),
                requirement: ModelRequirement {
                    model_type: Some(ModelType::Vision),
                    supported_functions: vec![ModelFunction::Chat, ModelFunction::ImageUnderstanding],
                    ..Default::default()
                },
                resolved_id: None,
                required: true,
            }],
            tags: vec!["vision".to_string(), "mcp".to_string()],
            supported_binding_types: HashSet::from([BindingType::Mcp]),
        }
    }
}

/// The vision backend's REST client appends its own operation path (e.g.
/// `chat/completions`) to whatever root it's given, so the trailing
/// operation is dropped from the endpoint path here -- same convention as
/// `pi`/`opencode`'s own `*_base_url` helpers.
fn vlm_base_url(base_url: &str, endpoint_path: &str) -> String {
    let root = base_url.trim_end_matches('/');
    let prefix = endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

/*-- tests -------------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig};
    use crate::models::{Model, ModelVariant};
    use crate::providers::{ApiEndpoint, HealthStatus, ModelFormat, Provider, ProviderError};
    use crate::registry::Secret;
    use std::collections::HashMap as StdHashMap;

    #[derive(Clone, Default)]
    struct FakeProvider {
        instance_id: String,
        base_url: String,
        api_key: Option<Secret>,
        verify_ssl: bool,
        api_types: Vec<ApiType>,
        endpoints: StdHashMap<ModelFunction, Vec<ApiEndpoint>>,
        alias: Option<String>,
    }

    impl ConfigConstructable for FakeProvider {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for FakeProvider {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[async_trait]
    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "Fake Provider"
        }
        fn function_endpoints(&self) -> StdHashMap<ModelFunction, Vec<ApiEndpoint>> {
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
        fn model_alias(&self, _variant: Option<&ModelVariant>) -> Option<String> {
            self.alias.clone()
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not used in tests")
        }
    }

    fn ok_provider() -> FakeProvider {
        let mut endpoints = StdHashMap::new();
        endpoints.insert(ModelFunction::Chat, vec![ApiEndpoint::OpenAIChat]);
        FakeProvider {
            instance_id: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: vec![ApiType::OpenAI],
            endpoints,
            alias: None,
        }
    }

    struct TestVisionModel {
        supported_functions: Vec<ModelFunction>,
        provider: FakeProvider,
    }

    impl ConfigConstructable for TestVisionModel {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for TestVisionModel {
        fn instance_id(&self) -> &str {
            "granite-vision-test"
        }
    }

    impl Model for TestVisionModel {
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
            4096
        }
        fn model_type(&self) -> &ModelType {
            &ModelType::Vision
        }
        fn huggingface_repo(&self) -> &str {
            "test/test-vision"
        }
        fn native_dtype(&self) -> &str {
            "bfloat16"
        }
        fn architecture(&self) -> &crate::models::ModelArchitecture {
            unimplemented!("not used in tests")
        }
        fn variants(&self) -> &[ModelVariant] {
            &[]
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
        fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
            Ok(Box::new(self.provider.clone()))
        }
    }

    /// Builds a `VisionMCPCapability` with a real registry model id (so
    /// construction succeeds) and then swaps in a test double model/provider,
    /// mirroring `agent_model.rs`'s test pattern.
    fn capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> VisionMCPCapability {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );
        let cap = VisionMCPCapability::new(
            "vision",
            &serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
            &config,
        );
        VisionMCPCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: ConfiguredModel::for_test(
                Arc::new(TestVisionModel {
                    supported_functions: functions,
                    provider,
                }),
                None,
            ),
            http_server: Mutex::new(None),
        }
    }

    fn request(transports: impl IntoIterator<Item = McpTransportKind>) -> BindingRequest {
        BindingRequest::Mcp(McpBindingRequest {
            supported_transports: transports.into_iter().collect(),
        })
    }

    fn ctx() -> LaunchContext {
        LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: std::env::temp_dir(),
            base_env: HashMap::new(),
            dry_run: false,
        }
    }

    #[test]
    fn binding_types_reports_mcp() {
        let cap =
            capability_with_test_model(vec![ModelFunction::ImageUnderstanding], ok_provider());
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::Mcp]));
    }

    #[tokio::test]
    async fn bind_starts_a_real_http_server_from_the_resolved_provider() {
        let cap =
            capability_with_test_model(vec![ModelFunction::ImageUnderstanding], ok_provider());
        let binding = cap.bind(request([McpTransportKind::Http])).await.unwrap();
        let Binding::Mcp(McpBinding::Http { url, .. }) = binding else {
            panic!("expected an Http McpBinding");
        };
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.ends_with("/mcp"));

        // A real MCP initialize handshake round-trips over Streamable HTTP.
        let client = reqwest::Client::new();
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "clientInfo": {"name": "smoke-test", "version": "0.0.0"},
                    },
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status: {}", resp.status());
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("protocolVersion"),
            "expected an initialize result, got: {body}"
        );

        cap.on_shutdown(&ctx()).await.unwrap();

        // The port is free again after shutdown.
        let addr = url.trim_start_matches("http://").trim_end_matches("/mcp");
        std::net::TcpListener::bind(addr).unwrap();
    }

    #[tokio::test]
    async fn bind_errors_when_launcher_offers_only_stdio() {
        let cap =
            capability_with_test_model(vec![ModelFunction::ImageUnderstanding], ok_provider());
        let err = cap
            .bind(request([McpTransportKind::Stdio]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no MCP transport in common"));
    }

    #[tokio::test]
    async fn bind_fails_when_model_lacks_image_understanding() {
        let cap = capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        let err = cap
            .bind(request([McpTransportKind::Http]))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not support Image Understanding")
        );
    }

    #[tokio::test]
    async fn bind_fails_when_provider_lacks_openai_api_type() {
        let cap = capability_with_test_model(
            vec![ModelFunction::ImageUnderstanding],
            FakeProvider {
                api_types: vec![ApiType::Ollama],
                ..ok_provider()
            },
        );
        let err = cap
            .bind(request([McpTransportKind::Http]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not support OpenAI"));
    }

    #[tokio::test]
    async fn bind_rejects_non_mcp_requests() {
        let cap =
            capability_with_test_model(vec![ModelFunction::ImageUnderstanding], ok_provider());
        let err = cap
            .bind(BindingRequest::AgentModel(
                crate::capabilities::base::AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                },
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not handle"));
    }

    #[tokio::test]
    async fn on_shutdown_without_a_started_server_is_a_no_op() {
        let cap =
            capability_with_test_model(vec![ModelFunction::ImageUnderstanding], ok_provider());
        cap.on_shutdown(&ctx()).await.unwrap();
    }

    #[test]
    fn vlm_base_url_drops_trailing_operation_and_keeps_version_prefix() {
        assert_eq!(
            vlm_base_url("http://localhost:11434", "/v1/chat/completions"),
            "http://localhost:11434/v1"
        );
    }

    #[test]
    fn vlm_base_url_trims_trailing_slash_from_provider_url() {
        assert_eq!(
            vlm_base_url("http://localhost:1234/", "/v1/chat/completions"),
            "http://localhost:1234/v1"
        );
    }
}
