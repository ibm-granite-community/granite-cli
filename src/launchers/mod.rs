// Standard
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};

use_channel!("LNCHR");

/*-- public --*/

pub static LAUNCHER_REGISTRY: LazyLock<base::LauncherFactory> = LazyLock::new(|| {
    let mut factory = base::LauncherFactory::new();
    factory.register::<claude::ClaudeLauncher>("claude");
    factory.register::<bob::BobLauncher>("bob");
    factory
});

/*-- LauncherSource -----------------------------------------------------------*/

/// The real `Configured<dyn Launcher>`: eagerly constructs a live launcher
/// instance for every enabled `LauncherConfig`, keyed by its instance
/// nickname (`launcher_id`) rather than its catalog type (`launcher_type`) --
/// this is what lets multiple named instances of one catalog type coexist
/// (e.g. `claude-local` and `claude-enterprise` both backed by `claude`).
pub struct LauncherSource {
    constructed: Vec<(String, Arc<dyn Launcher>)>,
}

impl LauncherSource {
    pub fn from_config(config: &crate::config::Config) -> Self {
        let constructed = config
            .launchers
            .values()
            .filter_map(|lc| {
                let result = LAUNCHER_REGISTRY.construct_shared(&lc.launcher_type, &lc.config);
                if result.is_err() {
                    alog_channel!(
                        MessageLevel::Warning,
                        "Could not construct launcher '{}'",
                        lc.launcher_type
                    );
                }
                result
                    .ok()
                    .map(|arc| (lc.launcher_id.clone(), arc))
            })
            .collect();
        Self { constructed }
    }
}

impl crate::dependency::Configured<dyn Launcher> for LauncherSource {
    fn instances(&self) -> Vec<(String, Arc<dyn Launcher + 'static>)> {
        self.constructed
            .iter()
            .map(|(id, arc)| (id.clone(), Arc::clone(arc)))
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

pub use base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata};
pub use bob::{BobLauncher, BobLauncherConfig};
pub use claude::{ClaudeLauncher, ClaudeLauncherConfig};

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
    fn registry_contains_claude_and_bob() {
        assert!(LAUNCHER_REGISTRY.get("claude").is_some());
        assert!(LAUNCHER_REGISTRY.get("bob").is_some());
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
    }

    #[test]
    fn launcher_source_config_schema_returns_schema_for_known_type() {
        let config = Config::default();
        let source = LauncherSource::from_config(&config);
        assert!(source.config_schema("claude").is_some());
        assert!(source.config_schema("bob").is_some());
        assert!(source.config_schema("nonexistent").is_none());
    }
}
