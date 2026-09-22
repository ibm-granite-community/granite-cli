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
    factory.register::<custom::CustomModel>("custom");
    factory
});

/*-- ModelSource ---------------------------------------------------------------*/

/// The real `Configured<dyn Model>`: builds a live model instance the first
/// time one is asked for by its instance id (`ModelConfig.model_id`) --
/// distinct from the registry key it's constructed from
/// (`ModelConfig.model_type`), so the same catalog type can be configured
/// more than once. The instance is kept, so every later ask for that id
/// returns the same object.
pub struct ModelSource {
    /// The configuration this source was built from. Only `config.models`
    /// is read; `construct` takes the whole thing.
    config: crate::config::Config,
    providers: Arc<crate::providers::ProviderSource>,
    cache: std::sync::Mutex<HashMap<String, Arc<dyn Model>>>,
}

impl ModelSource {
    /// Models whose providers carry their real connection details.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::with_proxy(config, None)
    }

    /// Models whose providers point at `model_proxy` when a launch started
    /// one, so a capability resolved against this source binds to the proxy.
    pub fn with_proxy(
        config: &crate::config::Config,
        model_proxy: Option<crate::proxy::ProxyHandle>,
    ) -> Self {
        Self {
            config: config.clone(),
            providers: Arc::new(crate::providers::ProviderSource::with_proxy(
                config,
                model_proxy,
            )),
            cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The provider the model configured under `model_id` names. Distinguishes
    /// a model that is not configured at all from one whose `provider_id`
    /// resolves to nothing, which `Model::provider()`'s single "model has no
    /// configured provider" could not express.
    pub fn provider_for(
        &self,
        model_id: &str,
    ) -> anyhow::Result<Arc<dyn crate::providers::Provider>> {
        let model_config = self
            .config
            .models
            .get(model_id)
            .ok_or_else(|| anyhow::anyhow!("model '{model_id}' is not configured"))?;
        self.providers.get(&model_config.provider_id).map_err(|_| {
            anyhow::anyhow!(
                "model '{model_id}' names provider '{}', which is not configured",
                model_config.provider_id
            )
        })
    }

    /// The same provider carrying its real connection details, whether or
    /// not a session proxy is running. The launch reads a route's upstream
    /// target from here.
    pub fn upstream_for(
        &self,
        model_id: &str,
    ) -> anyhow::Result<Arc<dyn crate::providers::Provider>> {
        let model_config = self
            .config
            .models
            .get(model_id)
            .ok_or_else(|| anyhow::anyhow!("model '{model_id}' is not configured"))?;
        self.providers
            .upstream(&model_config.provider_id)
            .map_err(|_| {
                anyhow::anyhow!(
                    "model '{model_id}' names provider '{}', which is not configured",
                    model_config.provider_id
                )
            })
    }

    /// The `"format/precision"` string the model configured under `model_id`
    /// was pinned to, if any.
    pub fn configured_variant(&self, model_id: &str) -> Option<String> {
        self.config
            .models
            .get(model_id)
            .and_then(|mc| mc.variant.clone())
    }

    /// The model configured under `model_id` (the instance id -- matches
    /// `ModelConfig.model_id`, which config loading enforces equals the outer
    /// `config.models` key), built on the first ask and returned from the
    /// cache on every one after it, so two capabilities naming one model
    /// share one object. Errors when no entry is configured under that id, or
    /// when its `model_type` is not in the registry.
    pub fn get(&self, model_id: &str) -> anyhow::Result<Arc<dyn Model>> {
        if let Some(built) = self.cache.lock().unwrap().get(model_id) {
            return Ok(built.clone());
        }
        let model_config = self
            .config
            .models
            .get(model_id)
            .ok_or_else(|| anyhow::anyhow!("model '{model_id}' is not configured"))?;

        let built = MODEL_REGISTRY
            .construct(
                &model_config.model_type,
                &model_config.model_id,
                &model_config.config,
            )
            .map_err(|e| e.about("model", model_id))?;

        let built: Arc<dyn Model> = Arc::from(built);
        // Built outside the lock, so two callers can reach here for one id.
        // `or_insert` keeps whichever landed first and drops the other, so
        // the id has one instance however the calls interleave.
        Ok(self
            .cache
            .lock()
            .unwrap()
            .entry(model_id.to_string())
            .or_insert(built)
            .clone())
    }
}

impl base::ModelLookup for ModelSource {
    fn resolve(
        &self,
        model_id: &str,
        requirement: Option<&crate::capabilities::ModelRequirement>,
    ) -> anyhow::Result<ConfiguredModel> {
        let model = self.get(model_id)?;
        if let Some(requirement) = requirement {
            use crate::dependency::Requirement;
            anyhow::ensure!(
                requirement.admits_instance(&*model),
                "model '{model_id}' does not satisfy what this capability requires of it: {}",
                describe_unmet(requirement, &*model)
            );
        }
        Ok(ConfiguredModel::new(
            model,
            self.provider_for(model_id)?,
            self.configured_variant(model_id),
        ))
    }
}

/// The parts of `requirement` this model does not meet, for an error that
/// says which one failed rather than that one did.
fn describe_unmet(
    requirement: &crate::capabilities::ModelRequirement,
    model: &dyn Model,
) -> String {
    let mut unmet: Vec<String> = Vec::new();
    if let Some(family) = &requirement.family
        && family != model.family()
    {
        unmet.push(format!("family '{family}'"));
    }
    if let Some(model_type) = &requirement.model_type
        && model_type != model.model_type()
    {
        unmet.push(format!("type {model_type:?}"));
    }
    if let Some(min) = requirement.min_context_length
        && model.context_length() < min
    {
        unmet.push(format!("context length at least {min}"));
    }
    if let Some(min) = requirement.min_size
        && model.size() < min
    {
        unmet.push(format!("size at least {min}"));
    }
    for tag in &requirement.tags {
        if !model.tags().contains(tag) {
            unmet.push(format!("tag '{tag}'"));
        }
    }
    for function in &requirement.supported_functions {
        if !model.supported_functions().contains(function) {
            unmet.push(format!("{function}"));
        }
    }
    if unmet.is_empty() {
        "nothing identifiable".to_string()
    } else {
        unmet.join(", ")
    }
}

impl crate::dependency::Configured<dyn Model> for ModelSource {
    fn instances(&self) -> Vec<(String, Arc<dyn Model + 'static>)> {
        self.config
            .models
            .keys()
            .filter_map(|id| match self.get(id) {
                Ok(model) => Some((id.clone(), model)),
                Err(e) => {
                    alog_channel!(MessageLevel::Warning, "{e}");
                    None
                }
            })
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
pub(crate) use base::find_variant;
pub use base::{
    ConfiguredModel, LayerKind, LayerTypeCount, MambaShape, Model, ModelArchitecture,
    ModelFunction, ModelLookup, ModelMetadata, ModelType, ModelVariant,
};

mod custom;
pub use custom::CustomModelConfig;

pub(crate) mod context_fit;
pub use context_fit::{ContextFit, required_gb};

pub mod huggingface;

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;

    /// Instance ids built so far, sorted. Lets a test tell what a call built
    /// from what it merely could have built, read straight off the cache: a
    /// child module sees its parent's private fields, so this needs no
    /// test-only accessor on `ModelSource` itself.
    fn cached_ids(source: &ModelSource) -> Vec<String> {
        let mut ids: Vec<String> = source.cache.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    #[test]
    fn model_source_constructs_one_instance_per_configured_model() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::Configured;

        let mut config = Config::default();
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
        config.models.insert(
            "granite-guardian-3.1-8b".to_string(),
            ModelConfig {
                model_id: "granite-guardian-3.1-8b".to_string(),
                model_type: "granite-guardian-3.1-8b".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
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
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        let provider = source.provider_for("granite-3.1-8b-instruct").unwrap();
        assert_eq!(provider.base_url(), "http://localhost:11434");
    }

    #[test]
    fn model_source_provider_errs_when_provider_id_unresolvable() {
        use crate::config::{Config, ModelConfig};

        let mut config = Config::default();
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "does-not-exist".to_string(),
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        let err = source
            .provider_for("granite-3.1-8b-instruct")
            .err()
            .expect("an unresolvable provider_id must not resolve")
            .to_string();
        assert!(
            err.contains("does-not-exist") && err.contains("granite-3.1-8b-instruct"),
            "the error must name both the model and the provider it points at, got: {err}"
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
                model_type: "not-a-real-model".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        assert!(source.instances().is_empty());
    }

    #[test]
    fn get_returns_the_same_instance_for_every_call() {
        use crate::config::{Config, ModelConfig};

        let mut config = Config::default();
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

        let source = ModelSource::from_config(&config);
        let first = source.get("granite-3.1-8b-instruct").unwrap();
        let second = source.get("granite-3.1-8b-instruct").unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second get must return the same object, not a rebuilt one"
        );
    }

    #[test]
    fn instances_hands_out_the_same_object_get_does() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::Configured;

        let mut config = Config::default();
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

        let source = ModelSource::from_config(&config);
        let from_instances = source
            .instances()
            .into_iter()
            .find(|(id, _)| id == "granite-3.1-8b-instruct")
            .map(|(_, model)| model)
            .unwrap();
        let from_get = source.get("granite-3.1-8b-instruct").unwrap();
        assert!(Arc::ptr_eq(&from_instances, &from_get));
    }

    #[test]
    fn get_errs_naming_an_id_that_is_not_configured() {
        use crate::config::Config;

        let source = ModelSource::from_config(&Config::default());
        let err = source
            .get("not-configured")
            .err()
            .expect("an unconfigured id must not resolve")
            .to_string();
        assert!(
            err.contains("not-configured"),
            "the error must name the id that was asked for, got: {err}"
        );
    }

    #[test]
    fn get_errs_for_a_configured_id_whose_type_is_unknown() {
        use crate::config::{Config, ModelConfig};

        let mut config = Config::default();
        config.models.insert(
            "mystery".to_string(),
            ModelConfig {
                model_id: "mystery".to_string(),
                model_type: "not-a-catalog-id".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );

        let source = ModelSource::from_config(&config);
        let err = source
            .get("mystery")
            .err()
            .expect("an unknown model_type must not construct")
            .to_string();
        assert!(err.contains("mystery"), "got: {err}");
    }

    #[test]
    fn get_builds_only_the_model_it_was_asked_for() {
        use crate::config::{Config, ModelConfig};

        let mut config = Config::default();
        for id in ["granite-3.1-8b-instruct", "granite-3.1-2b-instruct"] {
            config.models.insert(
                id.to_string(),
                ModelConfig {
                    model_id: id.to_string(),
                    model_type: id.to_string(),
                    config: serde_json::json!({}),
                    provider_id: "ollama".to_string(),
                    variant: None,
                },
            );
        }

        let source = ModelSource::from_config(&config);
        assert!(cached_ids(&source).is_empty(), "nothing is built up front");
        source.get("granite-3.1-8b-instruct").unwrap();
        assert_eq!(
            cached_ids(&source),
            vec!["granite-3.1-8b-instruct".to_string()],
            "asking for one model must not drag in the other"
        );
    }

    #[test]
    fn instances_omits_a_model_that_cannot_be_built_and_keeps_the_rest() {
        use crate::config::{Config, ModelConfig};
        use crate::dependency::Configured;

        let mut config = Config::default();
        for (id, model_type) in [
            ("granite-3.1-8b-instruct", "granite-3.1-8b-instruct"),
            ("broken", "not-a-catalog-id"),
        ] {
            config.models.insert(
                id.to_string(),
                ModelConfig {
                    model_id: id.to_string(),
                    model_type: model_type.to_string(),
                    config: serde_json::json!({}),
                    provider_id: "ollama".to_string(),
                    variant: None,
                },
            );
        }

        let source = ModelSource::from_config(&config);
        // The healthy model is reachable on its own, without the broken one
        // being touched at all.
        assert!(source.get("granite-3.1-8b-instruct").is_ok());
        assert_eq!(cached_ids(&source), vec!["granite-3.1-8b-instruct"]);

        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["granite-3.1-8b-instruct".to_string()]);
    }

    #[tokio::test]
    async fn a_proxied_source_hands_out_provider_details_pointed_at_the_proxy() {
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
                model_type: "granite-3.1-8b-instruct".to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        let server = ProxyServer::start().unwrap();

        let source = ModelSource::with_proxy(&config, Some(server.handle.clone()));

        // Registering the route is the launch path's job, so do here what
        // `register_proxy_routes` does there: read the real upstream details
        // through the source's upstream view, which the proxy swap does not
        // hide.
        let upstream = source.upstream_for("granite-3.1-8b-instruct").unwrap();
        assert_eq!(upstream.base_url(), format!("http://{addr}"));
        server
            .handle
            .register_route(
                "granite-3.1-8b-instruct".to_string(),
                crate::proxy::UpstreamTarget {
                    base_url: upstream.base_url().to_string(),
                    verify_ssl: upstream.verify_ssl(),
                    auth: crate::proxy::UpstreamAuth::Inject(upstream.api_key().cloned()),
                },
                "granite-3.1-8b-instruct".to_string(),
            )
            .unwrap();

        let provider = source.provider_for("granite-3.1-8b-instruct").unwrap();
        assert_eq!(provider.base_url(), server.handle.local_base_url);
        assert!(provider.api_key().is_none());

        // Round-trip through the proxy to prove the route is live and the
        // swapped details reach the real upstream.
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

    fn configured(model_id: &str) -> crate::config::Config {
        use crate::config::{Config, ModelConfig, ProviderConfig};
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
            model_id.to_string(),
            ModelConfig {
                model_id: model_id.to_string(),
                model_type: model_id.to_string(),
                config: serde_json::json!({}),
                provider_id: "ollama".to_string(),
                variant: None,
            },
        );
        config
    }

    #[test]
    fn resolve_refuses_a_model_that_does_not_meet_the_requirement() {
        use crate::capabilities::ModelRequirement;
        use base::ModelLookup;

        let config = configured("granite-3.1-8b-instruct");
        let source = ModelSource::from_config(&config);

        // The catalog model is a text model with no vision support.
        let wants_vision = ModelRequirement {
            supported_functions: vec![ModelFunction::ImageUnderstanding],
            ..Default::default()
        };
        let err = source
            .resolve("granite-3.1-8b-instruct", Some(&wants_vision))
            .err()
            .expect("a model that does not meet the requirement must not resolve")
            .to_string();
        assert!(
            err.contains("granite-3.1-8b-instruct") && err.contains("Image Understanding"),
            "the error must name the model and the part of the requirement it misses, got: {err}"
        );
    }

    #[test]
    fn resolve_accepts_the_same_model_for_a_dependent_that_does_not_require_it() {
        use crate::capabilities::ModelRequirement;
        use base::ModelLookup;

        let config = configured("granite-3.1-8b-instruct");
        let source = ModelSource::from_config(&config);

        assert!(
            source.resolve("granite-3.1-8b-instruct", None).is_ok(),
            "no requirement means nothing to fail"
        );
        let wants_chat = ModelRequirement {
            supported_functions: vec![ModelFunction::Chat],
            ..Default::default()
        };
        assert!(
            source
                .resolve("granite-3.1-8b-instruct", Some(&wants_chat))
                .is_ok(),
            "a requirement the model does meet must resolve"
        );
    }

    #[test]
    fn resolve_names_the_provider_when_it_is_the_provider_that_is_gone() {
        use base::ModelLookup;

        let mut config = configured("granite-3.1-8b-instruct");
        config.providers.clear();
        let source = ModelSource::from_config(&config);

        let err = source
            .resolve("granite-3.1-8b-instruct", None)
            .err()
            .expect("a model whose provider is gone must not resolve")
            .to_string();
        assert!(
            err.contains("ollama"),
            "the error must name the provider rather than stopping one hop short, got: {err}"
        );
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
