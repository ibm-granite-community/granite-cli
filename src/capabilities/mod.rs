// Standard
use std::collections::HashMap;
use std::sync::LazyLock;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

// Local
use crate::sources::SourceError;

use_channel!("CAPBL");

pub static CAPABILITY_REGISTRY: LazyLock<base::CapabilityFactory> = LazyLock::new(|| {
    let mut factory = base::CapabilityFactory::new();
    factory.register::<agent_model::AgentModelCapability>("agent-model");
    factory.register::<vision_mcp::VisionMCPCapability>("vision-mcp");
    factory.register::<sub_agent::SubAgentCapability>("sub-agent");
    factory.register::<sub_agent_code::CodeSubAgentCapability>("sub-agent-code");
    factory.register::<sub_agent_explore::ExploreSubAgentCapability>("sub-agent-explore");
    factory.register::<sub_agent_plan::PlanSubAgentCapability>("sub-agent-plan");
    factory
});

/*-- CapabilitySource -----------------------------------------------------------*/

/// The real `Configured<dyn Capability>`: builds a live capability instance
/// the first time one is asked for by its instance nickname
/// (`capability_id`) rather than its catalog type (`capability_type`). The
/// instance is kept, so every later ask for that id returns the same object.
pub struct CapabilitySource {
    /// The settings for this kind, from the configuration snapshot the
    /// source was built from.
    configs: HashMap<String, crate::config::CapabilityConfig>,
    /// The collection a capability's model name is resolved against.
    models: std::sync::Arc<crate::models::ModelSource>,
    cache: std::sync::Mutex<HashMap<String, std::sync::Arc<dyn ResolvedCapability>>>,
}

impl CapabilitySource {
    /// Capabilities resolved against a model source somebody else built, so
    /// two capabilities naming one model share the object it resolved to.
    pub(crate) fn with_models(
        config: &crate::config::Config,
        models: std::sync::Arc<crate::models::ModelSource>,
    ) -> Self {
        Self {
            configs: config.capabilities.clone(),
            models,
            cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The capability configured under `capability_id`, built on the first
    /// ask and returned from the cache on every one after it. Errors when no
    /// entry is configured under that id, when its `capability_type` is not
    /// in the registry, when its settings are not valid, or when the model it
    /// names does not resolve or does not meet what its type requires.
    pub fn get(
        &self,
        capability_id: &str,
    ) -> anyhow::Result<std::sync::Arc<dyn ResolvedCapability>> {
        self.build(capability_id)
            .map_err(|e| e.about("capability", capability_id))
    }

    /// The same, as the typed failure the validator turns into a problem it
    /// reports. The instance stays in the cache, so a command that goes on
    /// to use it does not build it again.
    pub(crate) fn build(
        &self,
        capability_id: &str,
    ) -> Result<std::sync::Arc<dyn ResolvedCapability>, SourceError> {
        if let Some(built) = self.cache.lock().unwrap().get(capability_id) {
            return Ok(built.clone());
        }
        let capability_config = self
            .configs
            .get(capability_id)
            .ok_or(SourceError::NotConfigured)?;

        let built = CAPABILITY_REGISTRY.construct(
            &capability_config.capability_type,
            &capability_config.capability_id,
            &capability_config.config,
        )?;

        // Wiring the capability to what it names is where a missing or
        // unsuitable model is reported. A model that does not meet the
        // requirement comes back typed, so it is reported with both names.
        let built =
            built.resolve_refs(&*self.models).map_err(|e| match e
                .downcast::<crate::models::UnmetRequirement>()
            {
                Ok(unmet) => SourceError::UnmetRequirement {
                    model_id: unmet.model_id,
                    unmet: unmet.unmet,
                },
                Err(e) => SourceError::Unresolved(e.to_string()),
            })?;

        let built: std::sync::Arc<dyn ResolvedCapability> = std::sync::Arc::from(built);
        // Built outside the lock, so two callers can reach here for one id.
        // `or_insert` keeps whichever landed first and drops the other, so
        // the id has one instance however the calls interleave.
        Ok(self
            .cache
            .lock()
            .unwrap()
            .entry(capability_id.to_string())
            .or_insert(built)
            .clone())
    }
}

impl crate::dependency::Configured<dyn ResolvedCapability> for CapabilitySource {
    fn instances(&self) -> Vec<(String, std::sync::Arc<dyn ResolvedCapability + 'static>)> {
        self.configs
            .keys()
            .filter_map(|id| match self.get(id) {
                Ok(capability) => Some((id.clone(), capability)),
                Err(e) => {
                    alog_channel!(MessageLevel::Warning, "{e}");
                    None
                }
            })
            .collect()
    }

    fn catalog(&self) -> HashMap<&'static str, CapabilityMetadata> {
        CAPABILITY_REGISTRY.entries()
    }

    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
        CAPABILITY_REGISTRY.config_schema(type_name)
    }
}

/*-- Module Declarations -----------------------------------------------------*/

mod base;
pub use crate::providers::ApiType;
pub use base::{
    AgentModelBinding, AgentModelBindingRequest, Binding, BindingRequest, BindingType, Capability,
    CapabilityInfo, CapabilityMetadata, Dependency, EnvBinding, KnownSubAgent, LaunchContext,
    McpBinding, McpBindingRequest, McpTransportKind, ResolvedCapability, SubAgentBinding,
    SubAgentBindingRequest, ToolName,
};

mod requirement;
pub use requirement::{ModelRequirement, ProviderRequirement, ShellCommandRequirement};

mod agent_model;
pub use agent_model::{AgentModelCapability, AgentModelCapabilityConfig};

mod vision_mcp;
pub use vision_mcp::{VisionMCPCapability, VisionMCPCapabilityConfig};

mod sub_agent;
pub use sub_agent::{SubAgentCapability, SubAgentCapabilityConfig};

mod sub_agent_code;
pub use sub_agent_code::{CodeSubAgentCapability, CodeSubAgentCapabilityConfig};

mod sub_agent_explore;
pub use sub_agent_explore::{ExploreSubAgentCapability, ExploreSubAgentCapabilityConfig};

mod sub_agent_plan;
pub use sub_agent_plan::{PlanSubAgentCapability, PlanSubAgentCapabilityConfig};

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityConfig, Config, ModelConfig, ProviderConfig};
    use crate::dependency::Configured;

    fn agent_model_config(id: &str, model_key: &str) -> CapabilityConfig {
        CapabilityConfig {
            capability_id: id.to_string(),
            capability_type: "agent-model".to_string(),
            config: serde_json::json!({
                "model_id": model_key,
            }),
        }
    }

    /// One `agent-model` capability, `chat`, naming a model whose provider is
    /// configured.
    fn one_chat_capability() -> Config {
        let mut config = Config::default();
        // The model needs a provider to bind, so the capability is only
        // constructible with one configured.
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
        config.capabilities.insert(
            "chat".to_string(),
            agent_model_config("chat", "granite-3.1-8b-instruct"),
        );

        config
    }

    impl CapabilitySource {
        /// Capabilities over a model source of their own, for a test that needs
        /// no other kind. Commands ask the application context.
        #[cfg(test)]
        pub(crate) fn from_config(config: &crate::config::Config) -> Self {
            Self::with_models(
                config,
                std::sync::Arc::new(crate::models::ModelSource::from_config(config)),
            )
        }
    }

    #[test]
    fn capability_source_constructs_one_instance_per_named_capability() {
        let config = one_chat_capability();
        let source = CapabilitySource::from_config(&config);
        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["chat".to_string()]);
    }

    #[test]
    fn every_type_reports_an_unmet_requirement_as_one() {
        // `build` recognises an unmet requirement by downcasting the error
        // `resolve_refs` returns, which works while each type passes that
        // error on unchanged. A type that rewraps it as text would have its
        // mismatch reported as `Unresolved`; this checks every registered
        // type against a model that supports no functions at all.
        let mut checked = 0;
        for (type_name, metadata) in CAPABILITY_REGISTRY.entries() {
            for dependency in &metadata.dependencies {
                let Dependency::Model { config_key, .. } = dependency else {
                    continue;
                };
                let mut config = one_chat_capability();
                config.models.insert(
                    "bare".to_string(),
                    ModelConfig {
                        model_id: "bare".to_string(),
                        model_type: "custom".to_string(),
                        config: serde_json::json!({
                            "family": "Test",
                            "supported_functions": [],
                        }),
                        provider_id: "ollama".to_string(),
                        variant: None,
                    },
                );
                let mut settings = CAPABILITY_REGISTRY.default_config(type_name);
                // Defaults can leave required text empty, such as a generic
                // sub-agent's description and prompt, which would fail
                // construction before the model is reached.
                for value in settings
                    .as_object_mut()
                    .into_iter()
                    .flat_map(|o| o.values_mut())
                {
                    if value.as_str() == Some("") {
                        *value = serde_json::json!("set by the test");
                    }
                }
                settings[config_key.as_str()] = serde_json::json!("bare");
                config.capabilities.insert(
                    "under-test".to_string(),
                    CapabilityConfig {
                        capability_id: "under-test".to_string(),
                        capability_type: type_name.to_string(),
                        config: settings,
                    },
                );

                // A type whose requirement a model with no functions meets
                // has nothing to report.
                let Err(error) = CapabilitySource::from_config(&config).build("under-test") else {
                    continue;
                };
                assert!(
                    matches!(error, SourceError::UnmetRequirement { ref model_id, .. } if model_id == "bare"),
                    "'{type_name}' reported {error:?}"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "no capability type declares a model requirement"
        );
    }

    #[test]
    fn the_capability_the_check_built_is_the_one_a_command_gets() {
        use crate::config::validation::{RefKind, validate_ref};

        let config = one_chat_capability();
        let sources = crate::sources::Sources::build(&config, None);

        validate_ref(RefKind::Capability, "chat", &config, &sources).unwrap();

        let capabilities = sources.capabilities();
        let cached = capabilities
            .cache
            .lock()
            .unwrap()
            .get("chat")
            .cloned()
            .expect("the check left the capability in the cache");
        assert!(std::sync::Arc::ptr_eq(
            &cached,
            &capabilities.get("chat").unwrap()
        ));
    }

    /// Records the id it was asked for and refuses it, so a test can see
    /// which name a capability's `resolve_refs` consumed.
    struct RecordingLookup {
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl crate::models::ModelLookup for RecordingLookup {
        fn resolve(
            &self,
            model_id: &str,
            _requirement: Option<&crate::capabilities::ModelRequirement>,
        ) -> anyhow::Result<crate::models::ConfiguredModel> {
            self.asked.lock().unwrap().push(model_id.to_string());
            anyhow::bail!("recorded")
        }
    }

    #[test]
    fn every_type_resolves_the_id_its_metadata_declares() {
        // The key a capability's metadata names and the field its
        // `resolve_refs` reads are two declarations of one fact. This walks
        // every registered type and fails if they drift apart.
        for (type_name, metadata) in CAPABILITY_REGISTRY.entries() {
            let Some(config_key) = metadata.dependencies.iter().find_map(|d| match d {
                Dependency::Model { config_key, .. } => Some(config_key.clone()),
                _ => None,
            }) else {
                continue;
            };

            // A superset of what any registered type's config needs. Serde
            // ignores the fields a given type does not declare, and a config
            // that fails to parse would silently become a default, which is
            // the empty `model_id` this assertion would then catch.
            let cfg = serde_json::json!({
                &config_key: "the-model-it-names",
                "description": "a probe",
                "prompt": "a probe",
            });
            let capability = CAPABILITY_REGISTRY
                .construct(type_name, "an-instance", &cfg)
                .unwrap_or_else(|e| panic!("{type_name} must construct from its own config: {e}"));

            let lookup = RecordingLookup {
                asked: std::sync::Mutex::new(Vec::new()),
            };
            let _ = capability.resolve_refs(&lookup);
            assert_eq!(
                *lookup.asked.lock().unwrap(),
                vec!["the-model-it-names".to_string()],
                "{type_name} declares '{config_key}' but resolved something else"
            );
        }
    }

    #[test]
    fn a_capability_whose_model_is_gone_errors_and_is_omitted_from_instances() {
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
        config.capabilities.insert(
            "healthy".to_string(),
            agent_model_config("healthy", "granite-3.1-8b-instruct"),
        );
        // As `model remove` would leave it: the capability still names a
        // model that is no longer configured.
        config
            .capabilities
            .insert("broken".to_string(), agent_model_config("broken", "gone"));

        let source = CapabilitySource::from_config(&config);

        let err = source
            .get("broken")
            .err()
            .expect("a capability naming a removed model must not resolve")
            .to_string();
        assert!(
            err.contains("broken") && err.contains("gone"),
            "the error must name the capability and the model it wanted, got: {err}"
        );

        assert!(source.get("healthy").is_ok());
        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["healthy".to_string()]);
    }

    #[test]
    fn settings_that_cannot_be_read_are_named_and_left_out() {
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
        config.capabilities.insert(
            "healthy".to_string(),
            agent_model_config("healthy", "granite-3.1-8b-instruct"),
        );
        config.capabilities.insert(
            "broken".to_string(),
            CapabilityConfig {
                capability_id: "broken".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": 42 }),
            },
        );

        let source = CapabilitySource::from_config(&config);
        let err = source.get("broken").err().unwrap().to_string();
        assert!(
            err.contains("capability 'broken'") && err.contains("invalid type"),
            "expected the instance and what serde said, got: {err}"
        );

        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["healthy".to_string()]);
    }

    #[test]
    fn capability_source_skips_unknown_capability_types() {
        let mut config = Config::default();
        config.capabilities.insert(
            "bogus".to_string(),
            CapabilityConfig {
                capability_id: "bogus".to_string(),
                capability_type: "not-a-real-capability".to_string(),
                config: serde_json::json!({}),
            },
        );

        let source = CapabilitySource::from_config(&config);
        assert!(source.instances().is_empty());
    }

    #[test]
    fn capability_source_skips_a_capability_whose_model_is_gone() {
        let mut config = Config::default();
        config.capabilities.insert(
            "chat".to_string(),
            agent_model_config("chat", "granite-3.1-8b-instruct"),
        );

        // No model entry, so constructing `chat` would panic inside
        // ConfiguredModel::resolve. It is skipped before reaching that.
        let source = CapabilitySource::from_config(&config);
        assert!(source.instances().is_empty());
    }

    #[test]
    fn capability_registry_has_agent_model() {
        assert!(CAPABILITY_REGISTRY.get("agent-model").is_some());
    }

    #[test]
    fn capability_registry_has_sub_agent() {
        assert!(CAPABILITY_REGISTRY.get("sub-agent").is_some());
    }

    #[test]
    fn capability_registry_has_sub_agent_plan() {
        assert!(CAPABILITY_REGISTRY.get("sub-agent-plan").is_some());
    }
}
