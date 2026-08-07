use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use_channel!("CONF");

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub models: HashMap<String, ModelConfig>,
    pub providers: HashMap<String, ProviderConfig>,
    pub capabilities: HashMap<String, CapabilityConfig>,
    pub launchers: HashMap<String, LauncherConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    pub model_id: String,
    pub provider_id: Option<String>,
    pub variant: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub config: serde_json::Value,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            provider_type: String::new(),
            config: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityConfig {
    pub capability_id: String,
    #[serde(rename = "type")]
    pub capability_type: String,
    pub config: serde_json::Value,
}

impl Default for CapabilityConfig {
    fn default() -> Self {
        Self {
            capability_id: String::new(),
            capability_type: String::new(),
            config: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    pub launcher_id: String,
    #[serde(rename = "type")]
    pub launcher_type: String,
    /// Capability IDs enabled for this launcher instance.
    pub enabled_capabilities: Vec<String>,
    /// Launcher-type-specific config passed to `ConfigConstructable::new`.
    pub config: serde_json::Value,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            launcher_id: String::new(),
            launcher_type: String::new(),
            enabled_capabilities: vec![],
            config: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

impl Config {
    fn config_dir() -> Result<PathBuf> {
        let val_res = std::env::var("GRANITE_CLI_HOME");

        if let Ok(val) = val_res
            && !val.is_empty()
        {
            let path = PathBuf::from(&val);

            let has_valid_parent = path.parent().is_none_or(|p| p.exists());

            let valid_dir =
                (!path.exists() && has_valid_parent) || (path.exists() && path.is_dir());

            if !valid_dir {
                anyhow::bail!(
                    "Invalid GRANITE_CLI_HOME: '{val}' parent does not exist or is not a directory."
                );
            }

            return Ok(path);
        }

        let default_dir = dirs::config_dir().ok_or_else(|| {
            anyhow::Error::msg("Could not determine system configuration directory")
        })?;

        Ok(default_dir.join("granite-cli"))
    }

    fn models_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("models"))
    }

    fn providers_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("providers"))
    }

    fn capabilities_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("capabilities"))
    }

    fn launchers_dir() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("launchers"))
    }

    fn ensure_directories() -> Result<()> {
        let config_dir = Self::config_dir()?;
        if !config_dir.exists() {
            fs::create_dir_all(&config_dir)?;
        }
        fs::create_dir_all(Self::models_dir()?)?;
        fs::create_dir_all(Self::providers_dir()?)?;
        fs::create_dir_all(Self::capabilities_dir()?)?;
        fs::create_dir_all(Self::launchers_dir()?)?;
        Ok(())
    }

    fn load_yaml_from_file<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let config: T = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
        Ok(config)
    }

    fn save_yaml_to_file<T: serde::Serialize>(path: &Path, data: &T) -> Result<()> {
        let content = serde_yaml::to_string(data).with_context(|| "Failed to serialize config")?;
        fs::write(path, content)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;
        Ok(())
    }

    fn load_dir<K: std::hash::Hash + Eq + ToString, V: serde::de::DeserializeOwned>(
        dir: &Path,
        into_key: impl Fn(&str) -> K + Copy,
    ) -> Result<HashMap<K, V>> {
        let mut map = HashMap::new();
        if !dir.exists() {
            return Ok(map);
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "yaml") {
                let file_name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Ok(config) = Self::load_yaml_from_file::<V>(&path) {
                    map.insert(into_key(&file_name), config);
                }
            }
        }
        Ok(map)
    }

    pub fn new() -> Result<Self> {
        Self::ensure_directories()?;

        let mut config = Config::default();

        // Load component files
        let models_dir = &Self::models_dir()?;
        let providers_dir = &Self::providers_dir()?;
        let capabilities_dir = &Self::capabilities_dir()?;
        let launchers_dir = &Self::launchers_dir()?;
        alog_channel!(MessageLevel::Debug, "Models Dir: {:#?}", models_dir);
        alog_channel!(MessageLevel::Debug, "Providers Dir: {:#?}", providers_dir);
        alog_channel!(
            MessageLevel::Debug,
            "Capabilities Dir: {:#?}",
            capabilities_dir
        );
        alog_channel!(MessageLevel::Debug, "Launchers Dir: {:#?}", launchers_dir);
        config.models = Self::load_dir(models_dir, |s| s.to_string())?;
        config.providers = Self::load_dir(providers_dir, |s| s.to_string())?;
        config.capabilities = Self::load_dir(capabilities_dir, |s| s.to_string())?;
        config.launchers = Self::load_dir(launchers_dir, |s| s.to_string())?;

        Ok(config)
    }

    fn save(&self) -> Result<()> {
        // Save individual model files
        for (id, model) in &self.models {
            let path = Self::models_dir()?.join(format!("{id}.yaml"));
            Self::save_yaml_to_file(&path, model)?;
        }

        // Save individual provider files
        for (id, provider) in &self.providers {
            let path = Self::providers_dir()?.join(format!("{id}.yaml"));
            Self::save_yaml_to_file(&path, provider)?;
        }

        // Save individual capability files
        for (id, capability) in &self.capabilities {
            let path = Self::capabilities_dir()?.join(format!("{id}.yaml"));
            Self::save_yaml_to_file(&path, capability)?;
        }

        // Save individual launcher files
        for (id, launcher) in &self.launchers {
            let path = Self::launchers_dir()?.join(format!("{id}.yaml"));
            Self::save_yaml_to_file(&path, launcher)?;
        }

        Ok(())
    }

    // -- Model --

    pub fn get_model(&self, id: &str) -> Option<&ModelConfig> {
        self.models.get(id)
    }

    pub fn insert_model(&mut self, id: &str, config: ModelConfig) -> Result<()> {
        self.models.insert(id.to_string(), config);
        self.save()
    }

    pub fn remove_model(&mut self, id: &str) -> Result<()> {
        self.models.remove(id);
        let path = Self::models_dir().ok().and_then(|d| {
            let p = d.join(format!("{id}.yaml"));
            if p.exists() { Some(p) } else { None }
        });
        if let Some(p) = path {
            let _ = fs::remove_file(&p);
        }
        self.save()
    }

    pub fn update_model(&mut self, id: &str, f: impl FnOnce(&mut ModelConfig)) -> Result<()> {
        if let Some(model) = self.models.get_mut(id) {
            f(model);
            self.save()
        } else {
            Ok(())
        }
    }

    // -- Provider --

    pub fn get_provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.get(id)
    }

    pub fn insert_provider(&mut self, id: &str, config: ProviderConfig) -> Result<()> {
        self.providers.insert(id.to_string(), config);
        self.save()
    }

    pub fn remove_provider(&mut self, id: &str) -> Result<()> {
        self.providers.remove(id);
        let path = Self::providers_dir().ok().and_then(|d| {
            let p = d.join(format!("{id}.yaml"));
            if p.exists() { Some(p) } else { None }
        });
        if let Some(p) = path {
            let _ = fs::remove_file(&p);
        }
        self.save()
    }

    pub fn update_provider(&mut self, id: &str, f: impl FnOnce(&mut ProviderConfig)) -> Result<()> {
        if let Some(provider) = self.providers.get_mut(id) {
            f(provider);
            self.save()
        } else {
            Ok(())
        }
    }

    // -- Capability --

    pub fn get_capability(&self, id: &str) -> Option<&CapabilityConfig> {
        self.capabilities.get(id)
    }

    pub fn insert_capability(&mut self, id: &str, config: CapabilityConfig) -> Result<()> {
        self.capabilities.insert(id.to_string(), config);
        self.save()
    }

    pub fn remove_capability(&mut self, id: &str) -> Result<()> {
        self.capabilities.remove(id);
        let path = Self::capabilities_dir().ok().and_then(|d| {
            let p = d.join(format!("{id}.yaml"));
            if p.exists() { Some(p) } else { None }
        });
        if let Some(p) = path {
            let _ = fs::remove_file(&p);
        }
        self.save()
    }

    pub fn update_capability(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut CapabilityConfig),
    ) -> Result<()> {
        if let Some(capability) = self.capabilities.get_mut(id) {
            f(capability);
            self.save()
        } else {
            Ok(())
        }
    }

    // -- Launcher --

    pub fn get_launcher(&self, id: &str) -> Option<&LauncherConfig> {
        self.launchers.get(id)
    }

    pub fn insert_launcher(&mut self, id: &str, config: LauncherConfig) -> Result<()> {
        self.launchers.insert(id.to_string(), config);
        self.save()
    }

    pub fn remove_launcher(&mut self, id: &str) -> Result<()> {
        self.launchers.remove(id);
        let path = Self::launchers_dir().ok().and_then(|d| {
            let p = d.join(format!("{id}.yaml"));
            if p.exists() { Some(p) } else { None }
        });
        if let Some(p) = path {
            let _ = fs::remove_file(&p);
        }
        self.save()
    }

    pub fn update_launcher(&mut self, id: &str, f: impl FnOnce(&mut LauncherConfig)) -> Result<()> {
        if let Some(launcher) = self.launchers.get_mut(id) {
            f(launcher);
            self.save()
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn launcher_config_default_round_trips() {
        let original = LauncherConfig::default();
        let serialized = serde_yaml::to_string(&original).unwrap();
        let deserialized: LauncherConfig = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.launcher_id, original.launcher_id);
        assert_eq!(deserialized.launcher_type, original.launcher_type);
        assert!(deserialized.enabled_capabilities.is_empty());
    }

    #[test]
    fn insert_and_remove_launcher() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("granite-cli-test-2");
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::set_var("GRANITE_CLI_HOME", &home) };

        let mut config = Config::new().unwrap();
        let lc = LauncherConfig {
            launcher_id: "claude".to_string(),
            launcher_type: "claude".to_string(),
            enabled_capabilities: vec![],
            config: serde_json::json!({}),
        };
        config.insert_launcher("claude", lc).unwrap();
        assert!(config.get_launcher("claude").is_some());

        config.remove_launcher("claude").unwrap();
        assert!(config.get_launcher("claude").is_none());

        unsafe { std::env::remove_var("GRANITE_CLI_HOME") };
    }
}
