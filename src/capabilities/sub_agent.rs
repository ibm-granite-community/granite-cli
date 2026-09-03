//! `SubAgentCapability`: defines a named sub-agent -- a prompt, a tool
//! allow-list, and a `Model`/`Provider` of its own -- that a launched coding
//! agent can delegate to independently of whichever model the main session
//! uses. See `docs/specs/0021-sub-agent-capability.md`.

use serde::{Deserialize, Serialize};
use serde_valid::Validate;

/*-- Macro: declare_sub_agent_basic -----------------------------------------------*/

/// Declares a sub-agent capability with a static prompt and static tools.
/// Config only has `description` and `model_id`.
#[macro_export]
macro_rules! declare_sub_agent_basic {
    (
        $name_struct:ident
        $config_struct:ident
        $name_cap:expr;
        $description_cap:expr;
        [$($tag:expr),* $(,)?]
        $description_expr:expr;
        $prompt_expr:expr;
        $tools_expr:expr;
        $known_type:expr
    ) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema, Validate)]
        pub struct $config_struct {
            /// Key into the configured models map (the user-chosen instance ID) for
            /// the model this sub-agent runs on.
            #[validate(min_length = 1)]
            pub model_id: String,
        }

        pub struct $name_struct {
            instance_id: String,
            config: $config_struct,
            configured_model: $crate::models::ConfiguredModel,
            /// Description shown to the parent agent for deciding when to delegate.
            pub description: String,
            /// Static prompt for this sub-agent.
            pub prompt: String,
            /// Static tool allow-list for this sub-agent.
            pub tools: Vec<$crate::capabilities::base::ToolName>,
        }

        impl $crate::registry::ConfigConstructable for $name_struct {
            type Config = $config_struct;

            fn new(
                instance_id: &str,
                cfg: &serde_json::Value,
                global_config: &$crate::config::Config,
            ) -> Self {
                let config: $config_struct =
                    serde_json::from_value(cfg.clone()).unwrap_or_default();
                let configured_model = $crate::models::ConfiguredModel::resolve(&config.model_id, global_config);
                let description = $description_expr;
                let prompt = $prompt_expr;
                let tools = $tools_expr;
                Self {
                    instance_id: instance_id.to_string(),
                    config,
                    configured_model,
                    description,
                    prompt,
                    tools,
                }
            }
        }

        impl $crate::registry::Named for $name_struct {
            fn instance_id(&self) -> &str {
                &self.instance_id
            }
        }

        #[async_trait::async_trait]
        impl $crate::capabilities::Capability for $name_struct {
            fn name(&self) -> &str {
                $name_cap
            }

            fn description(&self) -> &str {
                $description_cap
            }

            fn dependencies(&self) -> Vec<$crate::capabilities::Dependency> {
                vec![$crate::capabilities::Dependency::Model {
                    config_key: "model_id".to_string(),
                    requirement: $crate::capabilities::ModelRequirement {
                        supported_functions: vec![$crate::models::ModelFunction::Chat, $crate::models::ModelFunction::ToolCalling],
                        ..Default::default()
                    },
                    resolved_id: Some(self.config.model_id.clone()),
                    required: true,
                }]
            }

            fn binding_types(&self) -> std::collections::HashSet<$crate::capabilities::base::BindingType> {
                std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent])
            }

            async fn bind(&self, request: $crate::capabilities::base::BindingRequest) -> anyhow::Result<$crate::capabilities::base::Binding> {
                let api_type = match request {
                    $crate::capabilities::base::BindingRequest::SubAgent($crate::capabilities::base::SubAgentBindingRequest { api_type }) => api_type,
                    other => anyhow::bail!(
                        "{} does not handle {:?} binding requests",
                        stringify!($name_struct),
                        other.binding_type()
                    ),
                };
                let model_id = &self.config.model_id;
                let (provider, endpoint, model_name) = self.configured_model.resolve_provider_endpoint(
                    model_id,
                    api_type.clone(),
                    $crate::models::ModelFunction::Chat,
                    $crate::models::ModelFunction::Chat,
                )?;
                Ok($crate::capabilities::base::Binding::SubAgent($crate::capabilities::base::SubAgentBinding {
                    description: self.description.clone(),
                    prompt: self.prompt.clone(),
                    tools: self.tools.clone(),
                    model: $crate::capabilities::base::AgentModelBinding {
                        api_type,
                        provider_name: provider.instance_id().to_string(),
                        base_url: provider.base_url().to_string(),
                        model_name,
                        endpoint_path: endpoint.path().to_string(),
                        api_key: provider.api_key().cloned(),
                        verify_ssl: provider.verify_ssl(),
                        context_length: Some(self.configured_model.model.context_length()),
                    },
                    known_type: $known_type,
                }))
            }
        }

        impl $crate::capabilities::base::HasCapabilityMetadata for $name_struct {
            fn metadata() -> $crate::capabilities::base::CapabilityMetadata {
                $crate::capabilities::base::CapabilityMetadata {
                    name: $name_cap.to_string(),
                    description: $description_cap.to_string(),
                    dependencies: vec![$crate::capabilities::Dependency::Model {
                        config_key: "model_id".to_string(),
                        requirement: $crate::capabilities::ModelRequirement {
                            supported_functions: vec![$crate::models::ModelFunction::Chat, $crate::models::ModelFunction::ToolCalling],
                            ..Default::default()
                        },
                        resolved_id: None,
                        required: true,
                    }],
                    tags: vec![$($tag.to_string()),*],
                    supported_binding_types: std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent]),
                }
            }
        }
    };
}

/*-- Macro: declare_sub_agent_full ------------------------------------------------*/

/// Declares a sub-agent capability with configurable prompt and tools.
#[macro_export]
macro_rules! declare_sub_agent_full {
    (
        $name_struct:ident
        $config_struct:ident
        $name_cap:expr;
        $description_cap:expr;
        [$($tag:expr),* $(,)?]
        $known_type:expr;
        {$($config_fields:tt)*}
    ) => {
        #[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema, Validate)]
        pub struct $config_struct {
            /// Shown to the main agent so it can decide when to delegate to this
            /// sub-agent -- the same role Claude Code's own subagent `description`
            /// field plays.
            #[validate(min_length = 1)]
            pub description: String,
            /// Key into the configured models map (the user-chosen instance ID) for
            /// the model this sub-agent runs on.
            #[validate(min_length = 1)]
            pub model_id: String,
            $($config_fields)*
        }

        pub struct $name_struct {
            instance_id: String,
            config: $config_struct,
            configured_model: $crate::models::ConfiguredModel,
            /// Description shown to the parent agent for deciding when to delegate.
            pub description: String,
            /// Configurable prompt for this sub-agent.
            pub prompt: String,
            /// Configurable tool allow-list for this sub-agent.
            pub tools: Vec<$crate::capabilities::base::ToolName>,
        }

        impl $crate::registry::ConfigConstructable for $name_struct {
            type Config = $config_struct;

            fn new(
                instance_id: &str,
                cfg: &serde_json::Value,
                global_config: &$crate::config::Config,
            ) -> Self {
                let config: $config_struct =
                    serde_json::from_value(cfg.clone()).unwrap_or_default();
                let configured_model = $crate::models::ConfiguredModel::resolve(&config.model_id, global_config);
                let description = config.description.clone();
                let prompt = config.prompt.clone();
                let tools = config.tools.clone();
                Self {
                    instance_id: instance_id.to_string(),
                    config,
                    configured_model,
                    description,
                    prompt,
                    tools,
                }
            }
        }

        impl $crate::registry::Named for $name_struct {
            fn instance_id(&self) -> &str {
                &self.instance_id
            }
        }

        #[async_trait::async_trait]
        impl $crate::capabilities::Capability for $name_struct {
            fn name(&self) -> &str {
                $name_cap
            }

            fn description(&self) -> &str {
                $description_cap
            }

            fn dependencies(&self) -> Vec<$crate::capabilities::Dependency> {
                vec![$crate::capabilities::Dependency::Model {
                    config_key: "model_id".to_string(),
                    requirement: $crate::capabilities::ModelRequirement {
                        supported_functions: vec![$crate::models::ModelFunction::Chat, $crate::models::ModelFunction::ToolCalling],
                        ..Default::default()
                    },
                    resolved_id: Some(self.config.model_id.clone()),
                    required: true,
                }]
            }

            fn binding_types(&self) -> std::collections::HashSet<$crate::capabilities::base::BindingType> {
                std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent])
            }

            async fn bind(&self, request: $crate::capabilities::base::BindingRequest) -> anyhow::Result<$crate::capabilities::base::Binding> {
                let api_type = match request {
                    $crate::capabilities::base::BindingRequest::SubAgent($crate::capabilities::base::SubAgentBindingRequest { api_type }) => api_type,
                    other => anyhow::bail!(
                        "{} does not handle {:?} binding requests",
                        stringify!($name_struct),
                        other.binding_type()
                    ),
                };
                let model_id = &self.config.model_id;
                let (provider, endpoint, model_name) = self.configured_model.resolve_provider_endpoint(
                    model_id,
                    api_type.clone(),
                    $crate::models::ModelFunction::Chat,
                    $crate::models::ModelFunction::Chat,
                )?;
                Ok($crate::capabilities::base::Binding::SubAgent($crate::capabilities::base::SubAgentBinding {
                    description: self.description.clone(),
                    prompt: self.prompt.clone(),
                    tools: self.tools.clone(),
                    model: $crate::capabilities::base::AgentModelBinding {
                        api_type,
                        provider_name: provider.instance_id().to_string(),
                        base_url: provider.base_url().to_string(),
                        model_name,
                        endpoint_path: endpoint.path().to_string(),
                        api_key: provider.api_key().cloned(),
                        verify_ssl: provider.verify_ssl(),
                        context_length: Some(self.configured_model.model.context_length()),
                    },
                    known_type: $known_type,
                }))
            }
        }

        impl $crate::capabilities::base::HasCapabilityMetadata for $name_struct {
            fn metadata() -> $crate::capabilities::base::CapabilityMetadata {
                $crate::capabilities::base::CapabilityMetadata {
                    name: $name_cap.to_string(),
                    description: $description_cap.to_string(),
                    dependencies: vec![$crate::capabilities::Dependency::Model {
                        config_key: "model_id".to_string(),
                        requirement: $crate::capabilities::ModelRequirement {
                            supported_functions: vec![$crate::models::ModelFunction::Chat, $crate::models::ModelFunction::ToolCalling],
                            ..Default::default()
                        },
                        resolved_id: None,
                        required: true,
                    }],
                    tags: vec![$($tag.to_string()),*],
                    supported_binding_types: std::collections::HashSet::from([$crate::capabilities::base::BindingType::SubAgent]),
                }
            }
        }
    };
}

/*-- SubAgentCapability ------------------------------------------------------------*/

// Configuration for the generic sub-agent capability. The prompt and tools
// are configurable via JSON, leaving only description and model_id.
declare_sub_agent_full!(
    SubAgentCapability
    SubAgentCapabilityConfig
    "Sub-Agent";
    "Defines a named sub-agent (prompt, tool allow-list, and model) that a launched coding agent can delegate to.";
    ["agent", "sub-agent"]
    None;
    {
        /// The sub-agent's system prompt.
        #[validate(min_length = 1)]
        pub prompt: String,
        /// Tool allow-list. Empty (the default) means "inherit all tools."
        #[serde(default)]
        pub tools: Vec<crate::capabilities::base::ToolName>,
    }
);

/*-- tests -------------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::base::{
        Binding, BindingRequest, BindingType, Capability, Dependency, HasCapabilityMetadata,
        SubAgentBindingRequest, ToolName,
    };
    use crate::config::{Config, ModelConfig};
    use crate::models::{Model, ModelFunction};
    use crate::providers::{
        ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError,
    };
    use crate::registry::{ConfigConstructable, Secret};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::collections::HashSet;
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

    fn capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> SubAgentCapability {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
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
            description: cap.description,
            prompt: cap.prompt,
            tools: cap.tools,
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
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
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
            description: cap.description,
            prompt: cap.prompt,
            tools: cap.tools,
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
