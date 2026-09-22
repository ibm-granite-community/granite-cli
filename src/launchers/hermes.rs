//! Launcher for the `hermes` coding agent CLI
//! (<https://hermes-agent.nousresearch.com>, NousResearch's agent CLI --
//! not the Hermes model family).
//!
//! `HERMES_HOME` is Hermes's "profile boundary": it fully redirects
//! `config.yaml`, `.env`, `auth.json`, `memories/`, `skills/`, `cron/`,
//! `sessions/`, and `logs/` all at once. Pointing it straight at an
//! otherwise-empty generated directory (as this launcher originally did)
//! would silently discard the user's real auth/memories/skills on every
//! launch, so -- exactly like `pi.rs` does for `PI_CODING_AGENT_DIR` -- this
//! launcher symlinks every other top-level entry from the user's real
//! `$HERMES_HOME`/`~/.hermes` through into its own generated directory and
//! only ever writes `config.yaml` itself.

use crate::capabilities::{
    AgentModelBinding, ApiType, Binding, BindingType, McpBinding, ResolvedCapability,
};
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::mcp_binding_request;
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct HermesLauncherConfig {
    /// Override path to the `hermes` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Extra keys merged (shallow, last-write-wins) into the generated
    /// hermes config file -- e.g. `model.context_length` or provider-specific
    /// overrides. Necessary because the entry is regenerated on every launch.
    #[serde(default)]
    pub model_overrides: Option<serde_json::Value>,
}

pub struct HermesLauncher {
    instance_id: String,
    config: HermesLauncherConfig,
    bound_binding: Option<AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, written into the generated config's `mcp_servers` block.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
}

impl ConfigConstructable for HermesLauncher {
    type Config = HermesLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: HermesLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_binding: None,
            bound_mcp_bindings: vec![],
        })
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

    async fn bind_capability(&mut self, capability: &dyn ResolvedCapability) -> anyhow::Result<()> {
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

        let binding = capability
            .bind(crate::capabilities::BindingRequest::AgentModel(
                crate::capabilities::AgentModelBindingRequest {
                    api_type: ApiType::OpenAI,
                },
            ))
            .await?;
        match binding {
            Binding::AgentModel(binding) => {
                self.bound_binding = Some(binding);
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

    /// Redirects Hermes at the granite-cli-owned config directory and
    /// supplies the credential the generated `config.yaml` interpolates.
    async fn env_overlay(&self, ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        if self.bound_binding.is_none() && self.bound_mcp_bindings.is_empty() {
            return Ok(vec![]);
        }
        let mut overlay = vec![EnvBinding {
            key: HERMES_HOME_ENV.to_string(),
            value: hermes_state_dir(ctx)?.to_string_lossy().to_string(),
        }];
        if let Some(binding) = &self.bound_binding
            && let Some(api_key) = binding
                .api_key
                .as_ref()
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

    /// Materializes the granite-cli Hermes config directory (pass-through
    /// symlinks plus a freshly generated `config.yaml`), then execs `hermes`
    /// with the caller's arguments untouched.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;

        if self.bound_binding.is_some() || !self.bound_mcp_bindings.is_empty() {
            let config = self.generate_config()?;
            let state_dir = hermes_state_dir(ctx)?;
            let source_dir = hermes_source_dir()?;
            let config_path = state_dir.join(HERMES_CONFIG_FILE);

            if ctx.dry_run {
                ui.info(&format!(
                    "Would write Hermes config to {}:",
                    config_path.display()
                ));
                ui.info(&serde_json::to_string_pretty(&config)?);
                ui.info(&format!(
                    "  (other Hermes state linked through from {}, which is left unmodified)",
                    source_dir.display()
                ));
            } else {
                materialize_hermes_config(&state_dir, &source_dir, &config, ui)?;
                ui.info(&format!("Wrote Hermes config to {}", config_path.display()));
            }
        }

        run_command(binary, &self.env_overlay(ctx).await?, args, ctx, ui).await
    }
}

impl HasHermesLauncherMetadata for HermesLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Hermes CLI".to_string(),
            description: "Hermes Agent local CLI launcher".to_string(),
            default_command: "hermes".to_string(),
            supported_capabilities: HashSet::from([BindingType::AgentModel, BindingType::Mcp]),
            tags: vec!["hermes".to_string(), "agent".to_string()],
        }
    }
}

/*-- private --*/

use crate::launchers::base::HasLauncherMetadata as HasHermesLauncherMetadata;

/// Env var Hermes reads to locate its config directory -- and, per its own
/// docs, the boundary of an entire "profile" (config, secrets, auth,
/// memories, skills, sessions, logs), not just the one config file.
const HERMES_HOME_ENV: &str = "HERMES_HOME";

/// Env var the generated config's `model.api_key` field interpolates via
/// Hermes's `${VAR_NAME}` substitution syntax.
const API_KEY_ENV: &str = "GRANITE_CLI_HERMES_API_KEY";

/// The generated Hermes config file's name, relative to the launcher state dir.
const HERMES_CONFIG_FILE: &str = "config.yaml";

/// Hermes's `base_url` is the API root it appends operation paths to (e.g.
/// `/chat/completions`), same convention as `opencode.rs`/`pi.rs`/
/// `openclaw.rs`'s equivalent helpers -- so the trailing operation is
/// dropped from the binding's full endpoint path here, keeping the version
/// prefix (e.g. `/v1`). Without this, `base_url` is left as the bare host
/// root and every request 404s, since Ollama's (and most OpenAI-compatible
/// servers') actual endpoints live under `/v1`.
fn hermes_base_url(binding: &AgentModelBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = binding
        .endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

impl HermesLauncher {
    /// Builds the Hermes config describing the bound model and MCP servers.
    fn generate_config(&self) -> anyhow::Result<serde_json::Value> {
        let mut config = serde_json::json!({});

        if let Some(binding) = &self.bound_binding {
            let mut model = serde_json::json!({
                // `provider` is a discriminator Hermes recognizes (a
                // built-in id, or "custom") -- not a free-form label, so the
                // granite-cli provider name never goes here.
                "provider": "custom",
                "default": binding.model_name,
            });
            if !binding.base_url.is_empty() {
                model["base_url"] = serde_json::Value::String(hermes_base_url(binding));
            }
            if let Some(context_length) = binding.context_length {
                model["context_length"] = serde_json::json!(context_length);
            }
            if binding
                .api_key
                .as_ref()
                .is_some_and(|key| !key.0.is_empty())
            {
                model["api_key"] = serde_json::Value::String(format!("${{{API_KEY_ENV}}}"));
            }

            // Add custom headers as extra_headers if configured.
            // Sorted by key for deterministic output.
            if let Some(ref headers) = binding.custom_headers {
                if !headers.is_empty() {
                    model["extra_headers"] = serde_json::to_value(headers)?;
                }
            }

            // Merge user-provided overrides on top so they win on conflict.
            // Special case: if the override key is "model", merge the inner
            // object into config["model"] rather than replacing it entirely.
            if let Some(overrides) = self
                .config
                .model_overrides
                .as_ref()
                .and_then(serde_json::Value::as_object)
            {
                for (key, value) in overrides {
                    if key == "model" {
                        if let Some(inner) = value.as_object()
                            && let Some(target) = model.as_object_mut()
                        {
                            for (k, v) in inner {
                                target.insert(k.clone(), v.clone());
                            }
                        }
                    } else if let Some(target) = config.as_object_mut() {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            config["model"] = model;
        }

        if !self.bound_mcp_bindings.is_empty() {
            let mut mcp_servers = serde_json::Map::new();
            for (name, binding) in &self.bound_mcp_bindings {
                mcp_servers.insert(name.clone(), {
                    match binding {
                        McpBinding::Stdio {
                            command, args, env, ..
                        } => serde_json::json!({
                            "command": command,
                            "args": args,
                            "env": env,
                        }),
                        McpBinding::Http { url, headers, .. }
                        | McpBinding::Sse { url, headers, .. } => {
                            serde_json::json!({
                                "url": url,
                                "headers": headers,
                            })
                        }
                    }
                });
            }
            config["mcp_servers"] = serde_json::Value::Object(mcp_servers);
        }

        Ok(config)
    }
}

/// The granite-cli-owned Hermes config directory for this launcher instance.
fn hermes_state_dir(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    crate::config::Config::launcher_state_dir(&ctx.launcher_id)
}

/// The user's own Hermes profile directory, which we only ever read from:
/// `$HERMES_HOME` when set, else `~/.hermes`.
fn hermes_source_dir() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var(HERMES_HOME_ENV)
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory for Hermes's config"))?;
    Ok(home.join(".hermes"))
}

/// Builds `state_dir` into a usable Hermes profile: pass-through links to
/// everything in the user's real profile (`.env`, `auth.json`, `SOUL.md`,
/// `memories/`, `skills/`, `cron/`, `sessions/`, `logs/`), plus a freshly
/// written `config.yaml` that granite-cli owns outright. Nothing under
/// `source_dir` is written.
fn materialize_hermes_config(
    state_dir: &Path,
    source_dir: &Path,
    config: &serde_json::Value,
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;

    // A nested launch can land here with source == state (the parent already
    // redirected HERMES_HOME at us); linking a directory into itself is
    // meaningless, and config.yaml is already ours.
    if !same_dir(state_dir, source_dir) {
        link_pass_through_resources(state_dir, source_dir, ui);
    }

    write_owned_yaml(&state_dir.join(HERMES_CONFIG_FILE), config)
}

/// Links every top-level entry of the user's Hermes profile into `state_dir`,
/// so their secrets, OAuth credentials, memories, skills, cron jobs, and
/// sessions still apply. `config.yaml` is skipped (granite-cli generates its
/// own).
///
/// Best-effort: a platform or permission that refuses symlinks costs the user
/// those resources for granite-cli launches, not the launch itself.
fn link_pass_through_resources(state_dir: &Path, source_dir: &Path, ui: &dyn Ui) {
    let entries = match std::fs::read_dir(source_dir) {
        Ok(entries) => entries,
        // No Hermes profile of their own yet -- nothing to pass through.
        Err(_) => return,
    };

    let mut failed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == HERMES_CONFIG_FILE {
            continue;
        }
        // The link target must be absolute: a relative one resolves against
        // the *link's* directory, not ours, and would dangle.
        let Ok(target) = entry.path().canonicalize() else {
            failed += 1;
            continue;
        };
        let link = state_dir.join(&name);
        match std::fs::symlink_metadata(&link) {
            // Refresh our own link in case the target moved.
            Ok(md) if md.file_type().is_symlink() => {
                if std::fs::remove_file(&link).is_err() {
                    failed += 1;
                    continue;
                }
            }
            // Something real is sitting there; leave it be.
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
             secrets, auth and memories from there will not apply to this launch.",
            source_dir.display(),
            state_dir.display()
        ));
    }
}

/// Writes a file granite-cli owns outright. Any symlink already at `path` is
/// removed first -- writing through one would land in whatever it points at,
/// which is exactly the user's file we are trying not to touch.
fn write_owned_yaml(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink()) {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to replace symlink at {}", path.display()))?;
    }
    let content = serde_yaml::to_string(value)
        .with_context(|| format!("Failed to serialize {}", path.display()))?;
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

/// Whether two paths denote the same directory, comparing canonical forms when
/// both exist and falling back to a literal comparison when they don't.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
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

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::BindingType;
    use crate::registry::{Named, Secret};
    use crate::utils::ui::base::tests::CaptureUi;

    fn launcher(cfg: serde_json::Value) -> HermesLauncher {
        HermesLauncher::new("hermes", &cfg).unwrap()
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
            context_length: Some(131072),
            custom_headers: None,
        }
    }

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> HermesLauncher {
        let mut l = launcher(cfg);
        l.bound_binding = Some(binding);
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "hermes".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: std::collections::HashMap::new(),
            dry_run,
            usage_tracker: None,
            model_proxy: None,
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
        let l = HermesLauncher::new("hermes-local", &serde_json::json!({})).unwrap();
        assert_eq!(l.instance_id(), "hermes-local");
    }

    #[test]
    fn config_schema_exposes_command_path_and_model_overrides() {
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
    }

    // -- generate_config ------------------------------------------------------

    #[test]
    fn generate_config_sets_default_model_and_custom_provider() {
        let l = bound(serde_json::json!({}), binding());
        let config = l.generate_config().unwrap();
        // Key is `default`, not `model` -- and `provider` is always the
        // "custom" discriminator, never the granite-cli provider name.
        assert_eq!(config["model"]["default"], "granite4.1:8b");
        assert_eq!(config["model"]["provider"], "custom");
        // Version prefix kept, trailing operation dropped -- not the bare
        // host root, which would 404 against Ollama's /v1 endpoints.
        assert_eq!(config["model"]["base_url"], "http://localhost:11434/v1");
        assert_eq!(config["model"]["context_length"], serde_json::json!(131072));
    }

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

    #[test]
    fn generate_config_omits_context_length_when_none() {
        let b = AgentModelBinding {
            context_length: None,
            ..binding()
        };
        let l = bound(serde_json::json!({}), b);
        let config = l.generate_config().unwrap();
        assert!(config["model"].get("context_length").is_none());
    }

    #[test]
    fn generate_config_interpolates_api_key_env_when_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let l = bound(serde_json::json!({}), b);
        let config = l.generate_config().unwrap();
        assert_eq!(config["model"]["api_key"], "${GRANITE_CLI_HERMES_API_KEY}");
    }

    #[test]
    fn generate_config_omits_api_key_when_absent() {
        let l = bound(serde_json::json!({}), binding());
        let config = l.generate_config().unwrap();
        assert!(config["model"].get("api_key").is_none());
    }

    #[test]
    fn generate_config_merges_overrides() {
        let l = bound(
            serde_json::json!({
                "model_overrides": {
                    "model": {
                        "context_length": 4000
                    }
                }
            }),
            binding(),
        );
        let config = l.generate_config().unwrap();
        // Override wins
        assert_eq!(config["model"]["context_length"], serde_json::json!(4000));
        // Generated keys survive the merge
        assert_eq!(config["model"]["provider"], "custom");
        assert_eq!(config["model"]["default"], "granite4.1:8b");
    }

    #[test]
    fn generate_config_writes_mcp_servers_stdio_and_http() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_mcp_bindings.push((
            "vision".to_string(),
            McpBinding::Stdio {
                command: "/usr/local/bin/granite-cli".to_string(),
                args: vec!["__mcp-serve".to_string(), "vision".to_string()],
                env: std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())]),
                timeout: None,
            },
        ));
        l.bound_mcp_bindings.push((
            "remote".to_string(),
            McpBinding::Http {
                url: "http://127.0.0.1:9999".to_string(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer x".to_string(),
                )]),
                timeout: None,
            },
        ));
        let config = l.generate_config().unwrap();
        assert_eq!(
            config["mcp_servers"]["vision"]["command"],
            "/usr/local/bin/granite-cli"
        );
        assert_eq!(config["mcp_servers"]["vision"]["args"][0], "__mcp-serve");
        assert_eq!(config["mcp_servers"]["vision"]["env"]["FOO"], "bar");
        assert_eq!(
            config["mcp_servers"]["remote"]["url"],
            "http://127.0.0.1:9999"
        );
        assert_eq!(
            config["mcp_servers"]["remote"]["headers"]["Authorization"],
            "Bearer x"
        );
        assert!(config.get("model").is_none());
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
    async fn env_overlay_sets_hermes_home_when_bound() {
        let overlay = bound(serde_json::json!({}), binding())
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let home = overlay
            .iter()
            .find(|b| b.key == HERMES_HOME_ENV)
            .expect("HERMES_HOME env");
        assert!(
            Path::new(&home.value).ends_with(Path::new("launcher-state").join("hermes")),
            "{}",
            home.value
        );
    }

    #[tokio::test]
    async fn env_overlay_sets_api_key_env_when_key_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();
        let key = overlay
            .iter()
            .find(|b| b.key == API_KEY_ENV)
            .expect("api key env");
        assert_eq!(key.value, "sk-test");
    }

    // -- pass-through linking ---------------------------------------------------

    /// A (state_dir, source_dir) pair inside one tempdir.
    fn dirs(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let source = tmp.path().join("user-hermes");
        std::fs::create_dir_all(&source).unwrap();
        (tmp.path().join("state"), source)
    }

    #[test]
    fn materialize_writes_config_and_never_touches_source_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(
            source.join("config.yaml"),
            "model:\n  default: user-model\n",
        )
        .unwrap();

        materialize_hermes_config(
            &state,
            &source,
            &serde_json::json!({ "model": { "default": "granite4.1:8b" } }),
            &CaptureUi::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(source.join("config.yaml")).unwrap(),
            "model:\n  default: user-model\n"
        );
        let written: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(state.join("config.yaml")).unwrap())
                .unwrap();
        assert_eq!(written["model"]["default"], "granite4.1:8b");
    }

    #[cfg(unix)]
    #[test]
    fn materialize_links_user_resources_but_not_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join(".env"), "SECRET=1").unwrap();
        std::fs::write(source.join("auth.json"), "{}").unwrap();
        std::fs::create_dir(source.join("memories")).unwrap();
        std::fs::create_dir(source.join("skills")).unwrap();
        std::fs::write(source.join("config.yaml"), "{}").unwrap();

        materialize_hermes_config(
            &state,
            &source,
            &serde_json::json!({}),
            &CaptureUi::default(),
        )
        .unwrap();

        for linked in [".env", "auth.json", "memories", "skills"] {
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
            "SECRET=1"
        );
        // config.yaml is ours, not a link into the user's directory.
        assert!(
            !std::fs::symlink_metadata(state.join("config.yaml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn materialize_works_with_no_user_hermes_profile_at_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let source = tmp.path().join("does-not-exist");

        materialize_hermes_config(
            &state,
            &source,
            &serde_json::json!({ "model": { "default": "x" } }),
            &CaptureUi::default(),
        )
        .unwrap();

        let written: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(state.join("config.yaml")).unwrap())
                .unwrap();
        assert_eq!(written["model"]["default"], "x");
    }

    #[test]
    fn materialize_tolerates_source_equal_to_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("both");
        std::fs::create_dir_all(&dir).unwrap();

        materialize_hermes_config(
            &dir,
            &dir,
            &serde_json::json!({ "model": { "default": "x" } }),
            &CaptureUi::default(),
        )
        .unwrap();

        let written: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(dir.join("config.yaml")).unwrap())
                .unwrap();
        assert_eq!(written["model"]["default"], "x");
    }

    #[cfg(unix)]
    #[test]
    fn write_owned_yaml_replaces_a_symlink_instead_of_writing_through_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("users-real-file.yaml");
        std::fs::write(&victim, "SACRED").unwrap();
        let link = tmp.path().join("config.yaml");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        write_owned_yaml(&link, &serde_json::json!({ "ours": true })).unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "SACRED");
        let written: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&link).unwrap()).unwrap();
        assert_eq!(written["ours"], true);
    }

    // -- launch ---------------------------------------------------------------

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
            infos.iter().any(|m| {
                m.contains(r#""default": "granite4.1:8b""#) && m.contains(r#""provider": "custom""#)
            }),
            "expected the generated config to select the model, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("left unmodified")),
            "expected the source profile to be called out as untouched, got {infos:?}"
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
    async fn launch_without_binding_passes_args_through_unchanged() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        let ui = CaptureUi::default();
        l.launch(&["--version".to_string()], &ctx(true), &ui)
            .await
            .unwrap();

        let infos = ui.infos.borrow();
        assert!(infos.iter().any(|m| m.contains("--version")));
        assert!(
            !infos
                .iter()
                .any(|m| m.contains("Would write Hermes config")),
            "expected no config write without binding"
        );
        // Without a binding there is no generated config, so hermes keeps
        // using its own config chain.
        assert!(!infos.iter().any(|m| m.contains(HERMES_HOME_ENV)));
    }
}
