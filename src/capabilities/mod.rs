// Standard
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

use_channel!("CAPBL");

pub static CAPABILITY_REGISTRY: LazyLock<base::CapabilityFactory> = LazyLock::new(|| {
    let mut factory = base::CapabilityFactory::new();
    factory.register::<agent_model::AgentModelCapability>("agent-model");
    factory
});

/*-- CapabilitySource -----------------------------------------------------------*/

/// The real `Configured<dyn Capability>`: eagerly constructs a live
/// capability instance for every configured `CapabilityConfig`, keyed by its
/// instance nickname (`capability_id`) rather than its catalog type
/// (`capability_type`).
pub struct CapabilitySource {
    constructed: Vec<(String, Arc<dyn Capability>)>,
}

impl CapabilitySource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let constructed = config
            .capabilities
            .values()
            .filter_map(|capability_config| {
                let result = CAPABILITY_REGISTRY.construct_shared(
                    &capability_config.capability_type,
                    &capability_config.config,
                );
                if result.is_err() {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Could not construct capability '{}'",
                        capability_config.capability_type
                    );
                }
                result
                    .ok()
                    .map(|arc| (capability_config.capability_id.clone(), arc))
            })
            .collect();
        Self { constructed }
    }
}

impl crate::dependency::Configured<dyn Capability> for CapabilitySource {
    fn instances(&self) -> Vec<(String, Arc<dyn Capability + 'static>)> {
        self.constructed
            .iter()
            .map(|(id, arc)| (id.clone(), Arc::clone(arc)))
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
pub use base::{
    AgentModelBinding, AgentModelBindingRequest, Binding, BindingRequest, BindingType, Capability,
    CapabilityMetadata, Dependency, EnvBinding, LaunchContext,
};

mod requirement;
pub use requirement::{ModelRequirement, ProviderRequirement, ShellCommandRequirement};

mod agent_model;
pub use agent_model::{AgentModelCapability, AgentModelCapabilityConfig};

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CapabilityConfig, Config};
    use crate::dependency::Configured;

    fn agent_model_config(id: &str, model_id: &str) -> CapabilityConfig {
        CapabilityConfig {
            capability_id: id.to_string(),
            capability_type: "agent-model".to_string(),
            config: serde_json::json!({
                "model_id": model_id,
            }),
        }
    }

    #[test]
    fn capability_source_constructs_one_instance_per_named_capability() {
        let mut config = Config::default();
        config.capabilities.insert(
            "chat".to_string(),
            agent_model_config("chat", "granite-3.1-8b-instruct"),
        );

        let source = CapabilitySource::from_config(&config);
        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["chat".to_string()]);
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
    fn capability_registry_has_agent_model() {
        assert!(CAPABILITY_REGISTRY.get("agent-model").is_some());
    }
}
