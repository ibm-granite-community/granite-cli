//! The sources for one configuration snapshot.
//!
//! They are built together and handed out together, so a name resolves to one
//! instance however it was reached: the model a capability resolves and the
//! model a command lists are the same object, and so is the provider behind
//! them. [`crate::AppContext`] owns one set and discards it whenever
//! configuration is written.

use std::sync::Arc;

use crate::capabilities::CapabilitySource;
use crate::config::Config;
use crate::launchers::LauncherSource;
use crate::models::ModelSource;
use crate::providers::ProviderSource;

/*-- public --*/

/// Why a source cannot hand out the instance a name points at.
///
/// The cases are kept apart so the validator can turn each into the problem
/// it reports, and so a caller can act on one without reading a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceError {
    /// Nothing is configured under the id asked for.
    NotConfigured,
    /// Something is configured, and building it failed.
    Construct(crate::registry::ConstructError),
    /// A capability built, and the model it names does not meet what its
    /// type requires.
    UnmetRequirement { model_id: String, unmet: String },
    /// A capability built, and a name it holds did not resolve.
    Unresolved(String),
}

impl SourceError {
    /// This failure as a message naming the instance it is about, for a
    /// source that knows which kind and id it was asked for. One wording for
    /// all four kinds, so the same problem reads the same whichever source
    /// reports it.
    pub(crate) fn about(&self, kind: &str, instance_id: &str) -> anyhow::Error {
        use crate::registry::ConstructError;
        match self {
            Self::NotConfigured => anyhow::anyhow!("{kind} '{instance_id}' is not configured"),
            Self::Construct(ConstructError::UnknownType { type_name }) => {
                anyhow::anyhow!("{kind} '{instance_id}' has an unknown {kind} type '{type_name}'")
            }
            Self::Construct(ConstructError::Settings { detail }) => {
                anyhow::anyhow!("the settings for {kind} '{instance_id}' are not valid: {detail}")
            }
            Self::UnmetRequirement { model_id, unmet } => anyhow::anyhow!(
                "{kind} '{instance_id}' names model '{model_id}', which does not meet its \
                 requirement: {unmet}"
            ),
            Self::Unresolved(detail) => anyhow::anyhow!("{kind} '{instance_id}': {detail}"),
        }
    }
}

impl From<crate::registry::ConstructError> for SourceError {
    fn from(error: crate::registry::ConstructError) -> Self {
        Self::Construct(error)
    }
}

/// One source per kind, wired so each asks the one below it: capabilities ask
/// models, models ask providers.
///
/// Cloning shares the same sources rather than building new ones, so a
/// caller can keep a handle for as long as it needs the snapshot it came
/// from.
#[derive(Clone)]
pub(crate) struct Sources {
    providers: Arc<ProviderSource>,
    models: Arc<ModelSource>,
    capabilities: Arc<CapabilitySource>,
    launchers: Arc<LauncherSource>,
}

impl Sources {
    /// Record `config` in each, constructing nothing. `model_proxy` is
    /// the session proxy a launch started, which every provider handed out by
    /// [`Sources::providers`] then points at.
    pub(crate) fn build(config: &Config, model_proxy: Option<crate::proxy::ProxyHandle>) -> Self {
        let providers = Arc::new(ProviderSource::with_proxy(config, model_proxy));
        let models = Arc::new(ModelSource::with_providers(config, Arc::clone(&providers)));
        let capabilities = Arc::new(CapabilitySource::with_models(config, Arc::clone(&models)));
        Self {
            providers,
            models,
            capabilities,
            launchers: Arc::new(LauncherSource::new(config)),
        }
    }

    pub(crate) fn providers(&self) -> Arc<ProviderSource> {
        Arc::clone(&self.providers)
    }

    pub(crate) fn models(&self) -> Arc<ModelSource> {
        Arc::clone(&self.models)
    }

    pub(crate) fn capabilities(&self) -> Arc<CapabilitySource> {
        Arc::clone(&self.capabilities)
    }

    pub(crate) fn launchers(&self) -> Arc<LauncherSource> {
        Arc::clone(&self.launchers)
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, ProviderConfig};
    use crate::utils::ui::base::tests::CaptureUi;
    use crate::{AppContext, proxy::ProxyServer};

    fn ctx_with_one_model() -> AppContext {
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
        AppContext::new(config, Arc::new(CaptureUi::default()))
    }

    #[test]
    fn two_asks_share_one_set_and_one_instance_per_id() {
        let ctx = ctx_with_one_model();

        let first = ctx.sources();
        let second = ctx.sources();
        assert!(Arc::ptr_eq(&first.models(), &second.models()));
        assert!(Arc::ptr_eq(&first.providers(), &second.providers()));

        let model = first.models().get("granite-3.1-8b-instruct").unwrap();
        let again = second.models().get("granite-3.1-8b-instruct").unwrap();
        assert!(Arc::ptr_eq(&model, &again));
    }

    #[test]
    fn a_capability_and_a_command_resolve_the_same_provider() {
        let ctx = ctx_with_one_model();
        let sources = ctx.sources();

        // What the models source resolves for a model, and what a command
        // reading the providers source asks for by id, are one object.
        let through_model = sources
            .models()
            .provider_for("granite-3.1-8b-instruct")
            .unwrap();
        let by_id = sources.providers().get("ollama").unwrap();
        assert!(Arc::ptr_eq(&through_model, &by_id));
    }

    #[test]
    fn a_write_ends_the_snapshot() {
        let mut ctx = ctx_with_one_model();
        let before = ctx.sources();
        let model_before = before.models().get("granite-3.1-8b-instruct").unwrap();

        ctx.config_mut()
            .models
            .remove("granite-3.1-8b-instruct")
            .unwrap();

        let after = ctx.sources();
        assert!(!Arc::ptr_eq(&before.models(), &after.models()));
        assert!(
            after.models().get("granite-3.1-8b-instruct").is_err(),
            "the new set reads the configuration as it now stands"
        );

        // What was handed out before the write still answers, against the
        // snapshot it came from. A launch holds its set this way.
        assert_eq!(model_before.instance_id(), "granite-3.1-8b-instruct");
        assert!(before.models().get("granite-3.1-8b-instruct").is_ok());
    }

    #[tokio::test]
    async fn a_session_proxy_ends_the_snapshot_too() {
        let mut ctx = ctx_with_one_model();
        let upstream = ctx
            .sources()
            .providers()
            .get("ollama")
            .unwrap()
            .base_url()
            .to_string();
        assert_eq!(upstream, "http://localhost:11434");

        let server = ProxyServer::start().unwrap();
        ctx.set_model_proxy(server.handle.clone());

        let providers = ctx.sources().providers();
        assert_eq!(
            providers.get("ollama").unwrap().base_url(),
            server.handle.local_base_url,
            "after a launch starts its proxy, every provider handed out points at it"
        );
        assert_eq!(
            providers.upstream("ollama").unwrap().base_url(),
            "http://localhost:11434",
            "the upstream view still carries the real details, for route targets"
        );

        server.shutdown().await;
    }
}
