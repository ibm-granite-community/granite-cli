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
pub struct ClaudeLauncherConfig {
    /// Override path to the `claude` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,
}

pub struct ClaudeLauncher {
    config: ClaudeLauncherConfig,
}

impl ConfigConstructable for ClaudeLauncher {
    fn new(cfg: &serde_json::Value) -> Self {
        let config: ClaudeLauncherConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self { config }
    }
}

#[async_trait]
impl Launcher for ClaudeLauncher {
    fn name(&self) -> &str {
        "Claude CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("claude")
    }

    fn supported_capabilities(&self) -> HashSet<BindingType> {
        Self::metadata().supported_capabilities
    }

    fn validate_command(&self) -> anyhow::Result<PathBuf> {
        resolve_shell_command(&self.config.command_path, "claude")
    }

    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        Ok(vec![])
    }
}

impl HasClaudeLauncherMetadata for ClaudeLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Claude CLI".to_string(),
            description: "Anthropic's Claude CLI tool".to_string(),
            default_command: "claude".to_string(),
            supported_capabilities: HashSet::from([BindingType::AgentModel]),
            tags: vec!["claude".to_string(), "anthropic".to_string()],
        }
    }

    fn config_schema() -> schemars::Schema {
        schemars::schema_for!(ClaudeLauncherConfig)
    }

    fn default_config() -> serde_json::Value {
        serde_json::to_value(ClaudeLauncherConfig::default()).unwrap_or_default()
    }
}

/*-- private --*/

// HasClaudeLauncherMetadata is the macro-generated trait; re-exported via mod.rs.
use crate::launchers::base::HasLauncherMetadata as HasClaudeLauncherMetadata;

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_to_claude() {
        let l = ClaudeLauncher::new(&serde_json::json!({}));
        assert_eq!(l.command(), "claude");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = ClaudeLauncher::new(&serde_json::json!({
            "command_path": "/opt/bin/claude"
        }));
        assert_eq!(l.command(), "/opt/bin/claude");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = ClaudeLauncher::new(&serde_json::json!({
            "command_path": "/no/such/path/claude"
        }));
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = ClaudeLauncher::new(&serde_json::json!({
            "command_path": "ls"
        }));
        assert!(l.validate_command().is_ok());
    }

    #[test]
    fn metadata_name_is_claude_cli() {
        let meta = ClaudeLauncher::metadata();
        assert_eq!(meta.name, "Claude CLI");
        assert_eq!(meta.default_command, "claude");
    }

    #[test]
    fn config_schema_is_present() {
        let schema = ClaudeLauncher::config_schema();
        // Schema should reference ClaudeLauncherConfig properties
        let props = schema.get("properties").and_then(|p| p.as_object());
        assert!(props.is_some());
        assert!(props.unwrap().contains_key("command_path"));
    }
}
