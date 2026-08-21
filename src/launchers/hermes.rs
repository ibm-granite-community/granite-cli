//! Launcher for the `hermes` agent CLI (<https://github.com/NousResearch/hermes-agent>).
//!
//! Hermes reads everything (model endpoint, MCP servers, credentials) from
//! a single home directory based on HERMES_HOME. It has no additive config mechanism
//! and the CLI is interactive, so this launcher follows the same strategy as pi: build a
//! granite-cli-owned Hermes home under GRANITE_CLI_HOME with config.yaml
//! generated fresh from the bound model and MCP servers, everything else
//! linked through from the user's directory (including .env, so their
//! credentials and logins still apply) and point the child process at it
//! with HERMES_HOME. The generated model section selects the bound model
//! via provider: custom, which Hermes documents for any OpenAI-compatible
//! endpoint; the api_key is written as an ${ENV_VAR} reference that
//! Hermes expands at load time, so the secret stays out of the file.

use crate::capabilities::{AgentModelBinding, Binding, BindingType, Capability, McpBinding};
use crate::launchers::base::HasLauncherMetadata as HasHermesLauncherMetadata;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::mcp_binding_request;
use crate::providers::ApiType;
use crate::registry::ConfigConstructable;
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use_channel!("LNCHR");

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct HermesLauncherConfig {
    /// Override path to the `hermes` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Extra keys merged (shallow, last-write-wins) into the generated
    /// `model` section -- e.g. `max_tokens` or `temperature`. Necessary
    /// because the section is regenerated on every launch.
    #[serde(default)]
    pub model_overrides: Option<serde_json::Value>,
}

pub struct HermesLauncher {
    instance_id: String,
    config: HermesLauncherConfig,
    bound_agent_model: Option<AgentModelBinding>,
    bound_mcp_bindings: Vec<(String, McpBinding)>,
}

impl ConfigConstructable for HermesLauncher {
    type Config = HermesLauncherConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        _global_config: &crate::config::Config,
    ) -> Self {
        let config: HermesLauncherConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self {
            instance_id: instance_id.to_string(),
            config,
            bound_agent_model: None,
            bound_mcp_bindings: vec![],
        }
    }
}

impl crate::registry::Named for HermesLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for HermesLauncher {
    fn name(&self) -> &str {
        "Hermes CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("hermes")
    }

    async fn bind_capability(&mut self, capability: &dyn Capability) -> anyhow::Result<()> {
        let supported = Self::metadata().supported_capabilities;
        let capability_types = capability.binding_types();
        if !capability_types.is_subset(&supported) {
            anyhow::bail!(
                "capability supports {:?} which this launcher does not support",
                capability_types.difference(&supported).collect::<Vec<_>>()
            );
        }

        if capability_types.contains(&BindingType::Mcp) {
            let binding = capability.bind(mcp_binding_request()).await?;
            match binding {
                Binding::Mcp(binding) => {
                    self.bound_mcp_bindings
                        .push((capability.instance_id().to_string(), binding));
                }
                other => anyhow::bail!("expected an Mcp binding, got {:?}", other.binding_type()),
            }
            return Ok(());
        }

        let request = crate::capabilities::BindingRequest::AgentModel(
            crate::capabilities::AgentModelBindingRequest {
                api_type: ApiType::OpenAI,
            },
        );

        let binding = capability.bind(request).await?;
        match binding {
            Binding::AgentModel(binding) => {
                self.bound_agent_model = Some(binding);
            }
            other => anyhow::bail!(
                "expected an AgentModel binding, got {:?}",
                other.binding_type()
            ),
        }
        Ok(())
    }

    fn validate_command(&self) -> anyhow::Result<PathBuf> {
        resolve_shell_command(&self.config.command_path, "hermes")
    }

    async fn env_overlay(&self, ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        if self.bound_agent_model.is_none() && self.bound_mcp_bindings.is_empty() {
            return Ok(vec![]);
        }
        let mut overlay = vec![EnvBinding {
            key: CONFIG_DIR_ENV.to_string(),
            value: hermes_state_dir(ctx)?.to_string_lossy().to_string(),
        }];
        if let Some(api_key) = self
            .bound_agent_model
            .as_ref()
            .and_then(|binding| binding.api_key.as_ref())
            .map(|api_key| api_key.0.clone())
            .filter(|key| !key.is_empty())
        {
            overlay.push(EnvBinding {
                key: API_KEY_ENV.to_string(),
                value: api_key,
            });
        }
        Ok(overlay)
    }

    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;
        alog_channel!(MessageLevel::Debug2, "Env Overlay: {:#?}", overlay);

        if self.bound_agent_model.is_some() || !self.bound_mcp_bindings.is_empty() {
            let model = self
                .bound_agent_model
                .as_ref()
                .map(|binding| self.model_section(binding))
                .transpose()?;
            let mcp_servers = self
                .bound_mcp_bindings
                .iter()
                .map(|(name, binding)| Ok((name.clone(), mcp_server_entry(binding)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let state_dir = hermes_state_dir(ctx)?;
            let source_dir = hermes_source_dir()?;

            if ctx.dry_run {
                ui.info(&format!(
                    "Would write Hermes config to {}:",
                    state_dir.join(CONFIG_YAML).display()
                ));
                let mut preview = serde_yaml::Mapping::new();
                if let Some(model) = &model {
                    preview.insert("model".into(), model.clone());
                }
                if !mcp_servers.is_empty() {
                    let mut servers = serde_yaml::Mapping::new();
                    for (name, entry) in &mcp_servers {
                        servers.insert(name.clone().into(), entry.clone());
                    }
                    preview.insert("mcp_servers".into(), serde_yaml::Value::Mapping(servers));
                }
                ui.info(&serde_yaml::to_string(&preview)?);
                ui.info(&format!(
                    "  (merged over {}, which is left unmodified)",
                    source_dir.join(CONFIG_YAML).display()
                ));
            } else {
                materialize_hermes_config(&state_dir, &source_dir, model, &mcp_servers, ui)?;
                ui.info(&format!(
                    "Wrote Hermes config to {}",
                    state_dir.join(CONFIG_YAML).display()
                ));
            }
        }

        run_command(binary, &overlay, args, ctx, ui).await
    }
}

impl HasHermesLauncherMetadata for HermesLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Hermes CLI".to_string(),
            description: "Nous Research's Hermes agent CLI".to_string(),
            default_command: "hermes".to_string(),
            supported_capabilities: HashSet::from([BindingType::AgentModel, BindingType::Mcp]),
            tags: vec!["hermes".to_string(), "nous".to_string()],
        }
    }
}

/*-- private --*/

const CONFIG_DIR_ENV: &str = "HERMES_HOME";

const API_KEY_ENV: &str = "GRANITE_CLI_HERMES_API_KEY";

const CONFIG_YAML: &str = "config.yaml";

const STATE_DIRS: &[&str] = &["sessions", "logs", "cache"];

impl HermesLauncher {
    fn model_section(&self, binding: &AgentModelBinding) -> anyhow::Result<serde_yaml::Value> {
        let mut section = serde_yaml::Mapping::new();
        section.insert("default".into(), binding.model_name.clone().into());
        section.insert("provider".into(), "custom".into());
        section.insert("base_url".into(), hermes_base_url(binding).into());
        section.insert("context_length".into(), binding.context_length.into());
        if binding
            .api_key
            .as_ref()
            .is_some_and(|key| !key.0.is_empty())
        {
            section.insert("api_key".into(), format!("${{{API_KEY_ENV}}}").into());
        }

        if let Some(overrides) = self
            .config
            .model_overrides
            .as_ref()
            .and_then(serde_json::Value::as_object)
        {
            for (key, value) in overrides {
                section.insert(key.clone().into(), serde_yaml::to_value(value)?);
            }
        }
        Ok(serde_yaml::Value::Mapping(section))
    }
}

fn mcp_server_entry(binding: &McpBinding) -> anyhow::Result<serde_yaml::Value> {
    let mut entry = serde_yaml::Mapping::new();
    match binding {
        McpBinding::Stdio { command, args, env } => {
            entry.insert("command".into(), command.clone().into());
            entry.insert("args".into(), serde_yaml::to_value(args)?);
            if !env.is_empty() {
                entry.insert("env".into(), serde_yaml::to_value(env)?);
            }
        }
        McpBinding::Http { url, headers } | McpBinding::Sse { url, headers } => {
            entry.insert("url".into(), url.clone().into());
            if !headers.is_empty() {
                entry.insert("headers".into(), serde_yaml::to_value(headers)?);
            }
        }
    }
    Ok(serde_yaml::Value::Mapping(entry))
}

fn hermes_base_url(binding: &AgentModelBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = binding
        .endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

fn hermes_state_dir(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    crate::config::Config::launcher_state_dir(&ctx.launcher_id)
}

fn hermes_source_dir() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var(CONFIG_DIR_ENV)
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .map(|dir| dir.join("hermes"))
            .ok_or_else(|| {
                anyhow::anyhow!("Could not determine local data directory for Hermes' config")
            })
    }
    #[cfg(not(windows))]
    {
        let home = dirs::home_dir().ok_or_else(|| {
            anyhow::anyhow!("Could not determine home directory for Hermes' config")
        })?;
        Ok(home.join(".hermes"))
    }
}

fn materialize_hermes_config(
    state_dir: &Path,
    source_dir: &Path,
    model: Option<serde_yaml::Value>,
    mcp_servers: &[(String, serde_yaml::Value)],
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;

    let nested = same_dir(state_dir, source_dir);
    if !nested {
        link_pass_through_resources(state_dir, source_dir, ui);
    }

    let mut root = read_yaml_mapping(&source_dir.join(CONFIG_YAML))?;
    let malformed = |what: &str| {
        anyhow::anyhow!(
            "{} in {} is not a YAML mapping",
            what,
            source_dir.join(CONFIG_YAML).display()
        )
    };
    let root_map = root
        .as_mapping_mut()
        .ok_or_else(|| malformed("the top-level value"))?;
    if let Some(model) = model {
        root_map.insert("model".into(), model);
    }
    if !mcp_servers.is_empty() {
        let servers = root_map
            .entry("mcp_servers".into())
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
            .as_mapping_mut()
            .ok_or_else(|| malformed("`mcp_servers`"))?;
        for (name, entry) in mcp_servers {
            servers.insert(name.clone().into(), entry.clone());
        }
    }

    write_owned_yaml(&state_dir.join(CONFIG_YAML), &root)
}

fn read_yaml_mapping(path: &Path) -> anyhow::Result<serde_yaml::Value> {
    let empty = || serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Ok(empty()),
        Ok(content) => {
            let value: serde_yaml::Value = serde_yaml::from_str(&content)
                .with_context(|| format!("{} is not valid YAML", path.display()))?;
            if value.is_null() {
                Ok(empty())
            } else {
                Ok(value)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty()),
        Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn write_owned_yaml(path: &Path, value: &serde_yaml::Value) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink()) {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to replace symlink at {}", path.display()))?;
    }
    let content = serde_yaml::to_string(value)?;
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

fn link_pass_through_resources(state_dir: &Path, source_dir: &Path, ui: &dyn Ui) {
    let entries = match std::fs::read_dir(source_dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let mut failed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == CONFIG_YAML || STATE_DIRS.iter().any(|dir| name == *dir) {
            continue;
        }
        let Ok(target) = entry.path().canonicalize() else {
            failed += 1;
            continue;
        };
        let link = state_dir.join(&name);
        match std::fs::symlink_metadata(&link) {
            Ok(md) if md.file_type().is_symlink() => {
                if std::fs::remove_file(&link).is_err() {
                    failed += 1;
                    continue;
                }
            }
            Ok(_) => continue,
            Err(_) => {}
        }
        if symlink(&target, &link).is_err() {
            failed += 1;
        }
    }

    if failed > 0 {
        ui.warn(&format!(
            "Could not link {failed} Hermes resource(s) from {} into {}; \
             settings, logins and plugins from there will not apply to this launch.",
            source_dir.display(),
            state_dir.display()
        ));
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Named, Secret};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::collections::HashMap;

    fn launcher(cfg: serde_json::Value) -> HermesLauncher {
        HermesLauncher::new("hermes", &cfg, &crate::config::Config::default())
    }

    fn binding() -> AgentModelBinding {
        AgentModelBinding {
            api_type: ApiType::OpenAI,
            provider_name: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            model_name: "granite4.1:8b".to_string(),
            endpoint_path: "/v1/chat/completions".to_string(),
            api_key: None,
            verify_ssl: true,
            context_length: 131072,
        }
    }

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> HermesLauncher {
        let mut l = launcher(cfg);
        l.bound_agent_model = Some(binding);
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "hermes".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: HashMap::new(),
            dry_run,
        }
    }

    // -- command resolution ----------------------------------------------------

    #[test]
    fn command_defaults_to_hermes() {
        assert_eq!(launcher(serde_json::json!({})).command(), "hermes");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = launcher(serde_json::json!({ "command_path": "/opt/bin/hermes" }));
        assert_eq!(l.command(), "/opt/bin/hermes");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = launcher(serde_json::json!({ "command_path": "/no/such/path/hermes" }));
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        assert!(l.validate_command().is_ok());
    }

    // -- metadata / schema -----------------------------------------------------

    #[test]
    fn metadata_name_is_hermes_cli() {
        let meta = HermesLauncher::metadata();
        assert_eq!(meta.name, "Hermes CLI");
        assert_eq!(meta.default_command, "hermes");
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
    }

    #[test]
    fn instance_id_round_trips_from_construction() {
        let l = HermesLauncher::new(
            "hermes-local",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert_eq!(l.instance_id(), "hermes-local");
    }

    #[test]
    fn config_schema_exposes_only_command_path_and_overrides() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<HermesLauncher>("hermes");
        let schema = factory.config_schema("hermes").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("model_overrides"));
        assert!(!props.contains_key("provider_name"));
    }

    // -- model section ---------------------------------------------------------

    #[test]
    fn model_section_describes_bound_model() {
        let section = launcher(serde_json::json!({}))
            .model_section(&binding())
            .unwrap();
        assert_eq!(section["default"], "granite4.1:8b");
        assert_eq!(section["provider"], "custom");
        assert_eq!(section["base_url"], "http://localhost:11434/v1");
        assert_eq!(section["context_length"], 131072);
        assert!(section.get("api_key").is_none());
    }

    #[test]
    fn model_section_interpolates_env_when_key_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let section = launcher(serde_json::json!({})).model_section(&b).unwrap();
        assert_eq!(section["api_key"], "${GRANITE_CLI_HERMES_API_KEY}");
    }

    #[test]
    fn model_section_omits_api_key_for_empty_secret() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("")),
            ..binding()
        };
        let section = launcher(serde_json::json!({})).model_section(&b).unwrap();
        assert!(section.get("api_key").is_none());
    }

    #[test]
    fn model_section_merges_overrides() {
        let l = launcher(serde_json::json!({
            "model_overrides": { "max_tokens": 4096 }
        }));
        let section = l.model_section(&binding()).unwrap();
        assert_eq!(section["max_tokens"], 4096);
        assert_eq!(section["base_url"], "http://localhost:11434/v1");
    }

    #[test]
    fn model_section_overrides_win_on_conflict() {
        let l = launcher(serde_json::json!({
            "model_overrides": { "base_url": "http://proxy:8080/v1" }
        }));
        let section = l.model_section(&binding()).unwrap();
        assert_eq!(section["base_url"], "http://proxy:8080/v1");
    }

    // -- base url ----------------------------------------------------------

    #[test]
    fn base_url_keeps_version_prefix_and_drops_operation() {
        assert_eq!(hermes_base_url(&binding()), "http://localhost:11434/v1");
    }

    #[test]
    fn base_url_trims_trailing_slash_from_provider_url() {
        let b = AgentModelBinding {
            base_url: "http://localhost:1234/".to_string(),
            ..binding()
        };
        assert_eq!(hermes_base_url(&b), "http://localhost:1234/v1");
    }

    // -- mcp server entries ----------------------------------------------------

    #[test]
    fn mcp_entry_for_stdio_server() {
        let entry = mcp_server_entry(&McpBinding::Stdio {
            command: "uvx".to_string(),
            args: vec!["mcp-server-time".to_string()],
            env: HashMap::from([("KEY".to_string(), "value".to_string())]),
        })
        .unwrap();
        assert_eq!(entry["command"], "uvx");
        assert_eq!(entry["args"][0], "mcp-server-time");
        assert_eq!(entry["env"]["KEY"], "value");
    }

    #[test]
    fn mcp_entry_for_http_server() {
        let entry = mcp_server_entry(&McpBinding::Http {
            url: "http://localhost:9000/mcp".to_string(),
            headers: HashMap::new(),
        })
        .unwrap();
        assert_eq!(entry["url"], "http://localhost:9000/mcp");
        assert!(entry.get("headers").is_none());
    }

    #[test]
    fn mcp_entry_for_sse_server_uses_url() {
        let entry = mcp_server_entry(&McpBinding::Sse {
            url: "http://localhost:9000/sse".to_string(),
            headers: HashMap::new(),
        })
        .unwrap();
        assert_eq!(entry["url"], "http://localhost:9000/sse");
    }

    // -- env overlay -----------------------------------------------------------

    #[tokio::test]
    async fn env_overlay_is_empty_without_a_binding() {
        let overlay = launcher(serde_json::json!({}))
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        assert!(overlay.is_empty());
    }

    #[tokio::test]
    async fn env_overlay_redirects_home_and_exports_api_key() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();

        let dir = overlay
            .iter()
            .find(|b| b.key == "HERMES_HOME")
            .expect("home redirect");
        assert!(
            dir.value.ends_with("launcher-state/hermes"),
            "{}",
            dir.value
        );

        let key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_HERMES_API_KEY")
            .expect("api key");
        assert_eq!(key.value, "sk-test");
    }

    #[tokio::test]
    async fn env_overlay_omits_api_key_when_provider_has_none() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        assert!(
            !overlay
                .iter()
                .any(|b| b.key == "GRANITE_CLI_HERMES_API_KEY")
        );
    }

    #[tokio::test]
    async fn env_overlay_omits_api_key_for_empty_secret() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        assert!(
            !overlay
                .iter()
                .any(|b| b.key == "GRANITE_CLI_HERMES_API_KEY")
        );
    }

    // -- materialized config ---------------------------------------------------

    fn dirs(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let source = tmp.path().join("user-hermes");
        std::fs::create_dir_all(&source).unwrap();
        (tmp.path().join("state"), source)
    }

    fn read_config_yaml(dir: &Path) -> serde_yaml::Value {
        serde_yaml::from_str(&std::fs::read_to_string(dir.join(CONFIG_YAML)).unwrap()).unwrap()
    }

    fn model_fixture() -> serde_yaml::Value {
        launcher(serde_json::json!({}))
            .model_section(&binding())
            .unwrap()
    }

    #[test]
    fn materialize_writes_model_and_mcp_sections() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        let mcp = vec![(
            "vision".to_string(),
            mcp_server_entry(&McpBinding::Http {
                url: "http://localhost:9000/mcp".to_string(),
                headers: HashMap::new(),
            })
            .unwrap(),
        )];
        materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &mcp,
            &CaptureUi::default(),
        )
        .unwrap();

        let written = read_config_yaml(&state);
        assert_eq!(written["model"]["default"], "granite4.1:8b");
        assert_eq!(written["model"]["provider"], "custom");
        assert_eq!(
            written["mcp_servers"]["vision"]["url"],
            "http://localhost:9000/mcp"
        );
    }

    #[test]
    fn materialize_never_writes_into_the_source_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        let source_config = source.join(CONFIG_YAML);
        let original = "model:\n  default: their-model\n  provider: openrouter\n";
        std::fs::write(&source_config, original).unwrap();

        materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &[],
            &CaptureUi::default(),
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&source_config).unwrap(), original);
        let written = read_config_yaml(&state);
        assert_eq!(written["model"]["default"], "granite4.1:8b");
    }

    #[test]
    fn materialize_preserves_unrelated_keys_and_user_mcp_servers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(
            source.join(CONFIG_YAML),
            "somethingElse: 42\nmcp_servers:\n  time:\n    command: uvx\n",
        )
        .unwrap();
        let mcp = vec![(
            "vision".to_string(),
            mcp_server_entry(&McpBinding::Http {
                url: "http://localhost:9000/mcp".to_string(),
                headers: HashMap::new(),
            })
            .unwrap(),
        )];

        materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &mcp,
            &CaptureUi::default(),
        )
        .unwrap();

        let written = read_config_yaml(&state);
        assert_eq!(written["somethingElse"], 42);
        assert_eq!(written["mcp_servers"]["time"]["command"], "uvx");
        assert_eq!(
            written["mcp_servers"]["vision"]["url"],
            "http://localhost:9000/mcp"
        );
    }

    #[test]
    fn materialize_round_trips_api_key_env_reference_through_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let model = launcher(serde_json::json!({})).model_section(&b).unwrap();
        materialize_hermes_config(&state, &source, Some(model), &[], &CaptureUi::default())
            .unwrap();
        assert_eq!(
            read_config_yaml(&state)["model"]["api_key"],
            "${GRANITE_CLI_HERMES_API_KEY}"
        );
    }

    #[test]
    fn materialize_writes_mcp_servers_without_a_model_binding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        let mcp = vec![(
            "vision".to_string(),
            mcp_server_entry(&McpBinding::Http {
                url: "http://localhost:9000/mcp".to_string(),
                headers: HashMap::new(),
            })
            .unwrap(),
        )];
        materialize_hermes_config(&state, &source, None, &mcp, &CaptureUi::default()).unwrap();

        let written = read_config_yaml(&state);
        assert_eq!(
            written["mcp_servers"]["vision"]["url"],
            "http://localhost:9000/mcp"
        );
        assert!(written.get("model").is_none());
    }

    #[test]
    fn materialize_works_with_no_user_hermes_config_at_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let source = tmp.path().join("does-not-exist");

        materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &[],
            &CaptureUi::default(),
        )
        .unwrap();
        assert_eq!(
            read_config_yaml(&state)["model"]["default"],
            "granite4.1:8b"
        );
    }

    #[test]
    fn materialize_tolerates_comment_only_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(CONFIG_YAML), "# just a comment\n").unwrap();

        materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &[],
            &CaptureUi::default(),
        )
        .unwrap();
        assert_eq!(
            read_config_yaml(&state)["model"]["default"],
            "granite4.1:8b"
        );
    }

    #[test]
    fn materialize_is_idempotent_across_launches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join("settings.json"), "{}").unwrap();

        for model_name in ["old-model", "new-model"] {
            let b = AgentModelBinding {
                model_name: model_name.to_string(),
                ..binding()
            };
            let model = launcher(serde_json::json!({})).model_section(&b).unwrap();
            materialize_hermes_config(&state, &source, Some(model), &[], &CaptureUi::default())
                .unwrap();
        }
        assert_eq!(read_config_yaml(&state)["model"]["default"], "new-model");
        assert!(state.join("settings.json").exists());
    }

    #[test]
    fn materialize_refuses_malformed_source_yaml() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(CONFIG_YAML), "model: [unclosed").unwrap();

        let err = materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &[],
            &CaptureUi::default(),
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("not valid YAML"));
        assert!(!state.join(CONFIG_YAML).exists());
    }

    #[test]
    fn materialize_refuses_non_mapping_mcp_servers_in_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(CONFIG_YAML), "mcp_servers: []\n").unwrap();
        let mcp = vec![(
            "vision".to_string(),
            mcp_server_entry(&McpBinding::Http {
                url: "http://localhost:9000/mcp".to_string(),
                headers: HashMap::new(),
            })
            .unwrap(),
        )];

        let err = materialize_hermes_config(&state, &source, None, &mcp, &CaptureUi::default())
            .expect_err("must fail");
        assert!(err.to_string().contains("not a YAML mapping"));
    }

    #[cfg(unix)]
    #[test]
    fn materialize_links_user_resources_but_not_config_or_state_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(".env"), "NOUS_API_KEY=secret").unwrap();
        std::fs::create_dir(source.join("plugins")).unwrap();
        for dir in STATE_DIRS {
            std::fs::create_dir(source.join(dir)).unwrap();
        }
        std::fs::write(source.join(CONFIG_YAML), "{}").unwrap();

        materialize_hermes_config(
            &state,
            &source,
            Some(model_fixture()),
            &[],
            &CaptureUi::default(),
        )
        .unwrap();

        for linked in [".env", "plugins"] {
            let path = state.join(linked);
            let md = std::fs::symlink_metadata(&path)
                .unwrap_or_else(|_| panic!("{linked} should be linked"));
            assert!(md.file_type().is_symlink(), "{linked} should be a symlink");
            let target = std::fs::read_link(&path).unwrap();
            assert!(
                target.is_absolute(),
                "{linked} -> {} must be absolute",
                target.display()
            );
            assert!(path.exists(), "{linked} link must resolve");
        }
        assert_eq!(
            std::fs::read_to_string(state.join(".env")).unwrap(),
            "NOUS_API_KEY=secret"
        );
        for dir in STATE_DIRS {
            assert!(!state.join(dir).exists(), "{dir} must stay local");
        }
        assert!(
            !std::fs::symlink_metadata(state.join(CONFIG_YAML))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_owned_yaml_replaces_a_symlink_instead_of_writing_through_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("users-real-config.yaml");
        std::fs::write(&victim, "SACRED").unwrap();
        let link = tmp.path().join(CONFIG_YAML);
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        write_owned_yaml(&link, &serde_yaml::from_str("ours: true").unwrap()).unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "SACRED");
        let written: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&link).unwrap()).unwrap();
        assert_eq!(written["ours"], true);
    }

    #[test]
    fn materialize_tolerates_source_equal_to_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("both");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG_YAML), "keep: true\n").unwrap();

        materialize_hermes_config(
            &dir,
            &dir,
            Some(model_fixture()),
            &[],
            &CaptureUi::default(),
        )
        .unwrap();

        let written = read_config_yaml(&dir);
        assert_eq!(written["keep"], true);
        assert_eq!(written["model"]["default"], "granite4.1:8b");
    }

    // -- launch ----------------------------------------------------------------

    // Deliberately reads whatever `GRANITE_CLI_HOME` is ambient rather than
    // setting it: env mutation would race the other tests in this binary that
    // point that var at their own tempdirs.
    #[tokio::test]
    async fn dry_run_launch_reports_without_writing_anything() {
        let state_dir = crate::config::Config::launcher_state_dir("hermes").unwrap();
        let existed_before = state_dir.exists();

        let l = bound(serde_json::json!({ "command_path": "ls" }), binding());
        let ui = CaptureUi::default();
        let status = l
            .launch(&["--help".to_string()], &ctx(true), &ui)
            .await
            .unwrap();
        assert!(status.success());

        let infos = ui.infos.borrow();
        assert!(
            infos
                .iter()
                .any(|m| m.contains("Would write Hermes config")),
            "expected a dry-run notice, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("left unmodified")),
            "expected the source file to be called out as untouched, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("args: --help")),
            "expected caller args to pass through unmodified, got {infos:?}"
        );
        assert_eq!(
            state_dir.exists(),
            existed_before,
            "dry run must not create {}",
            state_dir.display()
        );
    }

    #[tokio::test]
    async fn dry_run_launch_with_only_mcp_binding_previews_servers() {
        let state_dir = crate::config::Config::launcher_state_dir("hermes").unwrap();
        let existed_before = state_dir.exists();

        let mut l = launcher(serde_json::json!({ "command_path": "ls" }));
        l.bound_mcp_bindings.push((
            "vision".to_string(),
            McpBinding::Http {
                url: "http://localhost:9000/mcp".to_string(),
                headers: HashMap::new(),
            },
        ));
        let ui = CaptureUi::default();
        let status = l.launch(&[], &ctx(true), &ui).await.unwrap();
        assert!(status.success());

        let infos = ui.infos.borrow();
        assert!(
            infos
                .iter()
                .any(|m| m.contains("Would write Hermes config")),
            "expected a dry-run notice, got {infos:?}"
        );
        assert!(
            infos
                .iter()
                .any(|m| m.contains("http://localhost:9000/mcp")),
            "expected the MCP server in the preview, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("env: HERMES_HOME=")),
            "expected the home redirect in the overlay, got {infos:?}"
        );
        assert_eq!(
            state_dir.exists(),
            existed_before,
            "dry run must not create {}",
            state_dir.display()
        );
    }

    #[tokio::test]
    async fn launch_without_binding_passes_args_through_unchanged() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        let ui = CaptureUi::default();
        l.launch(&["--version".to_string()], &ctx(true), &ui)
            .await
            .unwrap();

        let infos = ui.infos.borrow();
        assert!(infos.iter().any(|m| m.contains("args: --version")));
        assert!(!infos.iter().any(|m| m.contains("--provider")));
        assert!(!infos.iter().any(|m| m.contains(CONFIG_DIR_ENV)));
    }
}
