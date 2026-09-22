// Standard
use std::collections::HashMap;
use std::sync::LazyLock;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

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
    /// The configuration this source was built from. Only
    /// `config.capabilities` is read; `construct` takes the whole thing.
    config: crate::config::Config,
    /// The collection a capability's model name is resolved against.
    models: std::sync::Arc<crate::models::ModelSource>,
    cache: std::sync::Mutex<HashMap<String, std::sync::Arc<dyn ResolvedCapability>>>,
}

impl CapabilitySource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            config: config.clone(),
            models: std::sync::Arc::new(crate::models::ModelSource::from_config(config)),
            cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The capability configured under `capability_id`, built on the first
    /// ask and returned from the cache on every one after it. Errors when no
    /// entry is configured under that id, when its `capability_type` is not
    /// in the registry, or when the names it holds do not resolve.
    pub fn get(
        &self,
        capability_id: &str,
    ) -> anyhow::Result<std::sync::Arc<dyn ResolvedCapability>> {
        if let Some(built) = self.cache.lock().unwrap().get(capability_id) {
            return Ok(built.clone());
        }
        let capability_config = self
            .config
            .capabilities
            .get(capability_id)
            .ok_or_else(|| anyhow::anyhow!("capability '{capability_id}' is not configured"))?;

        let built = CAPABILITY_REGISTRY
            .construct(
                &capability_config.capability_type,
                &capability_config.capability_id,
                &capability_config.config,
            )
            .map_err(|e| e.about("capability", capability_id))?;

        // Wiring the capability to what it names is where a missing or
        // unsuitable model is reported. Spec 0024 gated this with a
        // `validate_ref` walk before constructing, because construction could
        // not report a failure; `resolve_refs` can, with the same outcome.
        let built = built
            .resolve_refs(&*self.models)
            .map_err(|e| anyhow::anyhow!("Skipping capability '{capability_id}': {e}"))?;

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
        self.config
            .capabilities
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

    #[test]
    fn capability_source_constructs_one_instance_per_named_capability() {
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

        let source = CapabilitySource::from_config(&config);
        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["chat".to_string()]);
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
