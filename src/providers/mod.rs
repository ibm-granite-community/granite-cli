// Standard
use std::collections::HashMap;
use std::sync::LazyLock;

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
pub struct ProviderSource {
    /// The configuration this source was built from. Only
    /// `config.providers` is read; `construct` takes the whole thing.
    config: crate::config::Config,
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
    /// Providers carrying their real connection details. This is what the
    /// launch path reads to register a route's upstream target, which has to
    /// happen before the proxy swap rather than from behind it.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::with_proxy(config, None)
    }

    /// Providers pointed at `model_proxy` when a launch started one.
    pub fn with_proxy(
        config: &crate::config::Config,
        model_proxy: Option<crate::proxy::ProxyHandle>,
    ) -> Self {
        Self {
            config: config.clone(),
            model_proxy,
            upstream: std::sync::Mutex::new(HashMap::new()),
            proxied: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The provider configured under `provider_id`, pointed at the session
    /// proxy when one is running, built on the first ask and returned from
    /// the cache on every one after it. Errors when no entry is configured
    /// under that id, or when its `provider_type` is not in the registry.
    pub fn get(&self, provider_id: &str) -> anyhow::Result<std::sync::Arc<dyn Provider>> {
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
    pub fn upstream(&self, provider_id: &str) -> anyhow::Result<std::sync::Arc<dyn Provider>> {
        if let Some(built) = self.upstream.lock().unwrap().get(provider_id) {
            return Ok(built.clone());
        }
        let provider_config = self
            .config
            .providers
            .get(provider_id)
            .ok_or_else(|| anyhow::anyhow!("provider '{provider_id}' is not configured"))?;
        let built = PROVIDER_REGISTRY
            .construct(
                &provider_config.provider_type,
                &provider_config.provider_id,
                &provider_config.config,
            )
            .map_err(|e| e.about("provider", provider_id))?;
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
        self.config
            .providers
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

mod openrouter;
pub use openrouter::{OpenRouterProvider, OpenRouterProviderConfig};

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

        let source = ProviderSource::from_config(&config);
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

        let source = ProviderSource::from_config(&config);
        let err = source.get("mystery").err().unwrap().to_string();
        assert!(
            err.contains("mystery") && err.contains("not-a-provider-type"),
            "expected the instance and the type it names, got: {err}"
        );
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
