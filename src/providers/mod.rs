// Standard
use std::collections::HashMap;
use std::sync::LazyLock;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

use_channel!("PROVD");

/*-- Provider Registry -------------------------------------------------------*/

pub(crate) static PROVIDER_REGISTRY: LazyLock<base::ProviderFactory> = LazyLock::new(|| {
    let mut factory = base::ProviderFactory::new();
    factory.register::<openai::OpenAIProvider>("openai-compatible");
    factory.register::<ollama::OllamaProvider>("ollama");
    factory.register::<llamacpp::LlamaCppProvider>("llama-cpp");
    factory.register::<lmstudio::LMStudioProvider>("lm-studio");
    factory.register::<openrouter::OpenRouterProvider>("openrouter");
    factory
});

/*-- ProviderSource -----------------------------------------------------------*/

/// The real `Configured<dyn Provider>`: builds a live provider instance the
/// first time one is asked for by its instance nickname (`provider_id`)
/// rather than its catalog type (`provider_type`) -- this is what lets
/// multiple named instances of one catalog type (e.g. `openai-compatible`
/// backing `llama-cpp`, `ollama`, `lm-studio`) coexist. The instance is kept,
/// so every later ask for that id returns the same object.
pub(crate) struct ProviderSource {
    /// The settings for this kind, from the configuration snapshot the
    /// source was built from.
    configs: HashMap<String, crate::config::ProviderConfig>,
    /// When a launch passes a session proxy, every provider handed out by
    /// `get` points at it instead of the real upstream.
    model_proxy: Option<crate::proxy::ProxyHandle>,
    /// Providers as configured, carrying their real connection details.
    upstream: std::sync::Mutex<HashMap<String, std::sync::Arc<dyn Provider>>>,
    /// The same providers pointed at the session proxy, built only while one
    /// is running. Two views of one instance, so a launch can read a route's
    /// upstream target without a second source holding a second copy of
    /// every provider.
    proxied: std::sync::Mutex<HashMap<String, std::sync::Arc<dyn Provider>>>,
}

impl ProviderSource {
    /// Providers pointed at `model_proxy` when a launch started one.
    pub(crate) fn with_proxy(
        config: &crate::config::Config,
        model_proxy: Option<crate::proxy::ProxyHandle>,
    ) -> Self {
        Self {
            configs: config.providers.clone(),
            model_proxy,
            upstream: std::sync::Mutex::new(HashMap::new()),
            proxied: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The provider configured under `provider_id`, pointed at the session
    /// proxy when one is running, built on the first ask and returned from
    /// the cache on every one after it. Errors when no entry is configured
    /// under that id, or when its `provider_type` is not in the registry.
    pub(crate) fn get(&self, provider_id: &str) -> anyhow::Result<std::sync::Arc<dyn Provider>> {
        let upstream = self.upstream(provider_id)?;
        let Some(handle) = &self.model_proxy else {
            return Ok(upstream);
        };
        if let Some(built) = self.proxied.lock().unwrap().get(provider_id) {
            return Ok(built.clone());
        }
        let wrapped: std::sync::Arc<dyn Provider> = std::sync::Arc::new(
            crate::proxy::ProxiedProvider::wrap(upstream, handle.local_base_url.clone()),
        );
        Ok(self
            .proxied
            .lock()
            .unwrap()
            .entry(provider_id.to_string())
            .or_insert(wrapped)
            .clone())
    }

    /// The provider configured under `provider_id` carrying its real
    /// connection details, whether or not a session proxy is running. The
    /// launch reads a route's upstream target from here, since a provider
    /// handed out by `get` reports the proxy's own address.
    pub(crate) fn upstream(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<std::sync::Arc<dyn Provider>> {
        self.build(provider_id)
            .map_err(|e| e.about("provider", provider_id))
    }

    /// The same, as the typed failure the validator turns into a problem it
    /// reports. The instance stays in the cache, so a command that goes on
    /// to use it does not build it again.
    pub(crate) fn build(
        &self,
        provider_id: &str,
    ) -> Result<std::sync::Arc<dyn Provider>, crate::sources::SourceError> {
        if let Some(built) = self.upstream.lock().unwrap().get(provider_id) {
            return Ok(built.clone());
        }
        let provider_config = self
            .configs
            .get(provider_id)
            .ok_or(crate::sources::SourceError::NotConfigured)?;
        let built = PROVIDER_REGISTRY.construct(
            &provider_config.provider_type,
            &provider_config.provider_id,
            &provider_config.config,
        )?;
        let built: std::sync::Arc<dyn Provider> = std::sync::Arc::from(built);
        // Built outside the lock, so two callers can reach here for one id.
        // `or_insert` keeps whichever landed first and drops the other, so
        // the id has one instance however the calls interleave.
        Ok(self
            .upstream
            .lock()
            .unwrap()
            .entry(provider_id.to_string())
            .or_insert(built)
            .clone())
    }
}

impl crate::dependency::Configured<dyn Provider> for ProviderSource {
    fn instances(&self) -> Vec<(String, std::sync::Arc<dyn Provider + 'static>)> {
        self.configs
            .keys()
            .filter_map(|id| match self.get(id) {
                Ok(provider) => Some((id.clone(), provider)),
                Err(e) => {
                    alog_channel!(MessageLevel::Warning, "{e}");
                    None
                }
            })
            .collect()
    }

    fn catalog(&self) -> HashMap<&'static str, ProviderMetadata> {
        PROVIDER_REGISTRY.entries()
    }

    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
        PROVIDER_REGISTRY.config_schema(type_name)
    }
}

/*-- Model discovery ---------------------------------------------------------*/

/// Whether a provider instance's type config has discovery on, applying the
/// type's default when the field is absent. Reads configuration only, so a
/// provider whose discovery is off is never constructed (and no TLS client is
/// built for a hosted type that does not support discovery).
///
/// `provider_type` is the registry key, not the instance id.
pub(crate) fn discovery_enabled(provider_type: &str, config: &serde_json::Value) -> bool {
    config
        .get("discover_models")
        .and_then(|value| value.as_bool())
        .or_else(|| {
            PROVIDER_REGISTRY
                .default_config(provider_type)
                .get("discover_models")
                .and_then(|value| value.as_bool())
        })
        .unwrap_or(false)
}

/*-- Module Declarations -----------------------------------------------------*/

mod base;
pub(crate) use base::{
    ApiEndpoint, ApiType, HealthStatus, ModelFormat, Provider, ProviderError, ProviderMetadata,
    ProviderModel, ProviderType, PullResult,
};

mod openai;
#[cfg(test)]
pub(crate) use openai::OpenAIProvider;

mod ollama;

mod llamacpp;

mod lmstudio;

mod openrouter;

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

    /// Providers carrying their real connection details, for a test that
    /// needs no other kind. Commands ask the application context.
    fn source(config: &crate::config::Config) -> ProviderSource {
        ProviderSource::with_proxy(config, None)
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
    fn get_names_the_provider_whose_settings_cannot_be_read() {
        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "timeout_secs": "ten" }),
            },
        );

        let source = source(&config);
        let err = source.get("ollama").err().unwrap().to_string();
        assert!(
            err.contains("provider 'ollama'") && err.contains("invalid type"),
            "expected the instance and what serde said, got: {err}"
        );
    }

    #[test]
    fn get_names_the_provider_whose_type_is_unknown() {
        let mut config = Config::default();
        config.providers.insert(
            "mystery".to_string(),
            ProviderConfig {
                provider_id: "mystery".to_string(),
                provider_type: "not-a-provider-type".to_string(),
                config: serde_json::json!({}),
            },
        );

        let source = source(&config);
        let err = source.get("mystery").err().unwrap().to_string();
        assert!(
            err.contains("mystery") && err.contains("not-a-provider-type"),
            "expected the instance and the type it names, got: {err}"
        );
    }

    #[test]
    fn provider_source_constructs_one_instance_per_named_provider() {
        let config = config_with_two_named_instances();
        let source = source(&config);

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
        let source = source(&config);

        let resolution = resolve(&AnyGguf, &source);
        let mut ids = resolution.existing_instances;
        ids.sort();
        assert_eq!(ids, vec!["llama-cpp".to_string(), "ollama".to_string()]);
    }

    #[test]
    fn discovery_enabled_uses_the_type_default_when_the_field_is_absent() {
        // An Ollama config written before the field existed still gets the
        // local default (on), while types without the field stay off.
        assert!(discovery_enabled(
            "ollama",
            &serde_json::json!({ "base_url": "http://localhost:11434" })
        ));
        assert!(!discovery_enabled(
            "openai-compatible",
            &serde_json::json!({ "base_url": "http://localhost:8000" })
        ));
        assert!(!discovery_enabled("openrouter", &serde_json::json!({})));
    }

    #[test]
    fn discovery_enabled_honours_an_explicit_false() {
        assert!(!discovery_enabled(
            "ollama",
            &serde_json::json!({ "discover_models": false })
        ));
    }
}
