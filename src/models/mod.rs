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
    model_proxy: Option<crate::proxy::ProxyHandle>,
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
            model_proxy: config.model_proxy.clone(),
        }
    }

    /// Removes and returns the constructed model for `model_id` (the catalog
    /// id -- matches `ModelConfig.model_id`, which config loading enforces
    /// equals the outer `config.models` key). `configured_variant` is the
    /// caller's already-resolved `"format/precision"` string, used (on the
    /// real, unwrapped provider) to compute the same alias
    /// `resolve_provider_endpoint` will use later, so the route registered
    /// here is keyed exactly how the launched process will address it.
    ///
    /// When a session proxy is active, registers this model's real
    /// connection details as a route on it (best-effort -- a registration
    /// failure is logged and the model is returned untracked/unrouted rather
    /// than failing construction over an accounting/routing feature) and
    /// returns a model wrapped to point at the proxy instead of the real
    /// upstream.
    pub fn take(
        &mut self,
        model_id: &str,
        configured_variant: Option<&str>,
    ) -> Option<Arc<dyn Model>> {
        let idx = self.constructed.iter().position(|(id, _)| id == model_id)?;
        let (_, model) = self.constructed.remove(idx);
        let model: Arc<dyn Model> = Arc::from(model);
        let Some(handle) = &self.model_proxy else {
            return Some(model);
        };
        match model.provider() {
            Ok(provider) => {
                let variant = base::find_variant(model.variants(), configured_variant);
                let route_key = provider
                    .model_alias(variant)
                    .unwrap_or_else(|| model_id.to_string());
                let target = crate::proxy::UpstreamTarget {
                    base_url: provider.base_url().to_string(),
                    verify_ssl: provider.verify_ssl(),
                    auth: crate::proxy::UpstreamAuth::Inject(provider.api_key().cloned()),
                };
                if let Err(e) = handle.register_route(route_key, target, model_id.to_string()) {
                    alog_channel!(
                        MessageLevel::Warning,
                        "failed to register proxy route for model '{model_id}': {e}"
                    );
                }
            }
            Err(e) => {
                alog_channel!(
                    MessageLevel::Warning,
                    "model '{model_id}' has no usable provider, skipping proxy route: {e}"
                );
            }
        }
        Some(Arc::new(crate::proxy::ProxiedModel::wrap(
            model,
            handle.local_base_url.clone(),
        )))
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
        assert!(source.take("granite-3.1-8b-instruct", None).is_some());
        assert!(source.take("granite-3.1-8b-instruct", None).is_none());
    }

    #[test]
    fn take_returns_none_for_unknown_model_id() {
        use crate::config::Config;

        let mut source = ModelSource::from_config(&Config::default());
        assert!(source.take("not-configured", None).is_none());
    }

    #[tokio::test]
    async fn take_routes_through_proxy_and_registers_a_route_when_a_handle_is_active() {
        use crate::config::{Config, ModelConfig, ProviderConfig};
        use crate::proxy::ProxyServer;

        async fn echo_model(body: axum::body::Bytes) -> axum::response::Response {
            use axum::response::IntoResponse;
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            axum::Json(serde_json::json!({ "model": value.get("model") })).into_response()
        }
        let app = axum::Router::new().route("/echo", axum::routing::post(echo_model));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "base_url": format!("http://{addr}") }),
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
        let server = ProxyServer::start().unwrap();
        config.model_proxy = Some(server.handle.clone());

        let mut source = ModelSource::from_config(&config);
        let model = source.take("granite-3.1-8b-instruct", None).unwrap();
        let provider = model.provider().unwrap();
        assert_eq!(provider.base_url(), server.handle.local_base_url);
        assert!(provider.api_key().is_none());

        // The real route (to the un-proxied fake upstream) was registered
        // under the model's catalog id (no alias, so it falls back to that)
        // -- prove it's actually live by round-tripping through the proxy.
        let client = reqwest::Client::new();
        let resp: serde_json::Value = client
            .post(format!("{}/echo", provider.base_url()))
            .json(&serde_json::json!({ "model": "granite-3.1-8b-instruct" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["model"], "granite-3.1-8b-instruct");

        server.shutdown().await;
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
