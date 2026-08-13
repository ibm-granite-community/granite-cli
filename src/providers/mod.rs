// Standard
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

use_channel!("PROVD");

/*-- Provider Registry -------------------------------------------------------*/

pub static PROVIDER_REGISTRY: LazyLock<base::ProviderFactory> = LazyLock::new(|| {
    let mut factory = base::ProviderFactory::new();
    factory.register::<openai::OpenAIProvider>("openai-compatible");
    factory.register::<ollama::OllamaProvider>("ollama");
    factory.register::<llamacpp::LlamaCppProvider>("llama-cpp");
    factory.register::<lmstudio::LMStudioProvider>("lm-studio");
    factory
});

/*-- ProviderSource -----------------------------------------------------------*/

/// The real `Configured<dyn Provider>`: eagerly constructs a live provider
/// instance for every enabled `ProviderConfig`, keyed by its instance
/// nickname (`provider_id`) rather than its catalog type (`provider_type`) --
/// this is what lets multiple named instances of one catalog type (e.g.
/// `openai-compatible` backing `llama-cpp`, `ollama`, `lm-studio`) coexist.
pub struct ProviderSource {
    constructed: Vec<(String, Arc<dyn Provider>)>,
}

impl ProviderSource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let constructed = config
            .providers
            .values()
            .filter_map(|provider_config| {
                let result = PROVIDER_REGISTRY
                    .construct_shared(&provider_config.provider_type, &provider_config.config);
                if result.is_err() {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Could not construct provider '{}'",
                        provider_config.provider_type
                    );
                }
                result
                    .ok()
                    .map(|arc| (provider_config.provider_id.clone(), arc))
            })
            .collect();
        Self { constructed }
    }
}

impl crate::dependency::Configured<dyn Provider> for ProviderSource {
    fn instances(&self) -> Vec<(String, Arc<dyn Provider + 'static>)> {
        self.constructed
            .iter()
            .map(|(id, arc)| (id.clone(), Arc::clone(arc)))
            .collect()
    }

    fn catalog(&self) -> HashMap<&'static str, ProviderMetadata> {
        PROVIDER_REGISTRY.entries()
    }

    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
        PROVIDER_REGISTRY.config_schema(type_name)
    }
}

/*-- Module Declarations -----------------------------------------------------*/

mod base;
pub use base::{
    ApiEndpoint, ApiType, AuthType, HealthStatus, ModelFormat, Provider, ProviderError,
    ProviderMetadata, ProviderType, PullResult,
};

mod openai;
pub use openai::{OpenAIProvider, OpenAIProviderConfig};

mod ollama;
pub use ollama::{OllamaProvider, OllamaProviderConfig};

mod llamacpp;
pub use llamacpp::{LlamaCppProvider, LlamaCppProviderConfig};

mod lmstudio;
pub use lmstudio::{LMStudioProvider, LMStudioProviderConfig};

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProviderConfig};
    use crate::dependency::{Configured, DependsOn, Requirement, resolve};

    fn openai_provider_config(id: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_id: id.to_string(),
            provider_type: "openai-compatible".to_string(),
            config: serde_json::json!({ "base_url": base_url }),
        }
    }

    fn config_with_two_named_instances() -> Config {
        let mut config = Config::default();
        config.providers.insert(
            "llama-cpp".to_string(),
            openai_provider_config("llama-cpp", "http://localhost:8080"),
        );
        config.providers.insert(
            "ollama".to_string(),
            openai_provider_config("ollama", "http://localhost:11434"),
        );
        config
    }

    #[test]
    fn provider_source_constructs_one_instance_per_named_provider() {
        let config = config_with_two_named_instances();
        let source = ProviderSource::from_config(&config);

        let mut ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, vec!["llama-cpp".to_string(), "ollama".to_string()]);
    }

    struct AnyGguf;
    impl Requirement<dyn Provider> for AnyGguf {
        fn admits_type(&self, metadata: &ProviderMetadata) -> bool {
            metadata.supported_formats.contains(&ModelFormat::GGUF)
        }
        fn admits_instance(&self, instance: &dyn Provider) -> bool {
            instance.can_run_model("gguf", "fp16")
        }
    }
    impl DependsOn<dyn Provider> for AnyGguf {
        type Requirement = Self;
        fn requirement(&self) -> Self {
            AnyGguf
        }
    }

    #[test]
    fn resolve_surfaces_all_matching_named_instances() {
        let config = config_with_two_named_instances();
        let source = ProviderSource::from_config(&config);

        let resolution = resolve(&AnyGguf, &source);
        let mut ids = resolution.existing_instances;
        ids.sort();
        assert_eq!(ids, vec!["llama-cpp".to_string(), "ollama".to_string()]);
    }
}
