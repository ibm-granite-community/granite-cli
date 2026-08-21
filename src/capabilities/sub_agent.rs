//! `SubAgentCapability`: defines a named sub-agent -- a prompt, a tool
//! allow-list, and a `Model`/`Provider` of its own -- that a launched coding
//! agent can delegate to independently of whichever model the main session
//! uses. See `docs/specs/0021-sub-agent-capability.md`.

use crate::capabilities::base::{
    AgentModelBinding, Binding, BindingRequest, BindingType, Capability, CapabilityMetadata,
    Dependency, HasCapabilityMetadata, SubAgentBinding, SubAgentBindingRequest, ToolName,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::models::{ConfiguredModel, ModelFunction};
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_valid::Validate;
use std::collections::HashSet;

/*-- SubAgentCapabilityConfig ------------------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema, Validate)]
pub struct SubAgentCapabilityConfig {
    /// Shown to the main agent so it can decide when to delegate to this
    /// sub-agent -- the same role Claude Code's own subagent `description`
    /// field plays.
    #[validate(min_length = 1)]
    pub description: String,
    /// The sub-agent's system prompt.
    #[validate(min_length = 1)]
    pub prompt: String,
    /// Tool allow-list. Empty (the default) means "inherit all tools."
    #[serde(default)]
    pub tools: Vec<ToolName>,
    /// Key into the configured models map (the user-chosen instance ID) for
    /// the model this sub-agent runs on.
    #[validate(min_length = 1)]
    pub model_id: String,
}

/*-- SubAgentCapability -------------------------------------------------------------*/

pub struct SubAgentCapability {
    instance_id: String,
    config: SubAgentCapabilityConfig,
    configured_model: ConfiguredModel,
}

impl ConfigConstructable for SubAgentCapability {
    type Config = SubAgentCapabilityConfig;

    /// Constructs the capability by resolving its model through
    /// `ConfiguredModel`, exactly like `AgentModelCapability::new` -- so
    /// `model.provider()` works at bind time and, when a usage-tracking
    /// session is active, the model is transparently tracked.
    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self {
        let config: SubAgentCapabilityConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();
        let configured_model = ConfiguredModel::resolve(&config.model_id, global_config);
        Self {
            instance_id: instance_id.to_string(),
            config,
            configured_model,
        }
    }
}

impl crate::registry::Named for SubAgentCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Capability for SubAgentCapability {
    fn name(&self) -> &str {
        "Sub-Agent"
    }

    fn description(&self) -> &str {
        "Defines a named sub-agent (prompt, tool allow-list, and model) that a launched coding agent can delegate to."
    }

    fn dependencies(&self) -> Vec<Dependency> {
        vec![Dependency::Model {
            config_key: "model_id".to_string(),
            requirement: ModelRequirement {
                supported_functions: vec![ModelFunction::Chat, ModelFunction::ToolCalling],
                ..Default::default()
            },
            resolved_id: Some(self.config.model_id.clone()),
            required: true,
        }]
    }

    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::SubAgent])
    }

    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let api_type = match request {
            BindingRequest::SubAgent(SubAgentBindingRequest { api_type }) => api_type,
            other => anyhow::bail!(
                "SubAgentCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };
        let model_id = &self.config.model_id;

        let (provider, endpoint, model_name) = self.configured_model.resolve_provider_endpoint(
            model_id,
            api_type.clone(),
            ModelFunction::Chat,
            ModelFunction::Chat,
        )?;

        Ok(Binding::SubAgent(SubAgentBinding {
            description: self.config.description.clone(),
            prompt: self.config.prompt.clone(),
            tools: self.config.tools.clone(),
            model: AgentModelBinding {
                api_type,
                provider_name: provider.instance_id().to_string(),
                base_url: provider.base_url().to_string(),
                model_name,
                endpoint_path: endpoint.path().to_string(),
                api_key: provider.api_key().cloned(),
                verify_ssl: provider.verify_ssl(),
                context_length: self.configured_model.model.context_length(),
            },
        }))
    }
}

impl HasCapabilityMetadata for SubAgentCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: "Sub-Agent".to_string(),
            description: "Defines a named sub-agent (prompt, tool allow-list, and model) that a launched coding agent can delegate to.".to_string(),
            dependencies: vec![Dependency::Model {
                config_key: "model_id".to_string(),
                requirement: ModelRequirement {
                    supported_functions: vec![ModelFunction::Chat, ModelFunction::ToolCalling],
                    ..Default::default()
                },
                resolved_id: None,
                required: true,
            }],
            tags: vec!["agent".to_string(), "sub-agent".to_string()],
            supported_binding_types: HashSet::from([BindingType::SubAgent]),
        }
    }
}

/*-- tests -------------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ModelConfig};
    use crate::models::Model;
    use crate::providers::{
        ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError,
    };
    use crate::registry::Secret;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct FakeProvider {
        instance_id: String,
        base_url: String,
        api_key: Option<Secret>,
        verify_ssl: bool,
        api_types: Vec<ApiType>,
        endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
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
        fn model_alias(&self, _variant: Option<&crate::models::ModelVariant>) -> Option<String> {
            self.alias.clone()
        }
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not used in tests")
        }
    }

    fn ok_provider() -> FakeProvider {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            ModelFunction::Chat,
            vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
        );
        FakeProvider {
            instance_id: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types: vec![ApiType::OpenAI, ApiType::Anthropic],
            endpoints,
            alias: None,
        }
    }

    struct TestModel {
        supported_functions: Vec<ModelFunction>,
        provider: FakeProvider,
    }

    impl ConfigConstructable for TestModel {
        type Config = crate::registry::NoConfig;
        fn new(_: &str, _: &serde_json::Value, _: &crate::config::Config) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for TestModel {
        fn instance_id(&self) -> &str {
            "granite-3.1-8b-instruct"
        }
    }

    impl Model for TestModel {
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
        fn model_type(&self) -> &crate::models::ModelType {
            &crate::models::ModelType::Text
        }
        fn huggingface_repo(&self) -> &str {
            "test/test"
        }
        fn native_dtype(&self) -> &str {
            "bfloat16"
        }
        fn architecture(&self) -> &crate::models::ModelArchitecture {
            unimplemented!("not used in tests")
        }
        fn variants(&self) -> &[crate::models::ModelVariant] {
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

    /// Builds a `SubAgentCapability` with a real registry model id (so
    /// construction succeeds) and then swaps in a test double model/provider,
    /// mirroring `agent_model.rs`'s test pattern.
    fn capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> SubAgentCapability {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );
        let cap = SubAgentCapability::new(
            "reviewer",
            &serde_json::json!({
                "description": "Reviews code",
                "prompt": "You are a meticulous code reviewer.",
                "model_id": "granite-3.1-8b-instruct",
            }),
            &config,
        );
        SubAgentCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModel {
                    supported_functions: functions,
                    provider,
                }),
                None,
            ),
        }
    }

    fn request(api_type: ApiType) -> BindingRequest {
        BindingRequest::SubAgent(SubAgentBindingRequest { api_type })
    }

    #[tokio::test]
    async fn bind_succeeds_and_carries_description_prompt_and_tools() {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );
        let cap = SubAgentCapability::new(
            "reviewer",
            &serde_json::json!({
                "description": "Reviews code",
                "prompt": "You are a meticulous code reviewer.",
                "tools": ["FileRead", "Search", {"Other": "SomeRawClaudeTool"}],
                "model_id": "granite-3.1-8b-instruct",
            }),
            &config,
        );
        let cap = SubAgentCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModel {
                    supported_functions: vec![ModelFunction::Chat],
                    provider: ok_provider(),
                }),
                None,
            ),
        };

        let binding = cap.bind(request(ApiType::Anthropic)).await.unwrap();
        let Binding::SubAgent(binding) = binding else {
            panic!("expected SubAgent binding")
        };
        assert_eq!(binding.description, "Reviews code");
        assert_eq!(binding.prompt, "You are a meticulous code reviewer.");
        assert_eq!(
            binding.tools,
            vec![
                ToolName::FileRead,
                ToolName::Search,
                ToolName::Other("SomeRawClaudeTool".to_string()),
            ]
        );
        assert_eq!(binding.model.base_url, "http://localhost:11434");
        assert_eq!(binding.model.model_name, "granite-3.1-8b-instruct");
        assert_eq!(binding.model.api_type, ApiType::Anthropic);
    }

    #[tokio::test]
    async fn bind_fails_when_provider_lacks_requested_api_type() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Chat],
            FakeProvider {
                api_types: vec![ApiType::OpenAI],
                ..ok_provider()
            },
        );
        let err = cap
            .bind(request(ApiType::Anthropic))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not support Anthropic"));
    }

    #[tokio::test]
    async fn bind_fails_when_model_lacks_chat() {
        let cap = capability_with_test_model(vec![ModelFunction::Embeddings], ok_provider());
        let err = cap
            .bind(request(ApiType::Anthropic))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not support Chat"));
    }

    #[tokio::test]
    async fn bind_rejects_non_sub_agent_requests() {
        let cap = capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        let err = cap
            .bind(BindingRequest::AgentModel(
                crate::capabilities::base::AgentModelBindingRequest {
                    api_type: ApiType::Anthropic,
                },
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not handle"));
    }

    #[test]
    fn binding_types_reports_sub_agent() {
        let cap = capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        assert_eq!(cap.binding_types(), HashSet::from([BindingType::SubAgent]));
    }

    #[test]
    fn dependencies_carry_resolved_model_id() {
        let cap = capability_with_test_model(vec![ModelFunction::Chat], ok_provider());
        let deps = cap.dependencies();
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { resolved_id: Some(id), .. } if id == "granite-3.1-8b-instruct"
        )));
    }

    #[test]
    fn metadata_reports_supported_binding_types_and_wildcard_dependency() {
        let meta = SubAgentCapability::metadata();
        assert_eq!(
            meta.supported_binding_types,
            HashSet::from([BindingType::SubAgent])
        );
        assert!(meta.dependencies.iter().any(|d| matches!(
            d,
            Dependency::Model {
                resolved_id: None,
                ..
            }
        )));
    }
}
