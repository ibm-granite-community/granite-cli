//! `ModelProviderCapability`: surfaces every configured chat model as one
//! launcher provider entry per granite-cli provider, each pointing at the local
//! granite-cli proxy. The proxy routes each request to the model's real
//! provider and records its usage, so the launched process never needs a real
//! provider credential or a URL that knows where a model lives.

use crate::capabilities::base::{
    Binding, BindingRequest, BindingType, Capability, CapabilityMetadata, Dependency,
    HasCapabilityMetadata, ModelProviderBinding, ModelProviderBindingRequest, ModelProviderEntry,
    ModelProviderModel,
};
use crate::models::{ConfiguredModel, ModelFunction};
use crate::registry::{ConfigConstructable, ConstructError, NoConfig};
use async_trait::async_trait;
use std::collections::{BTreeMap, HashSet};

/*-- public --*/

pub(crate) struct ModelProviderCapability {
    instance_id: String,
}

/// [`ModelProviderCapability`] with every configured model resolved. Built
/// only by `resolve_refs`, so holding one is what says every model the
/// configuration holds resolved.
pub(crate) struct ResolvedModelProviderCapability {
    inner: ModelProviderCapability,
    models: Vec<ResolvedModel>,
}

/// One configured model, under the id the configuration knows it by, together
/// with the provider and variant it resolved to.
struct ResolvedModel {
    model_id: String,
    configured_model: ConfiguredModel,
}

impl ConfigConstructable for ModelProviderCapability {
    type Config = NoConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        // The capability takes no settings of its own: it covers the whole
        // configured model collection. Deserialising still names a config
        // that is not an object rather than silently ignoring it.
        serde_json::from_value::<NoConfig>(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
        })
    }
}

impl crate::registry::Named for ModelProviderCapability {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

impl crate::registry::Named for ResolvedModelProviderCapability {
    fn instance_id(&self) -> &str {
        self.inner.instance_id()
    }
}

impl crate::capabilities::CapabilityInfo for ModelProviderCapability {
    fn binding_types(&self) -> HashSet<BindingType> {
        HashSet::from([BindingType::ModelProvider])
    }
}

impl crate::capabilities::CapabilityInfo for ResolvedModelProviderCapability {
    fn binding_types(&self) -> HashSet<BindingType> {
        self.inner.binding_types()
    }
}

impl Capability for ModelProviderCapability {
    fn resolve_refs(
        self: Box<Self>,
        models: &dyn crate::models::ModelLookup,
    ) -> anyhow::Result<Box<dyn crate::capabilities::ResolvedCapability>> {
        let mut resolved = Vec::new();
        for model_id in models.configured_ids() {
            let configured_model = models
                .resolve(&model_id, None)
                .map_err(|e| anyhow::anyhow!("model '{model_id}': {e}"))?;
            resolved.push(ResolvedModel {
                model_id,
                configured_model,
            });
        }
        Ok(Box::new(ResolvedModelProviderCapability {
            inner: *self,
            models: resolved,
        }))
    }
}

#[async_trait]
impl crate::capabilities::ResolvedCapability for ResolvedModelProviderCapability {
    async fn bind(&self, request: BindingRequest) -> anyhow::Result<Binding> {
        let ModelProviderBindingRequest {
            supported_api_types,
        } = match request {
            BindingRequest::ModelProvider(request) => request,
            other => anyhow::bail!(
                "ModelProviderCapability does not handle {:?} binding requests",
                other.binding_type()
            ),
        };

        // One entry per configured provider, keyed by the provider's own name
        // so two providers that speak the same dialect stay distinct.
        let mut by_provider: BTreeMap<String, Vec<&ResolvedModel>> = BTreeMap::new();
        for model in &self.models {
            if !model
                .configured_model
                .model
                .supported_functions()
                .contains(&ModelFunction::Chat)
            {
                continue;
            }
            by_provider
                .entry(model.configured_model.provider.instance_id().to_string())
                .or_default()
                .push(model);
        }

        let mut providers = Vec::new();
        for (provider_name, models) in by_provider {
            let Some(first) = models.first() else {
                continue;
            };
            let provider = &first.configured_model.provider;
            let chat_endpoints = provider.endpoints_for_function(&ModelFunction::Chat);

            // One dialect per provider: the first the launcher can express
            // that this provider serves with a chat endpoint. The models of
            // one provider share its endpoints, so this is also the dialect
            // all of them are reached through.
            let Some(api_type) = supported_api_types.iter().find(|api_type| {
                provider.supported_api_types().contains(api_type)
                    && chat_endpoints
                        .iter()
                        .any(|endpoint| endpoint.api_type() == **api_type)
            }) else {
                continue;
            };

            let mut entry = ModelProviderEntry {
                provider_name,
                api_type: api_type.clone(),
                endpoint_path: String::new(),
                models: Vec::new(),
            };
            for model in models {
                let Ok((_, endpoint, model_name)) =
                    model.configured_model.resolve_provider_endpoint(
                        &model.model_id,
                        api_type.clone(),
                        ModelFunction::Chat,
                    )
                else {
                    continue;
                };
                // The operation path is a property of the dialect, so the
                // first model in the entry settles it for all of them.
                if entry.endpoint_path.is_empty() {
                    entry.endpoint_path = endpoint.path().to_string();
                }
                // A provider alias can repeat across configured models; one
                // selectable entry is enough, and the proxy routes by that
                // same name.
                if entry
                    .models
                    .iter()
                    .any(|existing| existing.model_name == model_name)
                {
                    continue;
                }
                entry.models.push(ModelProviderModel {
                    model_name,
                    // A model whose provider reported no context length is
                    // unknown, not zero: a launcher that takes a length must
                    // be able to omit it rather than render 0.
                    context_length: Some(model.configured_model.model.context_length())
                        .filter(|length| *length > 0),
                    supported_reasoning_efforts: model
                        .configured_model
                        .model
                        .supported_reasoning_efforts()
                        .to_vec(),
                });
            }
            if !entry.models.is_empty() {
                providers.push(entry);
            }
        }

        providers.retain(|provider| !provider.models.is_empty());
        Ok(Binding::ModelProvider(ModelProviderBinding { providers }))
    }
}

impl HasCapabilityMetadata for ModelProviderCapability {
    fn metadata() -> CapabilityMetadata {
        CapabilityMetadata {
            name: NAME.to_string(),
            description: DESCRIPTION.to_string(),
            dependencies: vec![Dependency::Models],
            tags: vec![
                "model".to_string(),
                "provider".to_string(),
                "proxy".to_string(),
            ],
            supported_binding_types: HashSet::from([BindingType::ModelProvider]),
            auto_configurable: true,
        }
    }
}

/*-- private --*/

const NAME: &str = "Model Provider";
const DESCRIPTION: &str = "Surfaces every configured chat model as one launcher provider entry per granite-cli provider, routed through the local granite-cli proxy.";

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::CapabilityInfo;
    use crate::models::{ModelFunction, ModelLookup};
    use crate::providers::{ApiEndpoint, ApiType};
    use crate::utils::test_support::{FakeModel, FakeProvider};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A model plus the provider that serves it, as the lookup hands them to
    /// `resolve_refs`.
    struct ModelSpec {
        id: &'static str,
        functions: Vec<ModelFunction>,
        context_length: u64,
        provider: FakeProvider,
    }

    struct FakeLookup {
        specs: Vec<ModelSpec>,
    }

    impl FakeLookup {
        fn new(specs: Vec<ModelSpec>) -> Self {
            Self { specs }
        }
    }

    impl ModelLookup for FakeLookup {
        fn resolve(
            &self,
            model_id: &str,
            _requirement: Option<&crate::capabilities::ModelRequirement>,
        ) -> anyhow::Result<ConfiguredModel> {
            let spec = self
                .specs
                .iter()
                .find(|spec| spec.id == model_id)
                .ok_or_else(|| anyhow::anyhow!("model '{model_id}' is not configured"))?;
            Ok(ConfiguredModel::for_test(
                Arc::new(
                    FakeModel::text(spec.functions.clone())
                        .with_context_length(spec.context_length),
                ),
                Arc::new(spec.provider.clone()),
                None,
            ))
        }

        fn configured_ids(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.specs.iter().map(|spec| spec.id.to_string()).collect();
            ids.sort();
            ids
        }
    }

    /// A provider that serves `Chat` over exactly `endpoints`.
    fn provider(name: &str, api_types: Vec<ApiType>, endpoints: Vec<ApiEndpoint>) -> FakeProvider {
        FakeProvider {
            instance_id: name.to_string(),
            base_url: "http://localhost:1234".to_string(),
            api_key: None,
            verify_ssl: true,
            api_types,
            endpoints: HashMap::from([(ModelFunction::Chat, endpoints)]),
            alias: None,
        }
    }

    fn chat_model(id: &'static str, provider: FakeProvider) -> ModelSpec {
        ModelSpec {
            id,
            functions: vec![ModelFunction::Chat],
            context_length: 4096,
            provider,
        }
    }

    fn capability() -> ModelProviderCapability {
        ModelProviderCapability::new("models", &serde_json::json!({})).unwrap()
    }

    async fn bind_specs(
        specs: Vec<ModelSpec>,
        supported_api_types: Vec<ApiType>,
    ) -> ModelProviderBinding {
        let resolved = Box::new(capability())
            .resolve_refs(&FakeLookup::new(specs))
            .unwrap();
        let binding = resolved
            .bind(BindingRequest::ModelProvider(ModelProviderBindingRequest {
                supported_api_types,
            }))
            .await
            .unwrap();
        let Binding::ModelProvider(binding) = binding else {
            panic!("expected a ModelProvider binding");
        };
        binding
    }

    #[tokio::test]
    async fn makes_one_entry_per_provider_named_after_it() {
        let binding = bind_specs(
            vec![
                chat_model(
                    "openai-a",
                    provider(
                        "openai-a",
                        vec![ApiType::OpenAI],
                        vec![ApiEndpoint::OpenAIChat],
                    ),
                ),
                chat_model(
                    "anthropic-only",
                    provider(
                        "anthropic-only",
                        vec![ApiType::Anthropic],
                        vec![ApiEndpoint::AnthropicMessages],
                    ),
                ),
                chat_model(
                    "openai-b",
                    provider(
                        "openai-b",
                        vec![ApiType::OpenAI],
                        vec![ApiEndpoint::OpenAIChat],
                    ),
                ),
            ],
            vec![ApiType::OpenAI, ApiType::Anthropic],
        )
        .await;

        // One entry per provider, sorted by provider name.
        assert_eq!(binding.providers.len(), 3);
        assert_eq!(binding.providers[0].provider_name, "anthropic-only");
        assert_eq!(binding.providers[0].api_type, ApiType::Anthropic);
        assert_eq!(binding.providers[0].endpoint_path, "/v1/messages");
        assert_eq!(binding.providers[0].models[0].model_name, "anthropic-only");

        assert_eq!(binding.providers[1].provider_name, "openai-a");
        assert_eq!(binding.providers[1].api_type, ApiType::OpenAI);
        assert_eq!(binding.providers[1].models[0].model_name, "openai-a");

        assert_eq!(binding.providers[2].provider_name, "openai-b");
        assert_eq!(binding.providers[2].api_type, ApiType::OpenAI);
        assert_eq!(binding.providers[2].models[0].model_name, "openai-b");
    }

    #[tokio::test]
    async fn two_providers_that_speak_the_same_dialect_stay_distinct() {
        let binding = bind_specs(
            vec![
                chat_model(
                    "model-a",
                    provider(
                        "provider-a",
                        vec![ApiType::OpenAI],
                        vec![ApiEndpoint::OpenAIChat],
                    ),
                ),
                chat_model(
                    "model-b",
                    provider(
                        "provider-b",
                        vec![ApiType::OpenAI],
                        vec![ApiEndpoint::OpenAIChat],
                    ),
                ),
            ],
            vec![ApiType::OpenAI],
        )
        .await;

        let names: Vec<&str> = binding
            .providers
            .iter()
            .map(|provider| provider.provider_name.as_str())
            .collect();
        assert_eq!(names, vec!["provider-a", "provider-b"]);
    }

    #[tokio::test]
    async fn a_multi_dialect_provider_is_listed_once_under_the_first_requested_dialect() {
        let binding = bind_specs(
            vec![chat_model(
                "both",
                provider(
                    "both",
                    vec![ApiType::OpenAI, ApiType::Anthropic],
                    vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
                ),
            )],
            vec![ApiType::OpenAI, ApiType::Anthropic],
        )
        .await;

        assert_eq!(binding.providers.len(), 1);
        assert_eq!(binding.providers[0].provider_name, "both");
        assert_eq!(binding.providers[0].api_type, ApiType::OpenAI);
        assert_eq!(binding.providers[0].models.len(), 1);
        assert_eq!(binding.providers[0].models[0].model_name, "both");
    }

    #[tokio::test]
    async fn the_chosen_dialect_follows_the_launchers_declaration() {
        let binding = bind_specs(
            vec![chat_model(
                "both",
                provider(
                    "both",
                    vec![ApiType::OpenAI, ApiType::Anthropic],
                    vec![ApiEndpoint::OpenAIChat, ApiEndpoint::AnthropicMessages],
                ),
            )],
            vec![ApiType::Anthropic, ApiType::OpenAI],
        )
        .await;

        assert_eq!(binding.providers.len(), 1);
        assert_eq!(binding.providers[0].api_type, ApiType::Anthropic);
    }

    #[tokio::test]
    async fn a_zero_context_length_is_carried_as_unknown() {
        let binding = bind_specs(
            vec![ModelSpec {
                id: "zero-context",
                functions: vec![ModelFunction::Chat],
                context_length: 0,
                provider: provider("zero", vec![ApiType::OpenAI], vec![ApiEndpoint::OpenAIChat]),
            }],
            vec![ApiType::OpenAI],
        )
        .await;

        assert_eq!(binding.providers[0].models[0].context_length, None);
    }

    #[tokio::test]
    async fn a_known_context_length_is_carried_through() {
        let binding = bind_specs(
            vec![chat_model(
                "known",
                provider(
                    "known-provider",
                    vec![ApiType::OpenAI],
                    vec![ApiEndpoint::OpenAIChat],
                ),
            )],
            vec![ApiType::OpenAI],
        )
        .await;

        assert_eq!(binding.providers[0].models[0].context_length, Some(4096));
    }

    #[tokio::test]
    async fn models_without_chat_are_left_out() {
        let binding = bind_specs(
            vec![
                ModelSpec {
                    id: "embedding",
                    functions: vec![ModelFunction::Embeddings],
                    context_length: 4096,
                    provider: provider(
                        "embed",
                        vec![ApiType::OpenAI],
                        vec![ApiEndpoint::OpenAIEmbeddings],
                    ),
                },
                chat_model(
                    "chat",
                    provider(
                        "chat-provider",
                        vec![ApiType::OpenAI],
                        vec![ApiEndpoint::OpenAIChat],
                    ),
                ),
            ],
            vec![ApiType::OpenAI],
        )
        .await;

        assert_eq!(binding.providers.len(), 1);
        assert_eq!(
            binding.providers[0]
                .models
                .iter()
                .map(|model| model.model_name.as_str())
                .collect::<Vec<_>>(),
            vec!["chat"]
        );
    }

    #[tokio::test]
    async fn a_model_whose_provider_cannot_serve_any_supported_dialect_is_left_out() {
        let binding = bind_specs(
            vec![chat_model(
                "ollama-only",
                provider(
                    "ollama-only",
                    vec![ApiType::Ollama],
                    vec![ApiEndpoint::OllamaChat],
                ),
            )],
            vec![ApiType::OpenAI, ApiType::Anthropic],
        )
        .await;

        assert!(binding.providers.is_empty());
    }

    #[tokio::test]
    async fn an_empty_configuration_yields_an_empty_binding() {
        let binding = bind_specs(vec![], vec![ApiType::OpenAI]).await;
        assert!(binding.providers.is_empty());
    }

    #[tokio::test]
    async fn a_provider_alias_that_repeats_is_listed_once() {
        let mut first = provider(
            "shared-provider",
            vec![ApiType::OpenAI],
            vec![ApiEndpoint::OpenAIChat],
        );
        first.alias = Some("shared".to_string());
        let mut second = provider(
            "shared-provider",
            vec![ApiType::OpenAI],
            vec![ApiEndpoint::OpenAIChat],
        );
        second.alias = Some("shared".to_string());

        let binding = bind_specs(
            vec![chat_model("first", first), chat_model("second", second)],
            vec![ApiType::OpenAI],
        )
        .await;

        assert_eq!(binding.providers.len(), 1);
        assert_eq!(binding.providers[0].models.len(), 1);
        assert_eq!(binding.providers[0].models[0].model_name, "shared");
    }

    #[tokio::test]
    async fn resolves_every_configured_model_and_fails_on_a_missing_one() {
        let lookup = FakeLookup::new(vec![chat_model(
            "present",
            provider(
                "present",
                vec![ApiType::OpenAI],
                vec![ApiEndpoint::OpenAIChat],
            ),
        )]);
        // The lookup claims an id it cannot resolve, which is what a removed
        // model leaves behind.
        struct HalfBrokenLookup;
        impl ModelLookup for HalfBrokenLookup {
            fn resolve(
                &self,
                model_id: &str,
                _requirement: Option<&crate::capabilities::ModelRequirement>,
            ) -> anyhow::Result<ConfiguredModel> {
                anyhow::bail!("model '{model_id}' is not configured")
            }
            fn configured_ids(&self) -> Vec<String> {
                vec!["gone".to_string()]
            }
        }

        let Err(error) = Box::new(capability()).resolve_refs(&HalfBrokenLookup) else {
            panic!("a missing model must stop resolution");
        };
        assert!(error.to_string().contains("gone"), "{error}");

        // The healthy lookup resolves.
        assert!(
            Box::new(capability()).resolve_refs(&lookup).is_ok(),
            "a configured model resolves"
        );
    }

    #[test]
    fn binding_types_reports_model_provider() {
        assert_eq!(
            capability().binding_types(),
            HashSet::from([BindingType::ModelProvider])
        );
    }

    #[test]
    fn metadata_declares_the_whole_model_collection() {
        let meta = ModelProviderCapability::metadata();
        assert!(
            meta.dependencies
                .iter()
                .any(|dependency| matches!(dependency, Dependency::Models))
        );
        assert_eq!(
            meta.supported_binding_types,
            HashSet::from([BindingType::ModelProvider])
        );
        assert_eq!(meta.name, "Model Provider");
    }

    #[test]
    fn settings_that_are_not_an_object_are_rejected() {
        let Err(error) = ModelProviderCapability::new("models", &serde_json::json!("nope")) else {
            panic!("a non-object config must not construct");
        };
        assert!(error.to_string().contains("invalid type"), "{error}");
    }
}
