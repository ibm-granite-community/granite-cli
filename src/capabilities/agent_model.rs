//! The first concrete `Capability`: surfaces a configured model's connection
//! details (base URL, model name, auth, TLS) so a launcher can bind them into
//! an agent's environment.

use crate::capabilities::base::{
    AgentModelBinding, AgentModelBindingRequest, Binding, BindingRequest, BindingType, Capability,
    CapabilityMetadata, Dependency, HasCapabilityMetadata,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::dependency::Configured;
use crate::models::{Model, ModelFunction};
use crate::registry::ConfigConstructable;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/*-- AgentModelCapabilityConfig ---------------------------------------------------*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct AgentModelCapabilityConfig {
    #[serde(default)]
    pub model_id: String,
    /// Which model function to bind (defaults to `Chat` if unset).
    #[serde(default)]
    pub function: Option<ModelFunction>,
}

/*-- AgentModelCapability ---------------------------------------------------------*/

pub struct AgentModelCapability {
    config: AgentModelCapabilityConfig,
}

impl AgentModelCapability {
    fn function(&self) -> ModelFunction {
        self.config.function.clone().unwrap_or(ModelFunction::Chat)
    }
}

impl ConfigConstructable for AgentModelCapability {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: AgentModelCapabilityConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self { config }
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
        let function = self.function();
        vec![Dependency::Model {
            config_key: "model_id".to_string(),
            requirement: ModelRequirement {
                supported_functions: vec![function],
                ..Default::default()
            },
            resolved_id: Some(self.config.model_id.clone()),
            required: true,
        }]
    }

    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::AgentModel])
    }

    async fn bind(
        &self,
        request: BindingRequest,
        models: &(dyn Configured<dyn Model> + Sync),
    ) -> anyhow::Result<Binding> {
        let BindingRequest::AgentModel(AgentModelBindingRequest { api_type }) = request;
        let model_id = self.config.model_id.clone();

        let (_, model) = models
            .instances()
            .into_iter()
            .find(|(id, _)| id == &model_id)
            .ok_or_else(|| anyhow::anyhow!("model '{model_id}' not configured"))?;

        let provider = model
            .provider()
            .map_err(|e| anyhow::anyhow!("model '{model_id}' has no usable provider: {e}"))?;

        // Primary check -- which ApiType a launcher wants is only known here.
        anyhow::ensure!(
            provider.supported_api_types().contains(&api_type),
            "provider for model '{model_id}' does not support {api_type}"
        );
        let function = self.function();
        // Defensive only: setup-time resolution already guarantees these.
        anyhow::ensure!(
            model.supported_functions().contains(&function),
            "model '{model_id}' does not support {function}"
        );
        anyhow::ensure!(
            provider.supports_function(&function),
            "provider for model '{model_id}' does not support {function}"
        );

        let endpoint = provider
            .endpoints_for_function(&function)
            .into_iter()
            .find(|e| e.api_type() == api_type)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider for model '{model_id}' has no {api_type} endpoint for {function}"
                )
            })?;

        Ok(Binding::AgentModel(AgentModelBinding {
            api_type,
            base_url: provider.base_url().to_string(),
            model_name: model_id,
            endpoint_path: endpoint.path().to_string(),
            api_key: provider.api_key().cloned(),
            verify_ssl: provider.verify_ssl(),
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
                requirement: ModelRequirement::default(),
                resolved_id: None,
                required: true,
            }],
            tags: vec!["agent".to_string(), "model".to_string()],
            supported_binding_types: HashSet::from([BindingType::AgentModel]),
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(AgentModelCapabilityConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(AgentModelCapabilityConfig::default()).unwrap_or_default()
    }
}

/*-- tests -------------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError,
    };
    use crate::registry::Secret;
    use std::collections::HashMap;

    #[derive(Clone)]
    struct FakeProvider {
        base_url: String,
        api_key: Option<Secret>,
        verify_ssl: bool,
        api_types: Vec<ApiType>,
        endpoints: HashMap<ModelFunction, Vec<ApiEndpoint>>,
    }

    impl ConfigConstructable for FakeProvider {
        fn new(_cfg: &serde_json::Value) -> Self {
            unimplemented!("not used in tests")
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
        async fn health_check(&self) -> Result<HealthStatus, ProviderError> {
            unimplemented!("not used in tests")
        }
    }

    struct FakeModel {
        supported_functions: Vec<ModelFunction>,
        provider_result: Result<FakeProvider, String>,
    }

    impl ConfigConstructable for FakeModel {
        fn new(_cfg: &serde_json::Value) -> Self {
            unimplemented!("not used in tests")
        }
    }

    impl Model for FakeModel {
        fn family(&self) -> &str {
            "Fake"
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
            "fake/fake"
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
            self.provider_result
                .clone()
                .map(|p| Box::new(p) as Box<dyn Provider>)
                .map_err(|e| anyhow::anyhow!(e))
        }
    }

    struct FakeSource<T: ?Sized> {
        instances: Vec<(String, Box<T>)>,
    }

    impl Configured<dyn Model> for FakeSource<dyn Model> {
        fn instances(&self) -> Vec<(String, &(dyn Model + 'static))> {
            self.instances
                .iter()
                .map(|(id, m)| (id.clone(), m.as_ref()))
                .collect()
        }
        fn catalog(&self) -> HashMap<&'static str, crate::models::ModelMetadata> {
            HashMap::new()
        }
        fn config_schema(&self, _type_name: &str) -> Option<schemars::Schema> {
            None
        }
    }

    fn capability() -> AgentModelCapability {
        AgentModelCapability::new(&serde_json::json!({
            "model_id": "my-model",
        }))
    }

    fn models_with(model: FakeModel) -> FakeSource<dyn Model> {
        FakeSource {
            instances: vec![("my-model".to_string(), Box::new(model))],
        }
    }

    fn ok_provider(
        api_types: Vec<ApiType>,
        function: ModelFunction,
        endpoint: ApiEndpoint,
    ) -> Result<FakeProvider, String> {
        let mut endpoints = HashMap::new();
        endpoints.insert(function, vec![endpoint]);
        Ok(FakeProvider {
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types,
            endpoints,
        })
    }

    #[tokio::test]
    async fn bind_succeeds_for_matching_provider_and_model() {
        let cap = capability();
        let models = models_with(FakeModel {
            supported_functions: vec![ModelFunction::Chat],
            provider_result: ok_provider(
                vec![ApiType::OpenAI],
                ModelFunction::Chat,
                ApiEndpoint::OpenAIChat,
            ),
        });

        let binding = cap
            .bind(
                BindingRequest::AgentModel(AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                }),
                &models,
            )
            .await
            .unwrap();

        let Binding::AgentModel(binding) = binding;
        assert_eq!(binding.base_url, "http://localhost:11434");
        assert_eq!(binding.model_name, "my-model");
        assert_eq!(binding.endpoint_path, "/v1/chat/completions");
        assert_eq!(binding.api_type, ApiType::OpenAI);
        assert!(binding.verify_ssl);
    }

    #[tokio::test]
    async fn bind_fails_when_model_not_configured() {
        let cap = capability();
        let models: FakeSource<dyn Model> = FakeSource { instances: vec![] };

        let err = cap
            .bind(
                BindingRequest::AgentModel(AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                }),
                &models,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn bind_fails_when_model_has_no_provider() {
        let cap = capability();
        let models = models_with(FakeModel {
            supported_functions: vec![ModelFunction::Chat],
            provider_result: Err("no provider configured".to_string()),
        });

        let err = cap
            .bind(
                BindingRequest::AgentModel(AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                }),
                &models,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no usable provider"));
    }

    #[tokio::test]
    async fn bind_fails_when_provider_lacks_api_type() {
        let cap = capability();
        let models = models_with(FakeModel {
            supported_functions: vec![ModelFunction::Chat],
            provider_result: ok_provider(
                vec![ApiType::Ollama],
                ModelFunction::Chat,
                ApiEndpoint::OllamaChat,
            ),
        });

        let err = cap
            .bind(
                BindingRequest::AgentModel(AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                }),
                &models,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not support"));
    }

    #[tokio::test]
    async fn bind_fails_when_model_lacks_function() {
        let cap = capability();
        let models = models_with(FakeModel {
            supported_functions: vec![ModelFunction::Embeddings],
            provider_result: ok_provider(
                vec![ApiType::OpenAI],
                ModelFunction::Chat,
                ApiEndpoint::OpenAIChat,
            ),
        });

        let err = cap
            .bind(
                BindingRequest::AgentModel(AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                }),
                &models,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not support"));
    }

    #[tokio::test]
    async fn bind_fails_when_no_matching_endpoint() {
        let cap = capability();
        let models = models_with(FakeModel {
            supported_functions: vec![ModelFunction::Chat],
            provider_result: ok_provider(
                vec![ApiType::OpenAI, ApiType::Ollama],
                ModelFunction::Chat,
                ApiEndpoint::OllamaChat,
            ),
        });

        let err = cap
            .bind(
                BindingRequest::AgentModel(AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                }),
                &models,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no"));
    }

    #[test]
    fn binding_types_reports_agent_model() {
        let cap = capability();
        assert_eq!(
            cap.binding_types(),
            HashSet::from([BindingType::AgentModel])
        );
    }

    #[test]
    fn dependencies_carry_resolved_model_id() {
        let cap = capability();
        let deps = cap.dependencies();
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { resolved_id: Some(id), .. } if id == "my-model"
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
}
