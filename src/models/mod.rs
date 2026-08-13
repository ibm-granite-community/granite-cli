// Standard
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

// Include generated code from build.rs
include!(concat!(env!("OUT_DIR"), "/generated_models.rs"));

use_channel!("MODEL");

/*-- Public API --------------------------------------------------------------*/

pub static MODEL_REGISTRY: LazyLock<base::ModelFactory> = LazyLock::new(|| {
    let mut factory = base::ModelFactory::new();
    register_all_models(&mut factory);
    factory
});

/*-- ModelSource ---------------------------------------------------------------*/

/// The real `Configured<dyn Model>` + `ModelConfigured`: eagerly constructs a
/// shared model instance for every model referenced by the config's `models`
/// map, keyed by its catalog id. Instances are memoised inside `MODEL_REGISTRY`
/// so repeated construction of the same `(model_id, provider_id)` pair returns
/// the same `Arc`.
///
/// Provider configs are captured from `Config` at construction time so that
/// `provider_for()` can resolve live provider data at call time rather than
/// using a snapshot baked into the model struct.
pub struct ModelSource {
    constructed: Vec<(String, Arc<dyn Model>)>,
    /// Provider configs captured from `Config` when this source was built.
    /// `provider_for()` uses this map to resolve provider data at call time.
    provider_configs: HashMap<String, crate::config::ProviderConfig>,
}

impl ModelSource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let constructed = config
            .models
            .values()
            .filter_map(|model_config| {
                // Pass only the provider_id string — no ProviderConfig blob.
                // The model struct stores this id; actual provider resolution
                // happens at call time via provider_for().
                let cfg = serde_json::json!({
                    "provider_id": model_config.provider_id,
                });
                let result = MODEL_REGISTRY.construct_shared(&model_config.model_id, &cfg);
                if result.is_err() {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Could not construct model '{}'",
                        model_config.model_id
                    );
                }
                result.ok().map(|arc| (model_config.model_id.clone(), arc))
            })
            .collect();
        Self {
            constructed,
            provider_configs: config.providers.clone(),
        }
    }
}

/*-- ModelConfigured -----------------------------------------------------------*/

/// Extension of [`crate::dependency::Configured<dyn Model>`] that resolves a
/// live `Provider` for a model instance using the source's own provider config
/// map.
///
/// Keeping provider resolution here (rather than in the generic `dependency`
/// module) avoids a downward coupling from the foundation layer back into the
/// model and provider domain layers.
pub trait ModelConfigured: crate::dependency::Configured<dyn Model> + Send + Sync {
    /// Construct a `Provider` for `model` using this source's live provider
    /// config map. Returns an error if the model has no provider id or the
    /// referenced provider is not in the config map.
    fn provider_for(
        &self,
        model: &dyn Model,
    ) -> anyhow::Result<Box<dyn crate::providers::Provider>>;
}

impl crate::dependency::Configured<dyn Model> for ModelSource {
    fn instances(&self) -> Vec<(String, Arc<dyn Model + 'static>)> {
        self.constructed
            .iter()
            .map(|(id, arc)| (id.clone(), Arc::clone(arc)))
            .collect()
    }

    fn catalog(&self) -> HashMap<&'static str, ModelMetadata> {
        MODEL_REGISTRY.entries()
    }

    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
        MODEL_REGISTRY.config_schema(type_name)
    }
}

impl ModelConfigured for ModelSource {
    fn provider_for(
        &self,
        model: &dyn Model,
    ) -> anyhow::Result<Box<dyn crate::providers::Provider>> {
        let pid = model
            .provider_id()
            .ok_or_else(|| anyhow::anyhow!("model has no configured provider"))?;
        let pc = self.provider_configs.get(pid).ok_or_else(|| {
            anyhow::anyhow!("provider '{pid}' referenced by model is not configured")
        })?;
        crate::providers::PROVIDER_REGISTRY
            .construct(&pc.provider_type, &pc.config)
            .map_err(|e| anyhow::anyhow!(e))
    }
}

// Re-export types from base
mod base;
pub use base::{
    LayerKind, LayerTypeCount, MambaShape, Model, ModelArchitecture, ModelFunction, ModelMetadata,
    ModelType, ModelVariant,
};

pub(crate) mod context_fit;
pub use context_fit::ContextFit;

pub mod huggingface;

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_source_constructs_one_instance_per_configured_model() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::Configured;

        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );
        config.models.insert(
            "granite-guardian-3.1-8b".to_string(),
            ModelConfig {
                model_id: "granite-guardian-3.1-8b".to_string(),
                provider_id: None,
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        let mut ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "granite-3.1-8b-instruct".to_string(),
                "granite-guardian-3.1-8b".to_string()
            ]
        );
    }

    #[test]
    fn model_source_resolves_provider_from_provider_id() {
        use crate::config::{Config, ModelConfig, ProviderConfig};
        use crate::dependency::{Configured, ModelConfigured};

        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": "http://localhost:11434" }),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: Some("ollama".to_string()),
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        let (_, model) = source
            .instances()
            .into_iter()
            .find(|(id, _)| id == "granite-3.1-8b-instruct")
            .unwrap();
        // Resolution goes through the source's live provider_configs map.
        let provider = source.provider_for(model.as_ref()).unwrap();
        assert_eq!(provider.base_url(), "http://localhost:11434");
    }

    #[test]
    fn model_source_provider_errs_when_provider_id_unresolvable() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::{Configured, ModelConfigured};

        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: Some("does-not-exist".to_string()),
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        let (_, model) = source
            .instances()
            .into_iter()
            .find(|(id, _)| id == "granite-3.1-8b-instruct")
            .unwrap();
        assert!(source.provider_for(model.as_ref()).is_err());
    }

    #[test]
    fn model_source_reflects_updated_provider_config_on_rebuild() {
        use crate::config::{Config, ModelConfig, ProviderConfig};
        use crate::dependency::{Configured, ModelConfigured};

        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": "http://localhost:11434" }),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: Some("ollama".to_string()),
                variant: None,
            },
        );

        // First source — old URL
        let source1 = ModelSource::from_config(&config);
        let (_, model1) = source1
            .instances()
            .into_iter()
            .find(|(id, _)| id == "granite-3.1-8b-instruct")
            .unwrap();
        assert_eq!(
            source1.provider_for(model1.as_ref()).unwrap().base_url(),
            "http://localhost:11434"
        );

        // Simulate user editing the provider config
        config.providers.get_mut("ollama").unwrap().config =
            serde_json::json!({ "base_url": "http://localhost:9999" });

        // Rebuilding from the updated config reflects the new URL immediately.
        let source2 = ModelSource::from_config(&config);
        let (_, model2) = source2
            .instances()
            .into_iter()
            .find(|(id, _)| id == "granite-3.1-8b-instruct")
            .unwrap();
        assert_eq!(
            source2.provider_for(model2.as_ref()).unwrap().base_url(),
            "http://localhost:9999"
        );
    }

    #[test]
    fn model_source_skips_unknown_model_ids() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::Configured;

        let mut config = Config::default();
        config.models.insert(
            "not-a-real-model".to_string(),
            ModelConfig {
                model_id: "not-a-real-model".to_string(),
                provider_id: None,
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        assert!(source.instances().is_empty());
    }

    #[test]
    fn test_all_models_registered() {
        let models = MODEL_REGISTRY.entries();
        assert!(!models.is_empty(), "Expected models to be registered");
    }

    #[test]
    fn test_get_specific_model() {
        let model = MODEL_REGISTRY.get("granite-3.1-8b-instruct");
        assert!(
            model.is_some(),
            "granite-3.1-8b-instruct should be registered"
        );

        let metadata = model.unwrap();
        assert_eq!(metadata.family, "Granite Language");
        assert_eq!(metadata.version, "3.1");
        assert_eq!(metadata.context_length, 131072);
        assert_eq!(metadata.model_type, ModelType::Text);
    }

    #[test]
    fn test_model_variants() {
        let model = MODEL_REGISTRY.get("granite-3.1-8b-instruct").unwrap();
        assert!(
            !model.variants.is_empty(),
            "granite-3.1-8b-instruct should have variants"
        );

        // Check first variant
        let variant = &model.variants[0];
        assert!(!variant.format.is_empty());
        assert!(!variant.precision.is_empty());
        assert!(variant.size_gb > 0.0);
    }

    #[test]
    fn test_all_model_ids() {
        let models = MODEL_REGISTRY.entries();
        let ids: Vec<&str> = models.keys().copied().collect();

        assert!(ids.contains(&"granite-3.1-8b-instruct"));
        assert!(ids.contains(&"granite-guardian-3.1-8b"));
    }

    #[test]
    fn test_model_types() {
        let text_model = MODEL_REGISTRY.get("granite-3.1-8b-instruct").unwrap();
        assert_eq!(text_model.model_type, ModelType::Text);

        let vision_model = MODEL_REGISTRY.get("granite-vision-3.3-2b").unwrap();
        assert_eq!(vision_model.model_type, ModelType::Vision);

        let speech_model = MODEL_REGISTRY.get("granite-speech-4.1-2b").unwrap();
        assert_eq!(speech_model.model_type, ModelType::Speech);
    }

    #[test]
    fn test_model_supported_functions() {
        let text_model = MODEL_REGISTRY.get("granite-3.1-8b-instruct").unwrap();
        assert!(
            text_model
                .supported_functions
                .contains(&ModelFunction::Chat)
        );

        let vision_model = MODEL_REGISTRY.get("granite-vision-3.3-2b").unwrap();
        assert!(
            vision_model
                .supported_functions
                .contains(&ModelFunction::Chat)
        );
        assert!(
            vision_model
                .supported_functions
                .contains(&ModelFunction::ImageUnderstanding)
        );

        let speech_model = MODEL_REGISTRY.get("granite-speech-4.1-2b").unwrap();
        assert!(
            speech_model
                .supported_functions
                .contains(&ModelFunction::Chat)
        );
        assert!(
            speech_model
                .supported_functions
                .contains(&ModelFunction::Transcription)
        );
    }
}
