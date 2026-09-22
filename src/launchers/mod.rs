// Standard
use std::collections::HashMap;
use std::sync::LazyLock;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

use_channel!("LNCHR");

/*-- public --*/

pub static LAUNCHER_REGISTRY: LazyLock<base::LauncherFactory> = LazyLock::new(|| {
    let mut factory = base::LauncherFactory::new();
    factory.register::<claude::ClaudeLauncher>("claude");
    factory.register::<bob::BobLauncher>("bob");
    factory.register::<pi::PiLauncher>("pi");
    factory.register::<opencode::OpenCodeLauncher>("opencode");
    factory.register::<hermes::HermesLauncher>("hermes");
    factory.register::<goose::GooseLauncher>("goose");
    factory.register::<openclaw::OpenClawLauncher>("openclaw");
    factory
});

/*-- LauncherSource -----------------------------------------------------------*/

/// The real `Configured<dyn Launcher>`: builds a live launcher instance the
/// first time one is asked for by its instance nickname (`launcher_id`)
/// rather than its catalog type (`launcher_type`) -- this is what lets
/// multiple named instances of one catalog type coexist (e.g. `claude-local`
/// and `claude-enterprise` both backed by `claude`). The instance is kept, so
/// every later ask for that id returns the same object.
pub struct LauncherSource {
    /// The configuration this source was built from. Only
    /// `config.launchers` is read; `construct` takes the whole thing.
    config: crate::config::Config,
    cache: std::sync::Mutex<HashMap<String, std::sync::Arc<dyn Launcher>>>,
}

impl LauncherSource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            config: config.clone(),
            cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The launcher configured under `launcher_id`, built on the first ask
    /// and returned from the cache on every one after it. Errors when no
    /// entry is configured under that id, or when its `launcher_type` is not
    /// in the registry.
    pub fn get(&self, launcher_id: &str) -> anyhow::Result<std::sync::Arc<dyn Launcher>> {
        if let Some(built) = self.cache.lock().unwrap().get(launcher_id) {
            return Ok(built.clone());
        }
        let lc = self
            .config
            .launchers
            .get(launcher_id)
            .ok_or_else(|| anyhow::anyhow!("launcher '{launcher_id}' is not configured"))?;
        let built = LAUNCHER_REGISTRY
            .construct(&lc.launcher_type, &lc.launcher_id, &lc.config)
            .map_err(|e| e.about("launcher", launcher_id))?;
        let built: std::sync::Arc<dyn Launcher> = std::sync::Arc::from(built);
        // Built outside the lock, so two callers can reach here for one id.
        // `or_insert` keeps whichever landed first and drops the other, so
        // the id has one instance however the calls interleave.
        Ok(self
            .cache
            .lock()
            .unwrap()
            .entry(launcher_id.to_string())
            .or_insert(built)
            .clone())
    }
}

impl crate::dependency::Configured<dyn Launcher> for LauncherSource {
    fn instances(&self) -> Vec<(String, std::sync::Arc<dyn Launcher + 'static>)> {
        self.config
            .launchers
            .keys()
            .filter_map(|id| match self.get(id) {
                Ok(launcher) => Some((id.clone(), launcher)),
                Err(e) => {
                    alog_channel!(MessageLevel::Warning, "{e}");
                    None
                }
            })
            .collect()
    }

    fn catalog(&self) -> HashMap<&'static str, LauncherMetadata> {
        LAUNCHER_REGISTRY.entries()
    }

    fn config_schema(&self, type_name: &str) -> Option<schemars::Schema> {
        LAUNCHER_REGISTRY.config_schema(type_name)
    }
}

/*-- Module Declarations -----------------------------------------------------*/

mod base;
pub mod bob;
pub mod claude;
pub mod goose;
pub mod hermes;
pub mod openclaw;
pub mod opencode;
pub mod pi;

pub use base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata};
pub use bob::{BobLauncher, BobLauncherConfig};
pub use claude::{ClaudeLauncher, ClaudeLauncherConfig};
pub use goose::{GooseLauncher, GooseLauncherConfig};
pub use hermes::{HermesLauncher, HermesLauncherConfig};
pub use openclaw::{OpenClawLauncher, OpenClawLauncherConfig};
pub use opencode::{OpenCodeLauncher, OpenCodeLauncherConfig};
pub use pi::{PiLauncher, PiLauncherConfig};

/*-- private shared modules --*/

mod shared;

/*-- tests -------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LauncherConfig};
    use crate::dependency::Configured;

    fn config_with_launcher(id: &str, launcher_type: &str) -> Config {
        let mut config = Config::default();
        config.launchers.insert(
            id.to_string(),
            LauncherConfig {
                launcher_id: id.to_string(),
                launcher_type: launcher_type.to_string(),
                ..LauncherConfig::default()
            },
        );
        config
    }

    #[test]
    fn registry_contains_claude_bob_pi_and_opencode() {
        assert!(LAUNCHER_REGISTRY.get("claude").is_some());
        assert!(LAUNCHER_REGISTRY.get("bob").is_some());
        assert!(LAUNCHER_REGISTRY.get("pi").is_some());
        assert!(LAUNCHER_REGISTRY.get("opencode").is_some());
        assert!(LAUNCHER_REGISTRY.get("hermes").is_some());
        assert!(LAUNCHER_REGISTRY.get("goose").is_some());
        assert!(LAUNCHER_REGISTRY.get("openclaw").is_some());
        assert!(LAUNCHER_REGISTRY.get("nonexistent").is_none());
    }

    #[test]
    fn launcher_source_constructs_all_launchers() {
        let mut config = Config::default();
        config.launchers.insert(
            "my-claude".to_string(),
            LauncherConfig {
                launcher_id: "my-claude".to_string(),
                launcher_type: "claude".to_string(),
                ..LauncherConfig::default()
            },
        );
        config.launchers.insert(
            "my-bob".to_string(),
            LauncherConfig {
                launcher_id: "my-bob".to_string(),
                launcher_type: "bob".to_string(),
                ..LauncherConfig::default()
            },
        );

        let source = LauncherSource::from_config(&config);
        let ids: Vec<String> = source.instances().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn launcher_source_skips_unknown_types() {
        let config = config_with_launcher("mystery", "no-such-type");
        let source = LauncherSource::from_config(&config);
        assert!(source.instances().is_empty());
    }

    #[test]
    fn launcher_source_catalog_contains_all_registered_types() {
        let config = Config::default();
        let source = LauncherSource::from_config(&config);
        let catalog = source.catalog();
        assert!(catalog.contains_key("claude"));
        assert!(catalog.contains_key("bob"));
        assert!(catalog.contains_key("pi"));
        assert!(catalog.contains_key("opencode"));
        assert!(catalog.contains_key("hermes"));
        assert!(catalog.contains_key("goose"));
        assert!(catalog.contains_key("openclaw"));
    }

    #[test]
    fn launcher_source_config_schema_returns_schema_for_known_type() {
        let config = Config::default();
        let source = LauncherSource::from_config(&config);
        assert!(source.config_schema("claude").is_some());
        assert!(source.config_schema("bob").is_some());
        assert!(source.config_schema("pi").is_some());
        assert!(source.config_schema("opencode").is_some());
        assert!(source.config_schema("hermes").is_some());
        assert!(source.config_schema("goose").is_some());
        assert!(source.config_schema("openclaw").is_some());
        assert!(source.config_schema("nonexistent").is_none());
    }
}
