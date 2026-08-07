use crate::capabilities::BindingType;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata};
use crate::registry::ConfigConstructable;
use crate::utils::resolve_shell_command;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct BobLauncherConfig {
    /// Override path to the `bob` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,
}

pub struct BobLauncher {
    config: BobLauncherConfig,
}

impl ConfigConstructable for BobLauncher {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: BobLauncherConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self { config }
    }
}

#[async_trait]
impl Launcher for BobLauncher {
    fn name(&self) -> &str {
        "Bob CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("bob")
    }

    fn supported_capabilities(&self) -> HashSet<BindingType> {
        Self::metadata().supported_capabilities
    }

    fn validate_command(&self) -> anyhow::Result<PathBuf> {
        resolve_shell_command(&self.config.command_path, "bob")
    }

    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        Ok(vec![])
    }
}

impl HasBobLauncherMetadata for BobLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Bob CLI".to_string(),
            description: "IBM Bob AI assistant CLI".to_string(),
            default_command: "bob".to_string(),
            supported_capabilities: HashSet::new(),
            tags: vec!["bob".to_string(), "ibm".to_string()],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(BobLauncherConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(BobLauncherConfig::default()).unwrap_or_default()
    }
}

/*-- private --*/

use crate::launchers::base::HasLauncherMetadata as HasBobLauncherMetadata;

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_to_bob() {
        let l = BobLauncher::new(&serde_json::json!({}));
        assert_eq!(l.command(), "bob");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = BobLauncher::new(&serde_json::json!({
            "command_path": "/opt/bin/bob"
        }));
        assert_eq!(l.command(), "/opt/bin/bob");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = BobLauncher::new(&serde_json::json!({
            "command_path": "/no/such/path/bob"
        }));
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = BobLauncher::new(&serde_json::json!({
            "command_path": "ls"
        }));
        assert!(l.validate_command().is_ok());
    }

    #[test]
    fn metadata_name_is_bob_cli() {
        let meta = BobLauncher::metadata();
        assert_eq!(meta.name, "Bob CLI");
        assert_eq!(meta.default_command, "bob");
    }

    #[test]
    fn config_schema_is_present() {
        let schema = BobLauncher::config_schema();
        let props = schema.get("properties").and_then(|p| p.as_object());
        assert!(props.is_some());
        assert!(props.unwrap().contains_key("command_path"));
    }
}
