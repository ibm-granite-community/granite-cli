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

/// The real `Configured<dyn Model>`: eagerly constructs a live model instance
/// for every model referenced by the config's `models` map, keyed by its
/// catalog id (models have no separate instance nickname -- the config key
/// *is* the catalog id).
pub struct ModelSource {
    constructed: Vec<(String, Box<dyn Model>)>,
    usage_tracking: Option<crate::proxy::UsageTrackingContext>,
}

impl ModelSource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let constructed = config
            .models
            .values()
            .filter_map(|model_config| {
                let cfg = match model_config
                    .provider_id
                    .as_deref()
                    .and_then(|pid| config.get_provider(pid))
                {
                    Some(provider_config) => {
                        serde_json::json!({ "provider_config": provider_config })
                    }
                    None => serde_json::json!({}),
                };
                let result = MODEL_REGISTRY.construct(
                    &model_config.model_id,
                    &model_config.model_id,
                    &cfg,
                    config,
                );
                if result.is_err() {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Could not construct model '{}'",
                        model_config.model_id
                    );
                }
                result
                    .ok()
                    .map(|model| (model_config.model_id.clone(), model))
            })
            .collect();
        Self {
            constructed,
            usage_tracking: config.usage_tracking.clone(),
        }
    }

    /// Removes and returns the constructed model for `model_id` (the catalog
    /// id -- matches `ModelConfig.model_id`, which config loading enforces
    /// equals the outer `config.models` key). When a usage-tracking session
    /// is active, wraps the model in a local tracking proxy first; if the
    /// proxy fails to start, falls back to the untracked model with a
    /// warning rather than failing construction over an accounting feature.
    pub fn take(&mut self, model_id: &str) -> Option<Arc<dyn Model>> {
        let idx = self.constructed.iter().position(|(id, _)| id == model_id)?;
        let (_, model) = self.constructed.remove(idx);
        let model: Arc<dyn Model> = Arc::from(model);
        let Some(ctx) = &self.usage_tracking else {
            return Some(model);
        };
        match crate::proxy::UsageTrackingModel::wrap(
            Arc::clone(&model),
            model_id.to_string(),
            ctx.clone(),
        ) {
            Ok(wrapped) => Some(Arc::new(wrapped)),
            Err(e) => {
                alog_channel!(
                    MessageLevel::Warning,
                    "usage-tracking proxy failed to start for model '{model_id}', continuing untracked: {e}"
                );
                Some(model)
            }
        }
    }
}

impl crate::dependency::Configured<dyn Model> for ModelSource {
    fn instances(&self) -> Vec<(String, &(dyn Model + 'static))> {
        self.constructed
            .iter()
            .map(|(id, model)| (id.clone(), model.as_ref()))
            .collect()
    }

    fn catalog(&self) -> HashMap<&'static str, ModelMetadata> {
        MODEL_REGISTRY.entries()
    }

    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
        MODEL_REGISTRY.config_schema(type_name)
    }
}

// Re-export types from base
mod base;
pub use base::{
    ConfiguredModel, LayerKind, LayerTypeCount, MambaShape, Model, ModelArchitecture,
    ModelFunction, ModelMetadata, ModelType, ModelVariant,
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
        use crate::dependency::Configured;

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
        let provider = model.provider().unwrap();
        assert_eq!(provider.base_url(), "http://localhost:11434");
    }

    #[test]
    fn model_source_provider_errs_when_provider_id_unresolvable() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::Configured;

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
        assert!(model.provider().is_err());
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
    fn take_removes_and_returns_configured_model() {
        use crate::config::{Config, ModelConfig};

        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                provider_id: None,
                variant: None,
            },
        );

        let mut source = ModelSource::from_config(&config);
        assert!(source.take("granite-3.1-8b-instruct").is_some());
        assert!(source.take("granite-3.1-8b-instruct").is_none());
    }

    #[test]
    fn take_returns_none_for_unknown_model_id() {
        use crate::config::Config;

        let mut source = ModelSource::from_config(&Config::default());
        assert!(source.take("not-configured").is_none());
    }

    #[tokio::test]
    async fn take_wraps_model_when_usage_tracking_is_active() {
        use crate::config::{Config, ModelConfig, ProviderConfig};
        use crate::proxy::{ProxyServer, UsageTracker, UsageTrackingContext};
        use std::sync::Mutex;

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
        let servers: Arc<Mutex<Vec<ProxyServer>>> = Arc::new(Mutex::new(Vec::new()));
        config.usage_tracking = Some(UsageTrackingContext {
            tracker: Arc::new(UsageTracker::new()),
            servers: Arc::clone(&servers),
        });

        let mut source = ModelSource::from_config(&config);
        let model = source.take("granite-3.1-8b-instruct").unwrap();
        let provider = model.provider().unwrap();
        assert!(provider.base_url().starts_with("http://127.0.0.1:"));

        let started: Vec<_> = servers.lock().unwrap().drain(..).collect();
        assert_eq!(started.len(), 1);
        for server in started {
            server.shutdown().await;
        }
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
        assert!(variant.size_gb.unwrap_or(0.0) > 0.0);
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
