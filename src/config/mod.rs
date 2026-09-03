// Standard
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use_channel!("CONF");

const PATH_DELIM: &str = "---";

trait ConfigId {
    fn config_id(&self) -> &str;
}

impl ConfigId for ModelConfig {
    fn config_id(&self) -> &str {
        &self.model_id
    }
}

impl ConfigId for ProviderConfig {
    fn config_id(&self) -> &str {
        &self.provider_id
    }
}

impl ConfigId for CapabilityConfig {
    fn config_id(&self) -> &str {
        &self.capability_id
    }
}

impl ConfigId for LauncherConfig {
    fn config_id(&self) -> &str {
        &self.launcher_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub models: HashMap<String, ModelConfig>,
    pub providers: HashMap<String, ProviderConfig>,
    pub capabilities: HashMap<String, CapabilityConfig>,
    pub launchers: HashMap<String, LauncherConfig>,
    /// Ephemeral handle to the session-scoped model proxy for the current
    /// `launch` invocation, set whenever `-u`/`--usage-tracking` is enabled
    /// or a bound capability needs sub-agent routing. Never persisted --
    /// `Config` is saved as separate per-entry YAML files (see `save()`),
    /// never as a whole, so this field is simply skipped on both
    /// directions.
    #[serde(skip)]
    pub model_proxy: Option<crate::proxy::ProxyHandle>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    /// Instance id -- the config/file key this model is stored under.
    /// Defaults to `model_type` (see `commands::ModelCommands::setup`), but
    /// may differ to let the same catalog type be configured more than once
    /// (e.g. against different providers or precisions).
    pub model_id: String,
    /// Registry key: the catalog id this instance was constructed from (a
    /// `resources/models.yaml` id, or `"custom"`).
    pub model_type: String,
    pub provider_id: Option<String>,
    pub variant: Option<String>,
    /// Model-type-specific config (e.g. `CustomModelConfig`'s fields for a
    /// `"custom"` instance). `{}` for catalog models, which take no config
    /// beyond `provider_config`.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityConfig {
    pub capability_id: String,
    #[serde(rename = "type")]
    pub capability_type: String,
    pub config: serde_json::Value,
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

/*-- Windows / WSL ----------------------------------------------------------*/

/// Convert a Windows path string to a WSL path.
///
/// `C:\Users\gabel\AppData\Roaming\granite-cli\launchers` →
/// `/mnt/c/Users/gabel/AppData/Roaming/granite-cli/launchers`
#[allow(dead_code)]
fn windows_to_wsl(path: &str) -> Option<String> {
    let path = path.trim();
    // Match Windows drive letters: X:\... or X:/...
    let bytes = path.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || bytes[2] != b'\\' && bytes[2] != b'/' {
        return None;
    }
    let drive = &path[..1].to_lowercase();
    let rest = &path[2..];
    let normalized = rest.replace('\\', "/");
    Some(format!("/mnt/{drive}/{normalized}"))
}

/// Convert a WSL path to a Windows path.
///
/// `/mnt/c/Users/gabel/AppData/Roaming/granite-cli\launchers` →
/// `C:\Users\gabel\AppData\Roaming\granite-cli\launchers`
pub fn translate_wsl_to_windows(path: &str) -> Option<String> {
    let path = path.trim();
    if !path.starts_with("/mnt/") {
        return None;
    }
    let remainder = &path[5..];
    let mut parts = remainder.splitn(2, '/');
    let drive = parts.next()?.to_uppercase();
    let rest = parts.next()?;
    let normalized = rest.replace('/', "\\");
    Some(format!("{drive}:\\{normalized}"))
}

/// Recursively walk a serde_json::Value and translate path strings.
///
/// When `to_wsl` is true, converts Windows paths → WSL paths.
/// When `to_wsl` is false, converts WSL paths → Windows paths.
#[allow(dead_code)]
fn translate_paths_in_value(value: &serde_json::Value, to_wsl: bool) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let translated = if to_wsl {
                windows_to_wsl(s)
            } else {
                translate_wsl_to_windows(s)
            };
            match translated {
                Some(t) => serde_json::Value::String(t),
                None => value.clone(),
            }
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| translate_paths_in_value(v, to_wsl))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let new_map: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), translate_paths_in_value(v, to_wsl)))
                .collect();
            serde_json::Value::Object(new_map)
        }
        other => other.clone(),
    }
}

/// Translate all path strings in a serde_json::Value config object.
///
/// When running under WSL, Windows paths stored in config files are
/// converted to WSL paths so they work at runtime. On save, the reverse
/// conversion restores Windows-native paths for disk persistence.
#[cfg(not(windows))]
fn translate_paths_in_config(config: &mut serde_json::Value, to_wsl: bool) {
    config["command_path"] = translate_paths_in_value(&config["command_path"], to_wsl);
    if let Some(overrides) = config.get_mut("provider_overrides") {
        *overrides = translate_paths_in_value(overrides, to_wsl);
    }
    if let Some(overrides) = config.get_mut("model_overrides") {
        *overrides = translate_paths_in_value(overrides, to_wsl);
    }
    if let Some(overrides) = config.get_mut("base_path") {
        *overrides = translate_paths_in_value(overrides, to_wsl);
    }
}

/// No-op stub for native Windows builds where paths never need translation.
#[cfg(windows)]
fn translate_paths_in_config(_config: &mut serde_json::Value, _to_wsl: bool) {}

/// Trait for config types that carry a serde_json::Value config field.
/// Used to apply path translation generically across all config types.
trait ConfigPathTranslator {
    fn config_mut(&mut self) -> Option<&mut serde_json::Value>;
}

impl ConfigPathTranslator for ModelConfig {
    fn config_mut(&mut self) -> Option<&mut serde_json::Value> {
        Some(&mut self.config)
    }
}

impl ConfigPathTranslator for ProviderConfig {
    fn config_mut(&mut self) -> Option<&mut serde_json::Value> {
        Some(&mut self.config)
    }
}

impl ConfigPathTranslator for CapabilityConfig {
    fn config_mut(&mut self) -> Option<&mut serde_json::Value> {
        Some(&mut self.config)
    }
}

impl ConfigPathTranslator for LauncherConfig {
    fn config_mut(&mut self) -> Option<&mut serde_json::Value> {
        Some(&mut self.config)
    }
}

/*-- Core Config ------------------------------------------------------------*/

impl Config {
    /// Detect if running under Windows Subsystem for Linux.
    ///
    /// We check multiple indicators because PE binaries running under WSL
    /// may not have reliable access to `/proc/version` (the Windows libc
    /// runtime used by PE binaries doesn't translate `/proc` the same way
    /// Linux processes do). We prefer the `/proc/sys/kernel/osrelease` file
    /// which WSL consistently exposes, then fall back to `/proc/version`.
    fn is_wsl() -> bool {
        // Method 1: Check /proc/sys/kernel/osrelease (works for PE binaries under WSL)
        if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
            let lower = s.to_lowercase();
            if lower.contains("microsoft") || lower.contains("wsl") {
                return true;
            }
        }
        // Method 2: Check /proc/version (works for native Linux binaries)
        if let Ok(s) = std::fs::read_to_string("/proc/version") {
            let lower = s.to_lowercase();
            if lower.contains("microsoft") || lower.contains("wsl") {
                return true;
            }
        }
        // Method 3: Check for WSL_DISTRO_NAME environment variable
        std::env::var("WSL_DISTRO_NAME").is_ok()
    }

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

        // When running under WSL, always use the Windows AppData path so
        // config is shared between WSL and native Windows invocations,
        // regardless of how the binary was compiled. Under WSL the C: drive
        // is mounted at /mnt/c, so C:\Users\<user>\AppData\Roaming maps to
        // /mnt/c/Users/<user>/AppData/Roaming. We query the Windows username
        // via cmd.exe because it may differ from the WSL login name.
        //
        // The path format depends on the compile target: ELF binaries (Linux
        // builds) see WSL paths like `/mnt/c/...`, while PE binaries (Windows
        // builds) see native Windows paths like `C:\Users\...`.
        if Self::is_wsl() {
            let windows_username = std::process::Command::new("cmd.exe")
                .arg("/C")
                .arg("echo")
                .arg("%USERNAME%")
                .output()
                .ok()
                .and_then(|out| {
                    String::from_utf8(out.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                });

            if let Some(username) = windows_username {
                #[cfg(windows)]
                {
                    // PE binary: use native Windows path
                    let path = PathBuf::from("C:\\Users")
                        .join(&username)
                        .join("AppData")
                        .join("Roaming")
                        .join("granite-cli");
                    return Ok(path);
                }
                #[cfg(not(windows))]
                {
                    // ELF binary: use WSL mount path
                    let path = PathBuf::from("/mnt/c")
                        .join("Users")
                        .join(&username)
                        .join("AppData")
                        .join("Roaming")
                        .join("granite-cli");
                    return Ok(path);
                }
            }

            anyhow::bail!("Running under WSL but could not determine Windows username via cmd.exe");
        }

        let default_dir = dirs::config_dir().ok_or_else(|| {
            anyhow::Error::msg("Could not determine system configuration directory")
        })?;

        Ok(default_dir.join("granite-cli"))
    }

    /// Backstop against a test writing into the user's real global config:
    /// panics unless `GRANITE_CLI_HOME` has been pointed at an isolated
    /// directory (see `TestConfigHome`). Called at the top of every
    /// disk-mutating operation, never on the read-only path-resolution path.
    #[cfg(test)]
    fn assert_test_isolated() {
        let home = std::env::var("GRANITE_CLI_HOME").unwrap_or_default();
        assert!(
            !home.is_empty(),
            "Config write attempted during tests without GRANITE_CLI_HOME set -- \
             wrap the test body in `let _home = crate::config::TestConfigHome::new();` \
             so it never touches the real global config."
        );
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

    /// Directory a launcher may materialize generated state into -- config files
    /// it must put on disk for the tool it wraps. Kept under `GRANITE_CLI_HOME`
    /// so wrapping a tool never means editing that tool's own global config.
    ///
    /// The path is returned without being created; callers create it when they
    /// actually write, so a dry run leaves no trace.
    pub fn launcher_state_dir(launcher_id: &str) -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("launcher-state").join(launcher_id))
    }

    fn ensure_directories() -> Result<()> {
        #[cfg(test)]
        Self::assert_test_isolated();

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

    fn load_dir<
        K: std::hash::Hash + Eq + ToString,
        V: serde::de::DeserializeOwned + ConfigId + ConfigPathTranslator,
    >(
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
                if let Ok(mut config) = Self::load_yaml_from_file::<V>(&path) {
                    if let Some(cfg) = config.config_mut() {
                        translate_paths_in_config(cfg, true);
                    }
                    let id = config.config_id().to_string();
                    let file_id = Self::id_from_filename(&file_name);
                    if id != file_id {
                        let type_name = std::any::type_name::<V>();
                        alog_channel!(
                            MessageLevel::Warning,
                            "Found invalid config file {} with id \"{}\" (type: {})",
                            file_id,
                            id,
                            type_name
                        );
                    } else {
                        map.insert(into_key(&id), config);
                    }
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

    fn id_to_filename(id: &str) -> String {
        // NOTE: This replaces huggingface-style '/' delimiters which are not
        // platform-specific
        id.replace('/', PATH_DELIM)
    }

    fn id_from_filename(id: &str) -> String {
        // NOTE: This replaces huggingface-style '/' delimiters which are not
        // platform-specific
        id.replace(PATH_DELIM, "/")
    }

    fn save(&self) -> Result<()> {
        #[cfg(test)]
        Self::assert_test_isolated();

        // Save individual model files
        for (id, model) in &self.models {
            let mut model = model.clone();
            if let Some(cfg) = model.config_mut() {
                translate_paths_in_config(cfg, false);
            }
            let path = Self::models_dir()?.join(format!("{}.yaml", Self::id_to_filename(id)));
            alog_channel!(MessageLevel::Debug3, "Saving to {:#?}", path);
            Self::save_yaml_to_file(&path, &model)?;
        }

        // Save individual provider files
        for (id, provider) in &self.providers {
            let mut provider = provider.clone();
            if let Some(cfg) = provider.config_mut() {
                translate_paths_in_config(cfg, false);
            }
            let path = Self::providers_dir()?.join(format!("{}.yaml", Self::id_to_filename(id)));
            alog_channel!(MessageLevel::Debug3, "Saving to {:#?}", path);
            Self::save_yaml_to_file(&path, &provider)?;
        }

        // Save individual capability files
        for (id, capability) in &self.capabilities {
            let mut capability = capability.clone();
            if let Some(cfg) = capability.config_mut() {
                translate_paths_in_config(cfg, false);
            }
            let path = Self::capabilities_dir()?.join(format!("{}.yaml", Self::id_to_filename(id)));
            alog_channel!(MessageLevel::Debug3, "Saving to {:#?}", path);
            Self::save_yaml_to_file(&path, &capability)?;
        }

        // Save individual launcher files
        for (id, launcher) in &self.launchers {
            let mut launcher = launcher.clone();
            if let Some(cfg) = launcher.config_mut() {
                translate_paths_in_config(cfg, false);
            }
            let path = Self::launchers_dir()?.join(format!("{}.yaml", Self::id_to_filename(id)));
            alog_channel!(MessageLevel::Debug3, "Saving to {:#?}", path);
            Self::save_yaml_to_file(&path, &launcher)?;
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
            let p = d.join(format!("{}.yaml", Self::id_to_filename(id)));
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
            let p = d.join(format!("{}.yaml", Self::id_to_filename(id)));
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
            let p = d.join(format!("{}.yaml", Self::id_to_filename(id)));
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
            let p = d.join(format!("{}.yaml", Self::id_to_filename(id)));
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

/*-- Tests -- */

#[cfg(test)]
impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            provider_type: String::new(),
            config: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[cfg(test)]
impl Default for CapabilityConfig {
    fn default() -> Self {
        Self {
            capability_id: String::new(),
            capability_type: String::new(),
            config: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

#[cfg(test)]
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

/// Serializes access to `GRANITE_CLI_HOME`, which every `TestConfigHome`
/// mutates -- it's process-global env state shared across concurrently
/// running tests.
#[cfg(test)]
static CONFIG_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Points `GRANITE_CLI_HOME` at a fresh temp directory for the guard's
/// lifetime and restores it on drop -- including on panic or early return --
/// so `Config` mutations in tests can never land in the user's real global
/// config. Holds `CONFIG_HOME_LOCK` the whole time so concurrent tests using
/// this guard never race each other over the shared env var.
#[cfg(test)]
pub(crate) struct TestConfigHome {
    _tmp: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl TestConfigHome {
    pub(crate) fn new() -> Self {
        let guard = CONFIG_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialized by CONFIG_HOME_LOCK, held for the guard's lifetime.
        unsafe { std::env::set_var("GRANITE_CLI_HOME", tmp.path()) };
        Self {
            _tmp: tmp,
            _guard: guard,
        }
    }
}

#[cfg(test)]
impl Drop for TestConfigHome {
    fn drop(&mut self) {
        // SAFETY: still holding CONFIG_HOME_LOCK.
        unsafe { std::env::remove_var("GRANITE_CLI_HOME") };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let _home = TestConfigHome::new();

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
    }
}
