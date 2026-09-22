//! The first concrete `Capability`: surfaces a configured model's connection
//! details (base URL, model name, auth, TLS) so a launcher can bind them into
//! an agent's environment.

use crate::capabilities::base::{
    AgentModelBinding, AgentModelBindingRequest, Binding, BindingRequest, BindingType, Capability,
    CapabilityMetadata, Dependency, HasCapabilityMetadata,
};
use crate::capabilities::requirement::ModelRequirement;
use crate::models::{ConfiguredModel, ModelFunction};
use crate::registry::{ConfigConstructable, ConstructError};
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
}

/// [`AgentModelCapability`] with the model its `model_id` names. Built only
/// by `resolve_refs`, so holding one is what says the name resolved and the
/// model meets what this capability's metadata requires of it.
pub struct ResolvedAgentModelCapability {
    inner: AgentModelCapability,
    configured_model: ConfiguredModel,
}

impl ConfigConstructable for AgentModelCapability {
    type Config = AgentModelCapabilityConfig;

    /// Builds the capability from its own config alone. `cfg` holds the
    /// capability's instance config (e.g. `{"model_id": "my-model"}`), where
    /// `model_id` is a name resolved later by `resolve_refs`.
    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: AgentModelCapabilityConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
        })
    }
}

impl crate::registry::Named for AgentModelCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl crate::registry::Named for ResolvedAgentModelCapability {
    fn instance_id(&self) -> &str {
        self.inner.instance_id()
    }
}

impl AgentModelCapability {
    pub fn configured_model_id(&self) -> &str {
        &self.config.model_id
    }
}

impl crate::capabilities::CapabilityInfo for AgentModelCapability {
    fn name(&self) -> &str {
        "Agent Model Binding"
    }

    fn description(&self) -> &str {
        "Surfaces a configured model's connection details (base URL, model name, auth, TLS) to a launched agent."
    }

    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::AgentModel])
    }
}

impl crate::capabilities::CapabilityInfo for ResolvedAgentModelCapability {
    fn name(&self) -> &str {
        crate::capabilities::CapabilityInfo::name(&self.inner)
    }

    fn description(&self) -> &str {
        crate::capabilities::CapabilityInfo::description(&self.inner)
    }

    fn binding_types(&self) -> HashSet<BindingType> {
        crate::capabilities::CapabilityInfo::binding_types(&self.inner)
    }
}

impl Capability for AgentModelCapability {
    fn resolve_refs(
        self: Box<Self>,
        models: &dyn crate::models::ModelLookup,
    ) -> anyhow::Result<Box<dyn crate::capabilities::ResolvedCapability>> {
        let configured_model = crate::capabilities::base::resolve_declared_model(
            models,
            &Self::metadata(),
            &self.config.model_id,
        )?;
        Ok(Box::new(ResolvedAgentModelCapability {
            inner: *self,
            configured_model,
        }))
    }
}

#[async_trait]
impl crate::capabilities::ResolvedCapability for ResolvedAgentModelCapability {
    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let api_type = match request {
            BindingRequest::AgentModel(AgentModelBindingRequest { api_type }) => api_type,
            #[allow(unreachable_patterns)] // Will remove once more variants are available
            other => anyhow::bail!(
                "AgentModelCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };
        let model_id = &self.inner.config.model_id;
        let configured_model = &self.configured_model;

        let (provider, endpoint, model_name) = configured_model.resolve_provider_endpoint(
            model_id,
            api_type.clone(),
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
            context_length: Some(configured_model.model.context_length()),
            custom_headers: provider.custom_headers(),
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
    use crate::capabilities::{CapabilityInfo, ResolvedCapability};
    use crate::config::{Config, ModelConfig, ProviderConfig};
    use crate::utils::test_support::{FakeModel, FakeProvider};

    use crate::providers::{ApiEndpoint, ApiType};

    use std::collections::HashMap;
    use std::sync::Arc;

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
    ) -> ResolvedAgentModelCapability {
        capability_with_test_model_and_variant(functions, provider, None)
    }

    /// Like `capability_with_test_model` but also sets `configured_variant`
    /// and populates the test model's variants list, enabling `resolve_variant`
    /// and `model_alias` to be exercised at bind time.
    fn capability_with_test_model_and_variant(
        functions: Vec<ModelFunction>,
        provider: FakeProvider,
        configured_variant: Option<(&str, Vec<crate::models::ModelVariant>)>,
    ) -> ResolvedAgentModelCapability {
        let (variant_str, variants) = configured_variant
            .map(|(s, v)| (Some(s.to_string()), v))
            .unwrap_or((None, vec![]));
        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: variant_str.clone(),
            },
        );
        let cap = AgentModelCapability::new(
            "my-agent",
            &serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
        )
        .unwrap();
        // Replace the real model with our test double that has a custom provider
        // and the specified variants list.
        ResolvedAgentModelCapability {
            inner: cap,
            configured_model: crate::models::ConfiguredModel::for_test(
                Arc::new(FakeModel::text(functions).with_variants(variants)),
                std::sync::Arc::new(provider),
                variant_str,
            ),
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
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        let cap = AgentModelCapability::new(
            "my-agent",
            &serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
        )
        .unwrap();
        assert_eq!(
            cap.binding_types(),
            HashSet::from([BindingType::AgentModel])
        );
    }

    #[test]
    fn metadata_declares_a_required_model_dependency() {
        let deps = AgentModelCapability::metadata().dependencies;
        assert_eq!(deps.len(), 1);
        assert!(deps.iter().any(|d| matches!(
            d,
            Dependency::Model { config_key, required: true, .. } if config_key == "model_id"
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
