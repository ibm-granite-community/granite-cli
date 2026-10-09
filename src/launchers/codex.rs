//! Launcher for the OpenAI Codex CLI (`codex`).
//!
//! Codex is configured through a `config.toml` file inside its home directory
//! (`CODEX_HOME`, defaulting to `~/.codex`).  This launcher writes a fresh
//! `config.toml` into a granite-cli-owned directory under `GRANITE_CLI_HOME`
//! and points Codex at it by setting `CODEX_HOME`, following the same pattern
//! as `pi.rs` (`PI_CODING_AGENT_DIR`) and `hermes.rs` (`HERMES_HOME`):
//!
//! - Every entry in the user's real `~/.codex` (or `$CODEX_HOME`) is
//!   symlinked into the generated directory so auth, memories, sessions, etc.
//!   all carry over unchanged.
//! - Only `config.toml` is written fresh — model/provider settings plus any
//!   bound MCP servers — so the user's real config is never touched.
//!
//! Codex uses the OpenAI **Responses API** (`POST /v1/responses`) rather than
//! Chat Completions. Codex 0.160 and later reject the Chat Completions wire
//! protocol, so every generated provider uses `wire_api = "responses"`.

// Standard
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// Third Party
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    AgentModelBinding, Binding, BindingType, McpBinding, ResolvedCapability, SubAgentBinding,
};
use crate::launchers::base::HasLauncherMetadata as HasCodexLauncherMetadata;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::mcp_binding_request;
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct CodexLauncherConfig {
    /// Override path to the `codex` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Extra keys merged (shallow, last-write-wins) into the generated
    /// `[model_providers.<id>]` table. `wire_api` is reserved and must be
    /// `"responses"`: current Codex versions reject the older Chat
    /// Completions wire protocol. Necessary because the provider entry is
    /// regenerated on every launch.
    #[serde(default)]
    pub model_overrides: Option<serde_json::Value>,

    /// Additional `[model_providers.<id>]` entries written verbatim into the
    /// generated `config.toml` alongside the primary bound provider.
    ///
    /// Use this to expose extra endpoints — e.g. a local Ollama instance — so
    /// that Codex custom agents (defined under `~/.codex/agents/` or
    /// `.codex/agents/`) can reference them by name with `model = "<id>/..."`.
    ///
    /// Each key is the provider ID; the value is a map of TOML fields for that
    /// provider (`name`, `base_url`, `env_key`, `wire_api`, etc.).
    ///
    /// Example in granite-cli config:
    /// ```toml
    /// [launchers.my-codex.extra_providers.local-ollama]
    /// name     = "Local Ollama"
    /// base_url = "http://localhost:11434"
    /// wire_api = "responses"
    /// ```
    #[serde(default)]
    pub extra_providers: std::collections::HashMap<String, serde_json::Value>,
}

pub struct CodexLauncher {
    instance_id: String,
    config: CodexLauncherConfig,
    bound_agent_model: Option<AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher; written into the generated `config.toml`.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// Named Codex agent roles generated from granite-cli sub-agent
    /// capabilities. The role TOML files live in the launcher state directory.
    bound_sub_agents: Vec<(String, SubAgentBinding)>,
}

impl ConfigConstructable for CodexLauncher {
    type Config = CodexLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: CodexLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_agent_model: None,
            bound_mcp_bindings: vec![],
            bound_sub_agents: vec![],
        })
    }
}

impl crate::registry::Named for CodexLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for CodexLauncher {
    fn name(&self) -> &str {
        "Codex CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("codex")
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

        if capability_types.contains(&BindingType::SubAgent) {
            let request = crate::capabilities::BindingRequest::SubAgent(
                crate::capabilities::SubAgentBindingRequest {
                    api_type: crate::providers::ApiType::OpenAI,
                },
            );
            let binding = capability.bind(request).await?;
            match binding {
                Binding::SubAgent(binding) => {
                    self.bound_sub_agents
                        .push((capability.instance_id().to_string(), binding));
                }
                other => anyhow::bail!(
                    "expected a SubAgent binding, got {:?}",
                    other.binding_type()
                ),
            }
            return Ok(());
        }

        // Codex speaks the OpenAI Responses API, which is an OpenAI-family
        // protocol. Request an OpenAI binding from the capability.
        let request = crate::capabilities::BindingRequest::AgentModel(
            crate::capabilities::AgentModelBindingRequest {
                api_type: crate::providers::ApiType::OpenAI,
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
        resolve_shell_command(&self.config.command_path, "codex")
    }

    /// Redirects Codex at the granite-cli-owned config directory and supplies
    /// the API key that the generated `config.toml` reads from the environment.
    async fn env_overlay(&self, ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        if self.bound_agent_model.is_none()
            && self.bound_mcp_bindings.is_empty()
            && self.bound_sub_agents.is_empty()
        {
            return Ok(vec![]);
        }

        let mut overlay = vec![EnvBinding {
            key: CODEX_HOME_ENV.to_string(),
            value: codex_state_dir(ctx)?.to_string_lossy().to_string(),
        }];

        if let Some(binding) = &self.bound_agent_model {
            let base_url = codex_base_url(
                ctx.model_proxy
                    .as_ref()
                    .map(|handle| handle.local_base_url.as_str()),
                binding,
            );
            overlay.push(EnvBinding {
                key: "OPENAI_BASE_URL".to_string(),
                value: base_url,
            });

            if let Some(api_key) = binding
                .api_key
                .as_ref()
                .map(|k| k.0.clone())
                .filter(|k| !k.is_empty())
            {
                overlay.push(EnvBinding {
                    key: API_KEY_ENV.to_string(),
                    value: api_key,
                });
            }
        }

        for (_, sub_agent) in &self.bound_sub_agents {
            if let Some(api_key) = sub_agent
                .model
                .api_key
                .as_ref()
                .map(|key| key.0.clone())
                .filter(|key| !key.is_empty())
            {
                overlay.push(EnvBinding {
                    key: sub_agent_api_key_env(&sub_agent.model.provider_name),
                    value: api_key,
                });
            }
        }

        Ok(overlay)
    }

    /// Maps a canonical `ToolName` onto Codex's own built-in tool-name
    /// strings. MCP tools use Codex's `<server>.<tool>` dotted convention.
    fn map_tool_name(&self, tool: &crate::capabilities::ToolName) -> Option<String> {
        use crate::capabilities::ToolName;
        Some(match tool {
            ToolName::FileRead => "read_file".to_string(),
            ToolName::FileWrite => "write_file".to_string(),
            ToolName::FileEdit => "apply_patch".to_string(),
            ToolName::Search => "grep".to_string(),
            ToolName::FileSearch => "find_files".to_string(),
            ToolName::Shell => "shell".to_string(),
            ToolName::WebFetch => "web_fetch".to_string(),
            ToolName::WebSearch => "web_search".to_string(),
            ToolName::Mcp { server, tool: None } => format!("{server}.*"),
            ToolName::Mcp {
                server,
                tool: Some(t),
            } => format!("{server}.{t}"),
            ToolName::Other(raw) => raw.clone(),
        })
    }

    /// Materializes the granite-cli Codex config directory (pass-through
    /// symlinks plus a freshly generated `config.toml`), then execs `codex`
    /// with the caller's arguments untouched.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;

        if self.bound_agent_model.is_some()
            || !self.bound_mcp_bindings.is_empty()
            || !self.bound_sub_agents.is_empty()
        {
            let state_dir = codex_state_dir(ctx)?;
            let config_toml = self.generate_config_toml(ctx)?;
            let sub_agent_files = self.generate_sub_agent_files(&state_dir)?;
            let source_dir = codex_source_dir()?;
            let config_path = state_dir.join(CODEX_CONFIG_FILE);

            if ctx.dry_run {
                ui.info(&format!(
                    "Would write Codex config to {}:",
                    config_path.display()
                ));
                ui.info(&config_toml);
                for (path, content) in &sub_agent_files {
                    ui.info(&format!(
                        "Would write Codex agent role to {}:",
                        path.display()
                    ));
                    ui.info(content);
                }
                ui.info(&format!(
                    "  (other Codex state linked through from {}, which is left unmodified)",
                    source_dir.display()
                ));
            } else {
                materialize_codex_config(
                    &state_dir,
                    &source_dir,
                    &config_toml,
                    &sub_agent_files,
                    ui,
                )?;
                ui.info(&format!("Wrote Codex config to {}", config_path.display()));
            }
        }

        run_command(binary, &self.env_overlay(ctx).await?, args, ctx, ui).await
    }
}

impl HasCodexLauncherMetadata for CodexLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Codex CLI".to_string(),
            description: "OpenAI Codex CLI coding agent".to_string(),
            default_command: "codex".to_string(),
            supported_capabilities: HashSet::from([
                BindingType::AgentModel,
                BindingType::Mcp,
                BindingType::SubAgent,
            ]),
            tags: vec!["codex".to_string(), "openai".to_string()],
        }
    }
}

/*-- private --*/

/// Env var Codex reads to locate its home directory (config, auth, sessions,
/// memories, etc.) — the full "profile boundary", analogous to `HERMES_HOME`.
const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Env var the generated config's `env_key` field names; the actual key value
/// is injected into the subprocess environment separately.
const API_KEY_ENV: &str = "GRANITE_CLI_CODEX_API_KEY";

/// The generated Codex config file name, relative to `CODEX_HOME`.
const CODEX_CONFIG_FILE: &str = "config.toml";

/// Directory below the launcher state root that contains role TOML files
/// generated for bound sub-agent capabilities. It is deliberately separate
/// from `agents/`, which may be a symlink to the user's existing Codex roles.
const GENERATED_AGENTS_DIR: &str = "granite-cli-agents";

/// Entries in the Codex home that Codex creates as directories (or sockets)
/// at runtime.  Linking these from the user's profile would collide with
/// Codex's own `mkdir` / `bind` calls, so we skip them during pass-through.
const CODEX_RUNTIME_ENTRIES: &[&str] = &["app-server-daemon", "app-server-control"];

/// The granite-cli-owned Codex home directory for this launcher instance.
fn codex_state_dir(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    crate::config::Config::launcher_state_dir(&ctx.launcher_id)
}

/// The user's own Codex home directory, which we only ever read from:
/// `$CODEX_HOME` when set, else `~/.codex`.
fn codex_source_dir() -> anyhow::Result<PathBuf> {
    if let Ok(val) = std::env::var(CODEX_HOME_ENV)
        && !val.is_empty()
    {
        return Ok(PathBuf::from(val));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory for Codex's config"))?;
    Ok(home.join(".codex"))
}

/// Builds `state_dir` into a usable Codex home: pass-through symlinks for
/// every entry in the user's real Codex home, plus a freshly written
/// `config.toml` that granite-cli owns.  Nothing under `source_dir` is written.
fn materialize_codex_config(
    state_dir: &Path,
    source_dir: &Path,
    config_toml: &str,
    sub_agent_files: &[(PathBuf, String)],
    ui: &dyn Ui,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("Failed to create {}", state_dir.display()))?;

    // Remove any stale symlinks for runtime entries left over from a previous
    // launch.  Codex creates these as real directories/sockets at startup; a
    // dangling or live symlink at the same path blocks that creation.
    for name in CODEX_RUNTIME_ENTRIES {
        let path = state_dir.join(name);
        if std::fs::symlink_metadata(&path).is_ok_and(|md| md.file_type().is_symlink()) {
            let _ = std::fs::remove_file(&path);
        }
    }

    // A nested launch lands here with source == state; linking a directory
    // into itself is meaningless, and config.toml is already ours.
    if !same_dir(state_dir, source_dir) {
        link_pass_through_resources(state_dir, source_dir, ui);
    }

    write_owned_toml(&state_dir.join(CODEX_CONFIG_FILE), config_toml)?;
    for (path, content) in sub_agent_files {
        write_owned_toml(path, content)?;
    }
    Ok(())
}

/// Links every top-level entry of the user's Codex home into `state_dir`,
/// so their auth credentials, memories, sessions, etc. still apply.
/// `config.toml` is skipped — granite-cli generates its own.
///
/// Best-effort: a platform or permission that refuses symlinks costs the user
/// those resources for granite-cli launches, not the launch itself.
fn link_pass_through_resources(state_dir: &Path, source_dir: &Path, ui: &dyn Ui) {
    let entries = match std::fs::read_dir(source_dir) {
        Ok(entries) => entries,
        // No Codex profile of their own yet — nothing to pass through.
        Err(_) => return,
    };

    let mut failed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == CODEX_CONFIG_FILE {
            continue;
        }
        if CODEX_RUNTIME_ENTRIES
            .iter()
            .any(|r| std::ffi::OsStr::new(r) == name)
        {
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
            "Could not link {failed} Codex resource(s) from {} into {}; \
             auth and memories from there will not apply to this launch.",
            source_dir.display(),
            state_dir.display()
        ));
    }
}

/// Writes a file granite-cli owns outright.  Any symlink already at `path` is
/// removed first — writing through one would land in whatever it points at,
/// which is exactly the user's file we are trying not to touch.
fn write_owned_toml(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|md| md.file_type().is_symlink()) {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to replace symlink at {}", path.display()))?;
    }
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

impl CodexLauncher {
    /// Builds the `config.toml` content for the bound model and MCP servers.
    ///
    /// Codex's TOML schema:
    /// ```toml
    /// model_provider = "<id>"
    /// model          = "<name>"
    ///
    /// [model_providers.<id>]
    /// name     = "<display name>"
    /// base_url = "<url>"
    /// env_key  = "GRANITE_CLI_CODEX_API_KEY"
    /// wire_api = "responses"
    ///
    /// [mcp_servers.<name>]          # stdio
    /// command = "<cmd>"
    /// args    = [...]
    /// env     = { KEY = "val" }
    ///
    /// [mcp_servers.<name>]          # http / sse
    /// url     = "<url>"
    /// ```
    fn generate_config_toml(&self, ctx: &LaunchContext) -> anyhow::Result<String> {
        let mut out = String::new();

        if let Some(binding) = &self.bound_agent_model {
            let provider_id = toml_key(&safe_provider_id(&binding.provider_name));
            let base_url = codex_base_url(
                ctx.model_proxy
                    .as_ref()
                    .map(|handle| handle.local_base_url.as_str()),
                binding,
            );

            out.push_str(&format!("model_provider = {}\n", toml_string(&provider_id)));
            out.push_str(&format!(
                "model          = {}\n",
                toml_string(&binding.model_name)
            ));
            out.push('\n');
            out.push_str(&format!("[model_providers.{provider_id}]\n"));
            out.push_str(&format!(
                "name     = {}\n",
                toml_string(&binding.provider_name)
            ));
            out.push_str(&format!("base_url = {}\n", toml_string(&base_url)));

            let has_key = binding.api_key.as_ref().is_some_and(|k| !k.0.is_empty());
            if has_key {
                out.push_str(&format!("env_key  = {}\n", toml_string(API_KEY_ENV)));
            }

            // Apply model_overrides: any key overrides the value we'd write
            // for the [model_providers.<id>] table, except `wire_api`: Codex
            // 0.160+ only accepts its Responses API protocol.
            if let Some(overrides) = self
                .config
                .model_overrides
                .as_ref()
                .and_then(serde_json::Value::as_object)
            {
                for (key, value) in overrides {
                    if key == "wire_api" {
                        if value.as_str() != Some("responses") {
                            anyhow::bail!(
                                "Codex requires wire_api = \"responses\"; \"chat\" is unsupported by current Codex versions"
                            );
                        }
                    } else {
                        out.push_str(&format!(
                            "{} = {}\n",
                            toml_key(key),
                            json_value_to_toml(value)
                        ));
                    }
                }
            }

            out.push_str("wire_api = \"responses\"\n");
        }

        for (name, binding) in &self.bound_mcp_bindings {
            out.push('\n');
            out.push_str(&format!("[mcp_servers.{}]\n", toml_key(name)));
            match binding {
                McpBinding::Stdio {
                    command, args, env, ..
                } => {
                    out.push_str(&format!("command = {}\n", toml_string(command)));
                    if !args.is_empty() {
                        let args_toml: Vec<String> = args.iter().map(|a| toml_string(a)).collect();
                        out.push_str(&format!("args    = [{}]\n", args_toml.join(", ")));
                    }
                    if !env.is_empty() {
                        out.push_str("[mcp_servers.");
                        out.push_str(&toml_key(name));
                        out.push_str(".env]\n");
                        // Sort for deterministic output.
                        let mut pairs: Vec<_> = env.iter().collect();
                        pairs.sort_by_key(|(k, _)| k.as_str());
                        for (k, v) in pairs {
                            out.push_str(&format!("{} = {}\n", toml_key(k), toml_string(v)));
                        }
                    }
                }
                McpBinding::Http { url, headers, .. } | McpBinding::Sse { url, headers, .. } => {
                    out.push_str(&format!("url = {}\n", toml_string(url)));
                    if !headers.is_empty() {
                        out.push_str("[mcp_servers.");
                        out.push_str(&toml_key(name));
                        out.push_str(".headers]\n");
                        let mut pairs: Vec<_> = headers.iter().collect();
                        pairs.sort_by_key(|(k, _)| k.as_str());
                        for (k, v) in pairs {
                            out.push_str(&format!("{} = {}\n", toml_key(k), toml_string(v)));
                        }
                    }
                }
            }
        }

        for (index, (name, binding)) in self.bound_sub_agents.iter().enumerate() {
            let provider_id = toml_key(&safe_provider_id(&binding.model.provider_name));
            let base_url = codex_base_url(
                ctx.model_proxy
                    .as_ref()
                    .map(|handle| handle.local_base_url.as_str()),
                &binding.model,
            );
            let role_path = codex_state_dir(ctx)?
                .join(GENERATED_AGENTS_DIR)
                .join(format!("{index}.toml"));

            out.push('\n');
            out.push_str(&format!("[agents.{}]\n", toml_key(name)));
            out.push_str(&format!(
                "description = {}\n",
                toml_string(&binding.description)
            ));
            out.push_str(&format!(
                "config_file = {}\n",
                toml_string(&role_path.to_string_lossy())
            ));

            // Codex role files select a provider by its configured id. Add a
            // provider table for every sub-agent model; a duplicate id is
            // harmless when it denotes the same endpoint and lets several
            // roles share one provider definition.
            if self.bound_agent_model.as_ref().is_none_or(|main| {
                safe_provider_id(&main.provider_name)
                    != safe_provider_id(&binding.model.provider_name)
            }) && !self.bound_sub_agents[..index].iter().any(|(_, earlier)| {
                safe_provider_id(&earlier.model.provider_name)
                    == safe_provider_id(&binding.model.provider_name)
            }) {
                out.push('\n');
                out.push_str(&format!("[model_providers.{provider_id}]\n"));
                out.push_str(&format!(
                    "name     = {}\n",
                    toml_string(&binding.model.provider_name)
                ));
                out.push_str(&format!("base_url = {}\n", toml_string(&base_url)));
                if binding
                    .model
                    .api_key
                    .as_ref()
                    .is_some_and(|key| !key.0.is_empty())
                {
                    out.push_str(&format!(
                        "env_key  = {}\n",
                        toml_string(&sub_agent_api_key_env(&binding.model.provider_name))
                    ));
                }
                out.push_str("wire_api = \"responses\"\n");
            }
        }

        // Extra providers: additional [model_providers.<id>] tables for
        // Codex custom agents to reference by name.
        if !self.config.extra_providers.is_empty() {
            // Sort for deterministic output.
            let mut providers: Vec<_> = self.config.extra_providers.iter().collect();
            providers.sort_by_key(|(id, _)| id.as_str());
            for (id, fields) in providers {
                if fields
                    .get("wire_api")
                    .is_some_and(|value| value.as_str() != Some("responses"))
                {
                    anyhow::bail!(
                        "Codex extra provider '{id}' must use wire_api = \"responses\"; \"chat\" is unsupported by current Codex versions"
                    );
                }
                let safe_id = safe_provider_id(id);
                out.push('\n');
                out.push_str(&format!("[model_providers.{}]\n", toml_key(&safe_id)));
                if let Some(obj) = fields.as_object() {
                    // Sort fields for deterministic output.
                    let mut pairs: Vec<_> = obj.iter().collect();
                    pairs.sort_by_key(|(k, _)| k.as_str());
                    for (k, v) in pairs {
                        out.push_str(&format!("{} = {}\n", toml_key(k), json_value_to_toml(v)));
                    }
                }
            }
        }

        Ok(out)
    }

    /// Produces native Codex role files for the bound sub-agents. Roles are
    /// registered from the generated root config so this never overwrites or
    /// obscures files in the user's `agents/` directory. Codex's `tools`
    /// field is a structured feature configuration, not a tool-name allow
    /// list, so roles inherit the session's available tools instead.
    fn generate_sub_agent_files(&self, state_dir: &Path) -> anyhow::Result<Vec<(PathBuf, String)>> {
        Ok(self
            .bound_sub_agents
            .iter()
            .enumerate()
            .map(|(index, (name, binding))| {
                let role_name = codex_agent_name(name);
                let provider_id = safe_provider_id(&binding.model.provider_name);
                let out = format!(
                    "name = {}\ndescription = {}\ndeveloper_instructions = {}\nmodel = {}\nmodel_provider = {}\n",
                    toml_string(&role_name),
                    toml_string(&binding.description),
                    toml_multiline_literal(&binding.prompt),
                    toml_string(&binding.model.model_name),
                    toml_string(&provider_id),
                );
                (
                    state_dir
                        .join(GENERATED_AGENTS_DIR)
                        .join(format!("{index}.toml")),
                    out,
                )
            })
            .collect())
    }
}

/// Codex ships with built-in providers under these IDs; using the same name in
/// `[model_providers]` is rejected by Codex's validator.  When the configured
/// granite-cli provider instance has one of these names we append `-provider`
/// so the generated config is always accepted.
const CODEX_RESERVED_PROVIDER_IDS: &[&str] = &[
    "anthropic",
    "deepseek",
    "gemini",
    "mistral",
    "ollama",
    "openai",
    "xai",
];

fn safe_provider_id(name: &str) -> String {
    if CODEX_RESERVED_PROVIDER_IDS.contains(&name) {
        format!("{name}-provider")
    } else {
        name.to_string()
    }
}

/// Codex builds its request URL as `{base_url}/responses` or
/// `{base_url}/chat/completions` directly -- it never inserts a version
/// segment itself (the official `openai` provider's base URL already ends in
/// `/v1`; see https://github.com/openai/codex/discussions/7782). The
/// provider's own `base_url` (e.g. `http://localhost:11434`) has no such
/// segment, so it must come from the resolved endpoint path instead -- the
/// part before whichever of these two suffixes it actually ends with.
fn wire_prefix(endpoint_path: &str) -> &str {
    endpoint_path
        .strip_suffix("/responses")
        .or_else(|| endpoint_path.strip_suffix("/chat/completions"))
        .unwrap_or("")
}

/// Returns the API root Codex should call. A session proxy replaces only the
/// host; the API-version prefix still comes from the resolved provider
/// endpoint and must be retained for the proxy router to receive `/v1/...`.
fn codex_base_url(proxy_base_url: Option<&str>, binding: &AgentModelBinding) -> String {
    let base_url = proxy_base_url.unwrap_or(&binding.base_url);
    format!("{base_url}{}", wire_prefix(&binding.endpoint_path))
}

fn sub_agent_api_key_env(provider_name: &str) -> String {
    let normalized: String = provider_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GRANITE_CLI_CODEX_{normalized}_API_KEY")
}

fn codex_agent_name(name: &str) -> String {
    let normalized: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    if normalized.trim().is_empty() {
        "granite-cli-sub-agent".to_string()
    } else {
        normalized
    }
}

/// Wraps a string in TOML double-quotes, escaping quote, backslash, and
/// control characters that are not valid literally inside a basic string.
fn toml_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

/// Wraps a prompt in a TOML multiline literal string. Codex's agent-role
/// loader preserves this form correctly, while its loader currently rejects
/// newline escapes in `developer_instructions`.
fn toml_multiline_literal(s: &str) -> String {
    // TOML literal strings cannot escape their delimiter. Prompts normally do
    // not contain three adjacent apostrophes; retain the standard escaped
    // representation for that uncommon case so every prompt remains writable.
    if s.contains("'''") {
        toml_string(s)
    } else {
        format!("'''{s}'''")
    }
}

/// Produces a bare TOML key.  TOML allows bare keys with `[A-Za-z0-9_-]`;
/// anything else gets double-quoted.
fn toml_key(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        s.to_string()
    } else {
        toml_string(s)
    }
}

/// Converts a `serde_json::Value` to a TOML inline scalar.  Only booleans,
/// numbers, and strings are expected here (override values from user config).
fn json_value_to_toml(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => toml_string(s),
        other => toml_string(&other.to_string()),
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::ToolName;

    fn launcher() -> CodexLauncher {
        CodexLauncher::new("my-codex", &serde_json::json!({})).unwrap()
    }

    fn launcher_with_model(binding: AgentModelBinding) -> CodexLauncher {
        let mut l = launcher();
        l.bound_agent_model = Some(binding);
        l
    }

    fn test_binding() -> AgentModelBinding {
        AgentModelBinding {
            api_type: crate::providers::ApiType::OpenAI,
            provider_name: "my-openai".to_string(),
            base_url: "http://localhost:11434".to_string(),
            model_name: "granite4.1:8b".to_string(),
            endpoint_path: "/v1/responses".to_string(),
            api_key: Some(crate::registry::Secret::from("test-key")),
            verify_ssl: true,
            context_length: Some(131072),
            custom_headers: None,
        }
    }

    fn test_launch_context(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "my-codex".to_string(),
            working_dir: std::env::current_dir().unwrap(),
            base_env: std::collections::HashMap::new(),
            dry_run,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    // --- command / validate_command ---

    #[test]
    fn command_defaults_to_codex() {
        assert_eq!(launcher().command(), "codex");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = CodexLauncher::new(
            "my-codex",
            &serde_json::json!({ "command_path": "/opt/bin/codex" }),
        )
        .unwrap();
        assert_eq!(l.command(), "/opt/bin/codex");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = CodexLauncher::new(
            "my-codex",
            &serde_json::json!({ "command_path": "/no/such/path/codex" }),
        )
        .unwrap();
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l =
            CodexLauncher::new("my-codex", &serde_json::json!({ "command_path": "ls" })).unwrap();
        assert!(l.validate_command().is_ok());
    }

    // --- metadata ---

    #[test]
    fn metadata_name_is_codex_cli() {
        let meta = CodexLauncher::metadata();
        assert_eq!(meta.name, "Codex CLI");
        assert_eq!(meta.default_command, "codex");
    }

    #[test]
    fn metadata_supports_agent_model_mcp_and_sub_agent() {
        let meta = CodexLauncher::metadata();
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
        assert!(meta.supported_capabilities.contains(&BindingType::SubAgent));
    }

    #[test]
    fn config_schema_is_present_with_command_path_and_model_overrides_properties() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<CodexLauncher>("codex");
        let schema = factory.config_schema("codex").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("model_overrides"));
    }

    // --- generate_config_toml ---

    #[test]
    fn generate_config_toml_sets_model_provider_and_model() {
        let l = launcher_with_model(test_binding());
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("model_provider = \"my-openai\""),
            "expected model_provider: {toml}"
        );
        assert!(
            toml.contains("model          = \"granite4.1:8b\""),
            "expected model: {toml}"
        );
    }

    #[test]
    fn generate_config_toml_writes_provider_table() {
        let l = launcher_with_model(test_binding());
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("[model_providers.my-openai]"),
            "expected provider table: {toml}"
        );
        assert!(
            toml.contains("base_url = \"http://localhost:11434/v1\""),
            "expected base_url with /v1 prefix derived from endpoint_path: {toml}"
        );
        assert!(
            toml.contains(&format!("env_key  = \"{API_KEY_ENV}\"")),
            "expected env_key: {toml}"
        );
        assert!(
            toml.contains("wire_api = \"responses\""),
            "expected wire_api: {toml}"
        );
    }

    #[test]
    fn generate_config_toml_omits_env_key_when_no_api_key() {
        let b = AgentModelBinding {
            api_key: None,
            ..test_binding()
        };
        let l = launcher_with_model(b);
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(!toml.contains("env_key"), "unexpected env_key: {toml}");
    }

    #[test]
    fn generate_config_toml_sanitizes_reserved_provider_id() {
        // "ollama" is a Codex built-in; the generated id must not collide.
        let b = AgentModelBinding {
            provider_name: "ollama".to_string(),
            ..test_binding()
        };
        let l = launcher_with_model(b);
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            !toml.contains("[model_providers.ollama]"),
            "must not emit reserved id: {toml}"
        );
        assert!(
            toml.contains("[model_providers.ollama-provider]"),
            "expected suffixed id: {toml}"
        );
        assert!(
            toml.contains("model_provider = \"ollama-provider\""),
            "top-level model_provider must match: {toml}"
        );
    }

    #[test]
    fn generate_config_toml_writes_extra_providers() {
        let l = CodexLauncher::new(
            "my-codex",
            &serde_json::json!({
                "extra_providers": {
                    "local-ollama": {
                        "name": "Local Ollama",
                        "base_url": "http://localhost:11434",
                        "wire_api": "responses"
                    }
                }
            }),
        )
        .unwrap();
        let mut l = l;
        l.bound_agent_model = Some(test_binding());
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("[model_providers.local-ollama]"),
            "expected extra provider table: {toml}"
        );
        assert!(
            toml.contains("base_url = \"http://localhost:11434\""),
            "expected extra provider base_url: {toml}"
        );
        assert!(
            toml.contains("wire_api = \"responses\""),
            "expected extra provider wire_api: {toml}"
        );
    }

    #[test]
    fn generate_config_toml_sanitizes_reserved_id_in_extra_providers() {
        // "ollama" is reserved; the extra provider id must be suffixed.
        let l = CodexLauncher::new(
            "my-codex",
            &serde_json::json!({
                "extra_providers": {
                    "ollama": {
                        "base_url": "http://localhost:11434",
                        "wire_api": "responses"
                    }
                }
            }),
        )
        .unwrap();
        let mut l = l;
        l.bound_agent_model = Some(test_binding());
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            !toml.contains("[model_providers.ollama]"),
            "must not emit reserved id in extra providers: {toml}"
        );
        assert!(
            toml.contains("[model_providers.ollama-provider]"),
            "expected suffixed extra provider id: {toml}"
        );
    }

    #[test]
    fn generate_config_toml_rejects_chat_wire_api_override() {
        let l = CodexLauncher::new(
            "my-codex",
            &serde_json::json!({ "model_overrides": { "wire_api": "chat" } }),
        )
        .unwrap();
        let mut l = l;
        l.bound_agent_model = Some(test_binding());
        let err = l
            .generate_config_toml(&test_launch_context(false))
            .unwrap_err();
        assert!(err.to_string().contains("wire_api = \"responses\""));
    }

    #[test]
    fn generate_config_toml_uses_responses_wire_api_for_chat_completions_endpoint() {
        // Codex 0.160+ always sends Responses requests, even when Granite
        // resolved the provider through its Chat Completions fallback.
        let b = AgentModelBinding {
            endpoint_path: "/v1/chat/completions".to_string(),
            ..test_binding()
        };
        let l = launcher_with_model(b);
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("wire_api = \"responses\""),
            "expected wire_api=responses for chat-completions endpoint: {toml}"
        );
    }

    #[test]
    fn generate_config_toml_uses_responses_wire_api_for_responses_endpoint() {
        // A provider whose first matching endpoint is /v1/responses must
        // produce `wire_api = "responses"`.
        let b = AgentModelBinding {
            endpoint_path: "/v1/responses".to_string(),
            ..test_binding()
        };
        let l = launcher_with_model(b);
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("wire_api = \"responses\""),
            "expected wire_api=responses for responses endpoint: {toml}"
        );
    }

    #[test]
    fn generated_sub_agent_registers_a_native_codex_role_and_provider() {
        let mut l = launcher();
        l.bound_sub_agents.push((
            "reviewer".to_string(),
            SubAgentBinding {
                description: "Reviews code".to_string(),
                prompt:
                    "Review the code carefully.\n\n=== READ-ONLY MODE ===\nDo not modify files."
                        .to_string(),
                tools: vec![ToolName::FileRead, ToolName::Search],
                model: AgentModelBinding {
                    provider_name: "review-provider".to_string(),
                    model_name: "review-model".to_string(),
                    api_key: Some(crate::registry::Secret::from("review-key")),
                    ..test_binding()
                },
                known_type: None,
            },
        ));

        let ctx = test_launch_context(false);
        let config = l.generate_config_toml(&ctx).unwrap();
        let state_dir = codex_state_dir(&ctx).unwrap();
        let roles = l.generate_sub_agent_files(&state_dir).unwrap();

        assert!(config.contains("[agents.reviewer]"), "{config}");
        assert!(
            config.contains("[model_providers.review-provider]"),
            "{config}"
        );
        assert!(config.contains("GRANITE_CLI_CODEX_REVIEW_PROVIDER_API_KEY"));
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].0, state_dir.join("granite-cli-agents/0.toml"));
        assert!(roles[0].1.contains("model = \"review-model\""));
        assert!(roles[0].1.contains("model_provider = \"review-provider\""));
        assert!(
            !roles[0].1.contains("tools ="),
            "Codex role TOML does not accept a list-based tools allow-list: {}",
            roles[0].1
        );
        assert!(
            roles[0]
                .1
                .contains("developer_instructions = '''Review the code carefully.\n\n=== READ-ONLY MODE ===\nDo not modify files.'''"),
            "multiline instructions must use a TOML multiline literal: {}",
            roles[0].1
        );
    }

    #[test]
    fn multiline_literal_falls_back_to_an_escaped_string_when_needed() {
        assert_eq!(
            toml_multiline_literal("instruction with ''' delimiter"),
            "\"instruction with ''' delimiter\""
        );
    }

    #[test]
    fn generate_config_toml_writes_stdio_mcp_server() {
        let mut l = launcher();
        l.bound_mcp_bindings.push((
            "vision".to_string(),
            McpBinding::Stdio {
                command: "/usr/local/bin/granite-cli".to_string(),
                args: vec!["__mcp-serve".to_string(), "vision".to_string()],
                env: std::collections::HashMap::from([("FOO".to_string(), "bar".to_string())]),
                timeout: None,
            },
        ));
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("[mcp_servers.vision]"),
            "expected mcp table: {toml}"
        );
        assert!(
            toml.contains("command = \"/usr/local/bin/granite-cli\""),
            "expected command: {toml}"
        );
        assert!(toml.contains("\"__mcp-serve\""), "expected arg: {toml}");
        assert!(
            toml.contains("[mcp_servers.vision.env]"),
            "expected env subtable: {toml}"
        );
        assert!(toml.contains("FOO = \"bar\""), "expected env entry: {toml}");
    }

    #[test]
    fn generate_config_toml_writes_http_mcp_server() {
        let mut l = launcher();
        l.bound_mcp_bindings.push((
            "remote".to_string(),
            McpBinding::Http {
                url: "http://127.0.0.1:9000/mcp".to_string(),
                headers: std::collections::HashMap::from([(
                    "Authorization".to_string(),
                    "Bearer x".to_string(),
                )]),
                timeout: None,
            },
        ));
        let toml = l.generate_config_toml(&test_launch_context(false)).unwrap();
        assert!(
            toml.contains("[mcp_servers.remote]"),
            "expected mcp table: {toml}"
        );
        assert!(
            toml.contains("url = \"http://127.0.0.1:9000/mcp\""),
            "expected url: {toml}"
        );
        assert!(
            toml.contains("[mcp_servers.remote.headers]"),
            "expected headers subtable: {toml}"
        );
        assert!(
            toml.contains("Authorization = \"Bearer x\""),
            "expected header: {toml}"
        );
    }

    // --- env_overlay ---

    #[tokio::test]
    async fn env_overlay_is_empty_without_bound_model() {
        let overlay = launcher()
            .env_overlay(&test_launch_context(false))
            .await
            .unwrap();
        assert!(overlay.is_empty());
    }

    #[tokio::test]
    async fn env_overlay_sets_codex_home_and_api_key() {
        let l = launcher_with_model(test_binding());
        let overlay = l.env_overlay(&test_launch_context(false)).await.unwrap();

        let get = |key: &str| {
            overlay
                .iter()
                .find(|b| b.key == key)
                .map(|b| b.value.as_str())
        };

        assert!(
            get(CODEX_HOME_ENV).is_some(),
            "expected CODEX_HOME in overlay"
        );
        assert!(
            get(CODEX_HOME_ENV).unwrap().contains("launcher-state"),
            "CODEX_HOME should point into launcher-state"
        );
        assert_eq!(get(API_KEY_ENV), Some("test-key"));
        assert_eq!(get("OPENAI_BASE_URL"), Some("http://localhost:11434/v1"));
    }

    #[tokio::test]
    async fn env_overlay_omits_api_key_when_none() {
        let binding = AgentModelBinding {
            api_key: None,
            ..test_binding()
        };
        let l = launcher_with_model(binding);
        let overlay = l.env_overlay(&test_launch_context(false)).await.unwrap();
        assert!(
            overlay.iter().all(|b| b.key != API_KEY_ENV),
            "expected no api key env var: {overlay:?}"
        );
    }

    #[test]
    fn codex_base_url_preserves_api_prefix_when_proxy_is_active() {
        assert_eq!(
            codex_base_url(Some("http://127.0.0.1:9876"), &test_binding()),
            "http://127.0.0.1:9876/v1"
        );
    }

    // --- map_tool_name ---

    #[test]
    fn map_tool_name_covers_every_canonical_variant_and_formats_mcp_references() {
        let l = launcher();
        assert_eq!(
            l.map_tool_name(&ToolName::FileRead),
            Some("read_file".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileWrite),
            Some("write_file".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileEdit),
            Some("apply_patch".to_string())
        );
        assert_eq!(l.map_tool_name(&ToolName::Search), Some("grep".to_string()));
        assert_eq!(
            l.map_tool_name(&ToolName::FileSearch),
            Some("find_files".to_string())
        );
        assert_eq!(l.map_tool_name(&ToolName::Shell), Some("shell".to_string()));
        assert_eq!(
            l.map_tool_name(&ToolName::WebFetch),
            Some("web_fetch".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::WebSearch),
            Some("web_search".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: None,
            }),
            Some("vision.*".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: Some("vlm_compare_images".to_string()),
            }),
            Some("vision.vlm_compare_images".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Other("my_custom_tool".to_string())),
            Some("my_custom_tool".to_string())
        );
    }

    // --- bind_capability ---

    #[tokio::test]
    async fn bind_capability_stores_sub_agent_binding() {
        struct FakeSubAgentCapability;

        impl crate::registry::Named for FakeSubAgentCapability {
            fn instance_id(&self) -> &str {
                "fake-sub-agent"
            }
        }

        impl crate::capabilities::CapabilityInfo for FakeSubAgentCapability {
            fn name(&self) -> &str {
                "Fake"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn binding_types(&self) -> HashSet<BindingType> {
                HashSet::from([BindingType::SubAgent])
            }
        }

        #[async_trait]
        impl ResolvedCapability for FakeSubAgentCapability {
            async fn bind(
                &self,
                request: crate::capabilities::BindingRequest,
            ) -> anyhow::Result<Binding> {
                let crate::capabilities::BindingRequest::SubAgent(request) = request else {
                    anyhow::bail!("expected a sub-agent request");
                };
                Ok(Binding::SubAgent(SubAgentBinding {
                    description: "Reviews code".to_string(),
                    prompt: "Review the code carefully.".to_string(),
                    tools: vec![ToolName::FileRead],
                    model: AgentModelBinding {
                        api_type: request.api_type,
                        provider_name: "review-provider".to_string(),
                        base_url: "http://localhost:11434".to_string(),
                        model_name: "review-model".to_string(),
                        endpoint_path: "/v1/responses".to_string(),
                        api_key: Some(crate::registry::Secret::from("review-key")),
                        verify_ssl: true,
                        context_length: Some(4096),
                        custom_headers: None,
                    },
                    known_type: None,
                }))
            }
        }

        let mut l = launcher();
        l.bind_capability(&FakeSubAgentCapability).await.unwrap();
        assert_eq!(l.bound_sub_agents.len(), 1);
        assert_eq!(l.bound_sub_agents[0].0, "fake-sub-agent");
        assert_eq!(l.bound_sub_agents[0].1.model.model_name, "review-model");
    }

    #[tokio::test]
    async fn bind_capability_stores_agent_model_binding() {
        struct FakeAgentModelCapability {
            binding: AgentModelBinding,
        }

        impl crate::registry::Named for FakeAgentModelCapability {
            fn instance_id(&self) -> &str {
                "fake-model"
            }
        }

        impl crate::capabilities::CapabilityInfo for FakeAgentModelCapability {
            fn name(&self) -> &str {
                "Fake"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn binding_types(&self) -> HashSet<BindingType> {
                HashSet::from([BindingType::AgentModel])
            }
        }

        #[async_trait]
        impl ResolvedCapability for FakeAgentModelCapability {
            async fn bind(
                &self,
                _request: crate::capabilities::BindingRequest,
            ) -> anyhow::Result<Binding> {
                Ok(Binding::AgentModel(self.binding.clone()))
            }
        }

        let mut l = launcher();
        let cap = FakeAgentModelCapability {
            binding: test_binding(),
        };
        l.bind_capability(&cap).await.unwrap();
        assert!(l.bound_agent_model.is_some());
        assert_eq!(
            l.bound_agent_model.as_ref().unwrap().model_name,
            "granite4.1:8b"
        );
    }

    // --- materialize_codex_config / pass-through linking ---

    fn dirs(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let source = tmp.path().join("user-codex");
        std::fs::create_dir_all(&source).unwrap();
        (tmp.path().join("state"), source)
    }

    #[test]
    fn materialize_writes_config_and_never_touches_source_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join("config.toml"), "model = \"user-model\"\n").unwrap();

        materialize_codex_config(
            &state,
            &source,
            "model = \"granite4.1:8b\"\n",
            &[],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(source.join("config.toml")).unwrap(),
            "model = \"user-model\"\n"
        );
        assert_eq!(
            std::fs::read_to_string(state.join("config.toml")).unwrap(),
            "model = \"granite4.1:8b\"\n"
        );
    }

    #[test]
    fn materialize_writes_generated_roles_outside_the_user_agents_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::create_dir(source.join("agents")).unwrap();
        std::fs::write(source.join("agents/user.toml"), "name = \"user\"").unwrap();
        let role_path = state.join("granite-cli-agents/0.toml");

        materialize_codex_config(
            &state,
            &source,
            "model = \"x\"\n",
            &[(role_path.clone(), "name = \"reviewer\"\n".to_string())],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&role_path).unwrap(),
            "name = \"reviewer\"\n"
        );
        assert!(
            std::fs::symlink_metadata(state.join("agents"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the user's agents directory must remain linked through"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_links_user_resources_but_not_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::write(source.join("auth.json"), "{}").unwrap();
        std::fs::create_dir(source.join("sessions")).unwrap();
        std::fs::write(source.join("config.toml"), "model = \"old\"\n").unwrap();

        materialize_codex_config(
            &state,
            &source,
            "model = \"new\"\n",
            &[],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        for linked in ["auth.json", "sessions"] {
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
        }
        // config.toml is ours, not a link into the user's directory.
        assert!(
            !std::fs::symlink_metadata(state.join("config.toml"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_does_not_link_runtime_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        // Simulate a user profile that contains runtime entries.
        std::fs::create_dir(source.join("app-server-daemon")).unwrap();
        std::fs::create_dir(source.join("app-server-control")).unwrap();
        std::fs::write(source.join("auth.json"), "{}").unwrap();

        materialize_codex_config(
            &state,
            &source,
            "model = \"x\"\n",
            &[],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        // Runtime entries must NOT appear in the state dir at all.
        assert!(
            !state.join("app-server-daemon").exists(),
            "app-server-daemon must not be linked"
        );
        assert!(
            !state.join("app-server-control").exists(),
            "app-server-control must not be linked"
        );
        // Regular resources still pass through.
        assert!(
            std::fs::symlink_metadata(state.join("auth.json"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "auth.json should be a symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_removes_stale_runtime_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (state, source) = dirs(&tmp);
        std::fs::create_dir_all(&state).unwrap();
        // Pre-place a stale symlink as if left over from a prior launch.
        std::os::unix::fs::symlink(
            source.join("app-server-daemon"),
            state.join("app-server-daemon"),
        )
        .unwrap();

        materialize_codex_config(
            &state,
            &source,
            "model = \"x\"\n",
            &[],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        // The symlink must have been removed so Codex can create a real dir there.
        assert!(
            std::fs::symlink_metadata(state.join("app-server-daemon")).is_err(),
            "stale app-server-daemon symlink should have been removed"
        );
    }

    #[test]
    fn materialize_works_with_no_user_codex_home_at_all() {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let source = tmp.path().join("does-not-exist");

        materialize_codex_config(
            &state,
            &source,
            "model = \"x\"\n",
            &[],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(state.join("config.toml")).unwrap(),
            "model = \"x\"\n"
        );
    }

    #[test]
    fn materialize_tolerates_source_equal_to_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("both");
        std::fs::create_dir_all(&dir).unwrap();

        materialize_codex_config(
            &dir,
            &dir,
            "model = \"x\"\n",
            &[],
            &crate::utils::ui::base::tests::CaptureUi::default(),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).unwrap(),
            "model = \"x\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_owned_toml_replaces_a_symlink_instead_of_writing_through_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let victim = tmp.path().join("users-real-config.toml");
        std::fs::write(&victim, "SACRED").unwrap();
        let link = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        write_owned_toml(&link, "ours = true\n").unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "SACRED");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "ours = true\n");
    }

    // --- dry-run launch ---

    // Deliberately reads whatever `GRANITE_CLI_HOME` is ambient rather than
    // setting it: env mutation would race the other tests in this binary.
    #[tokio::test]
    async fn dry_run_launch_reports_without_writing_anything() {
        let state_dir = crate::config::Config::launcher_state_dir("my-codex").unwrap();
        let existed_before = state_dir.exists();

        let l = launcher_with_model(test_binding());
        let mut l = l;
        l.config.command_path = Some("ls".to_string());
        let ui = crate::utils::ui::base::tests::CaptureUi::default();
        let status = l
            .launch(&["--help".to_string()], &test_launch_context(true), &ui)
            .await
            .unwrap();
        assert!(status.success());

        let infos = ui.infos.borrow();
        assert!(
            infos.iter().any(|m| m.contains("Would write Codex config")),
            "expected dry-run notice, got {infos:?}"
        );
        assert!(
            infos.iter().any(|m| m.contains("model_provider")),
            "expected toml content in output, got {infos:?}"
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
        let l =
            CodexLauncher::new("my-codex", &serde_json::json!({ "command_path": "ls" })).unwrap();
        let ui = crate::utils::ui::base::tests::CaptureUi::default();
        l.launch(&["--version".to_string()], &test_launch_context(true), &ui)
            .await
            .unwrap();

        let infos = ui.infos.borrow();
        assert!(
            !infos.iter().any(|m| m.contains("Would write Codex config")),
            "expected no config write without binding"
        );
        assert!(!infos.iter().any(|m| m.contains(CODEX_HOME_ENV)));
    }
}
