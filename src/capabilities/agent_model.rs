//! The first concrete `Capability`: surfaces a configured model's connection
//! details (base URL, model name, auth, TLS) so a launcher can bind them into
//! an agent's environment.

use crate::capabilities::base::{
    AgentModelBinding, AgentModelBindingRequest, Binding, BindingRequest, BindingType, Capability,
    CapabilityMetadata, Dependency, HasCapabilityMetadata,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::models::{ConfiguredModel, ModelFunction};
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_valid::Validate;
use std::collections::HashSet;

/*-- AgentModelCapabilityConfig ---------------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema, Validate)]
pub struct AgentModelCapabilityConfig {
    /// Key into the configured models map (the user-chosen instance ID).
    #[validate(min_length = 1)]
    pub model_id: String,
}

/*-- AgentModelCapability ---------------------------------------------------------*/

pub struct AgentModelCapability {
    instance_id: String,
    config: AgentModelCapabilityConfig,
    configured_model: ConfiguredModel,
}

impl ConfigConstructable for AgentModelCapability {
    type Config = AgentModelCapabilityConfig;

    /// Constructs the capability by resolving its model through
    /// `ConfiguredModel`, which handles provider resolution (so
    /// `model.provider()` works at bind time) and, when a usage-tracking
    /// session is active, transparently wraps the model in a local tracking
    /// proxy.
    ///
    /// `cfg` contains the capability's instance config (e.g. `{"model_id": "my-model"}`)
    /// where `model_id` is the key into `global_config.models`. The resolved
    /// `ModelConfig` supplies the catalog model ID and the provider ID.
    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self {
        let config: AgentModelCapabilityConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();
        let configured_model = ConfiguredModel::resolve(&config.model_id, global_config);
        Self {
            instance_id: instance_id.to_string(),
            config,
            configured_model,
        }
    }
}

impl crate::registry::Named for AgentModelCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl AgentModelCapability {
    pub fn configured_model_id(&self) -> &str {
        &self.config.model_id
    }
}

#[async_trait]
impl Capability for AgentModelCapability {
    fn name(&self) -> &str {
        "Agent Model Binding"
    }

    fn description(&self) -> &str {
        "Surfaces a configured model's connection details (base URL, model name, auth, TLS) to a launched agent."
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
        HashSet::from([BindingType::AgentModel])
    }

    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let api_type = match request {
            BindingRequest::AgentModel(AgentModelBindingRequest { api_type }) => api_type,
            #[allow(unreachable_patterns)] // Will remove once more variants are available
            other => anyhow::bail!(
                "AgentModelCapability does not handle {:?} binding requests",
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

        Ok(Binding::AgentModel(AgentModelBinding {
            api_type,
            provider_name: provider.instance_id().to_string(),
            base_url: provider.base_url().to_string(),
            model_name,
            endpoint_path: endpoint.path().to_string(),
            api_key: provider.api_key().cloned(),
            verify_ssl: provider.verify_ssl(),
            context_length: Some(self.configured_model.model.context_length()),
        }))
    }
}

impl HasCapabilityMetadata for AgentModelCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: "Agent Model Binding".to_string(),
            description: "Surfaces a configured model's connection details (base URL, model name, auth, TLS) to a launched agent.".to_string(),
            dependencies: vec![Dependency::Model {
                config_key: "model_id".to_string(),
                requirement: ModelRequirement {
                    supported_functions: vec![ModelFunction::Chat, ModelFunction::ToolCalling],
                    ..Default::default()
                },
                resolved_id: None,
                required: true,
            }],
            tags: vec!["agent".to_string(), "model".to_string()],
            supported_binding_types: HashSet::from([BindingType::AgentModel]),
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

        fn new(
            _instance_id: &str,
            _cfg: &serde_json::Value,
            _global_config: &crate::config::Config,
        ) -> Self {
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

    fn ok_provider(
        api_types: Vec<ApiType>,
        function: ModelFunction,
        endpoint: ApiEndpoint,
    ) -> FakeProvider {
        let mut endpoints = HashMap::new();
        endpoints.insert(function, vec![endpoint]);
        FakeProvider {
            instance_id: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types,
            endpoints,
            alias: None,
        }
    }

    /// Create an AgentModelCapability with a custom test model and provider.
    /// Uses a real registry model ID so the model can be looked up in global config.
    fn capability_with_test_model(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
    ) -> AgentModelCapability {
        capability_with_test_model_and_variant(functions, provider, None)
    }

    /// Like `capability_with_test_model` but also sets `configured_variant`
    /// and populates the test model's variants list, enabling `resolve_variant`
    /// and `model_alias` to be exercised at bind time.
    fn capability_with_test_model_and_variant(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
        configured_variant: Option<(&str, Vec<crate::models::ModelVariant>)>,
    ) -> AgentModelCapability {
        let (variant_str, variants) = configured_variant
            .map(|(s, v)| (Some(s.to_string()), v))
            .unwrap_or((None, vec![]));
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: variant_str.clone(),
            },
        );
        let cap = AgentModelCapability::new(
            "my-agent",
            &serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
            &config,
        );
        // Replace the real model with our test double that has a custom provider
        // and the specified variants list.
        AgentModelCapability {
            instance_id: cap.instance_id,
            config: cap.config,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(TestModelWithVariants {
                    supported_functions: functions,
                    provider,
                    variants,
                }),
                variant_str,
            ),
        }
    }

    /// Extended test model that carries a mutable variants list.
    struct TestModelWithVariants {
        supported_functions: Vec<ModelFunction>,
        provider: FakeProvider,
        variants: Vec<crate::models::ModelVariant>,
    }

    impl ConfigConstructable for TestModelWithVariants {
        type Config = crate::registry::NoConfig;
        fn new(
            _instance_id: &str,
            _cfg: &serde_json::Value,
            _global_config: &crate::config::Config,
        ) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl crate::registry::Named for TestModelWithVariants {
        fn instance_id(&self) -> &str {
            "granite-3.1-8b-instruct"
        }
    }

    impl Model for TestModelWithVariants {
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
        fn provider(&self) -> anyhow::Result<Box<dyn Provider>> {
            Ok(Box::new(self.provider.clone()))
        }
    }

    #[tokio::test]
    async fn bind_succeeds_for_matching_provider_and_model() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Chat],
            ok_provider(
                vec![ApiType::OpenAI],
                ModelFunction::Chat,
                ApiEndpoint::OpenAIChat,
            ),
        );

        let binding = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap();

        let Binding::AgentModel(binding) = binding else {
            panic!("expected AgentModel binding")
        };
        assert_eq!(binding.base_url, "http://localhost:11434");
        assert_eq!(binding.model_name, "granite-3.1-8b-instruct");
        assert_eq!(binding.endpoint_path, "/v1/chat/completions");
        assert_eq!(binding.api_type, ApiType::OpenAI);
        assert!(binding.verify_ssl);
    }

    #[tokio::test]
    async fn bind_fails_when_provider_has_no_endpoints_for_function() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Chat],
            FakeProvider {
                instance_id: "my-ollama".to_string(),
                base_url: "http://localhost:11434".to_string(),
                api_key: None,
                verify_ssl: true,
                api_types: vec![ApiType::OpenAI],
                endpoints: HashMap::new(),
                alias: None,
            },
        );

        let err = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("has no OpenAI endpoint for Chat"));
    }

    #[tokio::test]
    async fn bind_fails_when_provider_lacks_api_type() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Chat],
            ok_provider(
                vec![ApiType::Ollama],
                ModelFunction::Chat,
                ApiEndpoint::OllamaChat,
            ),
        );

        let err = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not support"));
    }

    #[tokio::test]
    async fn bind_fails_when_model_lacks_function() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Embeddings],
            ok_provider(
                vec![ApiType::OpenAI],
                ModelFunction::Chat,
                ApiEndpoint::OpenAIChat,
            ),
        );

        let err = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not support"));
    }

    #[tokio::test]
    async fn bind_fails_when_no_matching_endpoint() {
        let cap = capability_with_test_model(
            vec![ModelFunction::Chat],
            ok_provider(
                vec![ApiType::OpenAI, ApiType::Ollama],
                ModelFunction::Chat,
                ApiEndpoint::OllamaChat,
            ),
        );

        let err = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no OpenAI endpoint for Chat"));
    }

    #[test]
    fn binding_types_reports_agent_model() {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );
        let cap = AgentModelCapability::new(
            "my-agent",
            &serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
            &config,
        );
        assert_eq!(
            cap.binding_types(),
            HashSet::from([BindingType::AgentModel])
        );
    }

    #[test]
    fn dependencies_carry_resolved_model_id() {
        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );
        let cap = AgentModelCapability::new(
            "my-agent",
            &serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
            &config,
        );
        let deps = cap.dependencies();
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { resolved_id: Some(id), .. } if id == "granite-3.1-8b-instruct"
        )));
    }

    #[test]
    fn metadata_reports_supported_binding_types_and_wildcard_dependency() {
        let meta = AgentModelCapability::metadata();
        assert_eq!(
            meta.supported_binding_types,
            HashSet::from([BindingType::AgentModel])
        );
        assert!(meta.dependencies.iter().any(|d| matches!(
            d,
            Dependency::Model {
                resolved_id: None,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn bind_uses_provider_alias_when_variant_matches() {
        let ollama_variant = crate::models::ModelVariant {
            format: "Ollama".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://ollama.com/library/granite4.1:8b".to_string(),
        };
        let cap = capability_with_test_model_and_variant(
            vec![ModelFunction::Chat],
            FakeProvider {
                alias: Some("granite4.1:8b".to_string()),
                ..ok_provider(
                    vec![ApiType::OpenAI],
                    ModelFunction::Chat,
                    ApiEndpoint::OpenAIChat,
                )
            },
            Some(("Ollama/Q4_K_M", vec![ollama_variant])),
        );

        let binding = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap();

        let Binding::AgentModel(binding) = binding else {
            panic!("expected AgentModel binding")
        };
        assert_eq!(binding.model_name, "granite4.1:8b");
    }

    #[tokio::test]
    async fn bind_falls_back_to_catalog_id_when_alias_is_none() {
        let ollama_variant = crate::models::ModelVariant {
            format: "Ollama".to_string(),
            precision: "Q4_K_M".to_string(),
            size_gb: Some(5.3),
            url: "https://ollama.com/library/granite4.1:8b".to_string(),
        };
        // Provider returns None for model_alias (default FakeProvider behaviour)
        let cap = capability_with_test_model_and_variant(
            vec![ModelFunction::Chat],
            ok_provider(
                vec![ApiType::OpenAI],
                ModelFunction::Chat,
                ApiEndpoint::OpenAIChat,
            ),
            Some(("Ollama/Q4_K_M", vec![ollama_variant])),
        );

        let binding = cap
            .bind(BindingRequest::AgentModel(AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            }))
            .await
            .unwrap();

        let Binding::AgentModel(binding) = binding else {
            panic!("expected AgentModel binding")
        };
        assert_eq!(binding.model_name, "granite-3.1-8b-instruct");
    }
}
