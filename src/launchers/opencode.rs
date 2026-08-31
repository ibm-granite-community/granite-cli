//! Launcher for the `opencode` coding agent (<https://opencode.ai>).
//!
//! Unlike `pi`, OpenCode's config sources are additive: `OPENCODE_CONFIG`
//! points at one extra file that gets merged into the chain (loaded after the
//! global config, before the project config) rather than a whole directory to
//! redirect wholesale. This launcher reads the user's global and project
//! `opencode.json` configs to discover configured providers, registers them on
//! the session proxy for usage tracking, then writes its own small generated
//! file under `GRANITE_CLI_HOME` and points `OPENCODE_CONFIG` at it. The
//! generated config includes granite-cli providers plus any user providers
//! merged in with their `baseURL` redirected to the proxy.

// Standard
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    AgentModelBinding, Binding, BindingType, Capability, KnownSubAgent, McpBinding,
    SubAgentBinding, ToolName,
};
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::mcp_binding_request;
use crate::providers::ApiType;
use crate::proxy::ProxyHandle;
use crate::registry::ConfigConstructable;
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;

use_channel!("OPNCD");

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct OpenCodeLauncherConfig {
    /// Override path to the `opencode` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Extra keys merged (shallow, last-write-wins) into every generated
    /// provider entry -- e.g. `headers` a particular server needs. Necessary
    /// because the entries are regenerated on every launch. Applied
    /// uniformly to the main model's provider entry *and* every bound
    /// sub-agent's, since there is no per-sub-agent override knob yet -- an
    /// override meant for one provider (e.g. a dialect-specific `npm`
    /// package) will also land on any other bound provider.
    #[serde(default)]
    pub provider_overrides: Option<serde_json::Value>,
}

pub struct OpenCodeLauncher {
    instance_id: String,
    config: OpenCodeLauncherConfig,
    bound_agent_model: Option<AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, written into the generated config's `mcp` block.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// `(name, binding)` for every `SubAgentCapability` bound to this
    /// launcher -- `name` is the capability's own `instance_id`, used as the
    /// key in the generated config's `agent` block. Unlike Claude Code (one
    /// `ANTHROPIC_BASE_URL` for the whole session), OpenCode's config natively
    /// supports any number of named providers, so each sub-agent's model gets
    /// its own `provider.<name>` entry and is referenced directly as
    /// `<provider>/<model>` in `agent.<name>.model` -- no mini-router needed.
    bound_sub_agents: Vec<(String, SubAgentBinding)>,
    /// The session-scoped model proxy, if one was booted for this launch
    /// (see `run_launch`) -- present whenever usage tracking or sub-agent
    /// routing is needed. Used to redirect provider baseURLs so all traffic
    /// flows through the proxy for usage accounting.
    model_proxy: Option<ProxyHandle>,
}

impl ConfigConstructable for OpenCodeLauncher {
    type Config = OpenCodeLauncherConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self {
        let config: OpenCodeLauncherConfig =
            serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self {
            instance_id: instance_id.to_string(),
            config,
            bound_agent_model: None,
            bound_mcp_bindings: vec![],
            bound_sub_agents: vec![],
            model_proxy: global_config.model_proxy.clone(),
        }
    }
}

impl crate::registry::Named for OpenCodeLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for OpenCodeLauncher {
    fn name(&self) -> &str {
        "OpenCode CLI"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("opencode")
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

        if capability_types.contains(&BindingType::SubAgent) {
            // Same dialect choice as the main-model request below: every
            // granite-cli provider can serve `@ai-sdk/openai-compatible`.
            let request = crate::capabilities::BindingRequest::SubAgent(
                crate::capabilities::SubAgentBindingRequest {
                    api_type: ApiType::OpenAI,
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

        // OpenCode's custom-provider config speaks whatever dialect its `npm`
        // SDK package implements. `@ai-sdk/openai-compatible` is the one every
        // granite-cli provider can serve, so that is what we ask for.
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
        resolve_shell_command(&self.config.command_path, "opencode")
    }

    /// Maps a canonical `ToolName` onto the tool-id strings OpenCode's own
    /// (legacy, but still supported) `tools` boolean map uses -- confirmed
    /// against current official docs (<https://opencode.ai/docs/agents/>,
    /// <https://opencode.ai/docs/permissions/>). `edit`/`write` are two
    /// distinct tool ids at this granularity even though OpenCode's newer
    /// `permission` config consolidates both under one `edit` category. MCP
    /// tools are named `<server>_<tool>`, with `<server>_*` disabling/enabling
    /// every tool from that server (confirmed via the docs' own example for
    /// disabling a whole MCP server's tools).
    fn map_tool_name(&self, tool: &ToolName) -> Option<String> {
        Some(match tool {
            ToolName::FileRead => "read".to_string(),
            ToolName::FileWrite => "write".to_string(),
            ToolName::FileEdit => "edit".to_string(),
            ToolName::Search => "grep".to_string(),
            ToolName::FileSearch => "glob".to_string(),
            ToolName::Shell => "bash".to_string(),
            ToolName::WebFetch => "webfetch".to_string(),
            ToolName::WebSearch => "websearch".to_string(),
            ToolName::Mcp { server, tool: None } => format!("{server}_*"),
            ToolName::Mcp {
                server,
                tool: Some(t),
            } => format!("{server}_{t}"),
            ToolName::Other(raw) => raw.clone(),
        })
    }

    /// Points OpenCode at the granite-cli-generated config file and supplies
    /// the credential the file interpolates.
    ///
    /// The `apiKey` is written as an environment reference
    /// (`${GRANITE_CLI_OPENCODE_API_KEY}`) rather than a literal, so the
    /// secret stays out of the generated file and off OpenCode's command
    /// line. Providers with no key omit `apiKey` entirely -- OpenCode's
    /// custom-provider config does not require one.
    async fn env_overlay(&self, ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        let mut overlay = vec![];
        if self.bound_agent_model.is_some()
            || !self.bound_mcp_bindings.is_empty()
            || !self.bound_sub_agents.is_empty()
        {
            overlay.push(EnvBinding {
                key: CONFIG_ENV.to_string(),
                value: opencode_config_path(ctx)?.to_string_lossy().to_string(),
            });

            for (index, (binding, _)) in self.provider_groups().iter().enumerate() {
                if let Some(api_key) = binding
                    .api_key
                    .as_ref()
                    .map(|api_key| api_key.0.clone())
                    .filter(|key| !key.is_empty())
                {
                    overlay.push(EnvBinding {
                        key: provider_api_key_env(index),
                        value: api_key,
                    });
                }
            }
        }
        Ok(overlay)
    }

    /// Writes the granite-cli OpenCode config file, then execs `opencode`
    /// with the caller's arguments untouched.
    ///
    /// Model selection goes through the config's top-level `model` key
    /// rather than a `--model` CLI flag: that flag only exists on some of
    /// OpenCode's subcommands (`run`, `attach`, the default TUI), so
    /// injecting it ahead of an arbitrary subcommand (e.g. `models`,
    /// `agent`) would either be rejected or silently misparsed. The config
    /// key is documented to apply uniformly across all of those surfaces.
    ///
    /// When usage tracking is active (proxy is running), discovers all
    /// providers the user has configured in their global and project
    /// `opencode.json` files and any env-based providers, registers them on
    /// the proxy, and merges them into the generated config with their
    /// `baseURL` redirected to the proxy. This ensures all model calls
    /// (granite-cli managed and user-configured) flow through the proxy for
    /// usage accounting.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        if self.bound_agent_model.is_some()
            || !self.bound_mcp_bindings.is_empty()
            || !self.bound_sub_agents.is_empty()
        {
            // Build granite-cli provider entries
            let mut granite_providers = serde_json::Map::new();
            for (index, (binding, model_names)) in self.provider_groups().iter().enumerate() {
                let entry =
                    self.provider_entry(binding, model_names, &provider_api_key_env(index))?;
                granite_providers.insert(binding.provider_name.clone(), entry);
            }

            // Discover and register user providers for usage tracking
            if let Some(proxy_handle) = &self.model_proxy {
                // Discover user's configured providers from their config files
                if let Some(user_providers) = Self::discover_user_providers(ctx) {
                    // Register on proxy so the proxy can route and track them
                    self.register_user_providers_on_proxy(proxy_handle, &user_providers);
                    granite_providers = self.merge_user_providers_into_config(
                        &user_providers,
                        &Self::discover_env_providers(),
                        &granite_providers,
                    );
                } else {
                    // No user config providers — discover env-based ones
                    let env_providers = Self::discover_env_providers();
                    if !env_providers.is_empty() {
                        self.register_user_providers_on_proxy(proxy_handle, &env_providers);
                        granite_providers = self.merge_user_providers_into_config(
                            &serde_json::Map::new(),
                            &env_providers,
                            &granite_providers,
                        );
                    }
                }
            }

            let agent = self.build_agent_config(ui);
            let config = generate_config(
                self.bound_agent_model.as_ref(),
                granite_providers,
                agent,
                &self.bound_mcp_bindings,
            );
            let config_path = opencode_config_path(ctx)?;

            if ctx.dry_run {
                ui.info(&format!(
                    "Would write OpenCode config to {}:",
                    config_path.display()
                ));
                ui.info(&serde_json::to_string_pretty(&config)?);
            } else {
                write_opencode_config(&config_path, &config)?;
                ui.info(&format!(
                    "Wrote OpenCode config to {}",
                    config_path.display()
                ));
            }
        }

        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;
        alog_channel!(MessageLevel::Debug2, "Env Overlay: {:#?}", overlay);

        run_command(binary, &overlay, args, ctx, ui).await
    }
}

impl HasOpenCodeLauncherMetadata for OpenCodeLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "OpenCode CLI".to_string(),
            description: "OpenCode terminal coding agent".to_string(),
            default_command: "opencode".to_string(),
            supported_capabilities: HashSet::from([
                BindingType::AgentModel,
                BindingType::Mcp,
                BindingType::SubAgent,
            ]),
            tags: vec!["opencode".to_string(), "coding-agent".to_string()],
        }
    }
}

/*-- private --*/

// HasOpenCodeLauncherMetadata is the macro-generated trait; re-exported via mod.rs.
use crate::launchers::base::HasLauncherMetadata as HasOpenCodeLauncherMetadata;

/// Env var OpenCode merges an extra config file from, in addition to its own
/// global/project config.
const CONFIG_ENV: &str = "OPENCODE_CONFIG";

/// Env var the generated provider entry interpolates its `apiKey` from.
const API_KEY_ENV: &str = "GRANITE_CLI_OPENCODE_API_KEY";

/// The generated config file's name, relative to the launcher state dir.
const CONFIG_FILE: &str = "opencode.json";

impl OpenCodeLauncher {
    /// Builds the `provider.<name>` entry describing `binding`'s provider,
    /// with one `models` entry per name in `model_names` -- plural because a
    /// single provider instance may back both the main model and one or more
    /// sub-agents' models, all of which must land in the same generated
    /// `provider.<name>` entry rather than clobbering each other.
    ///
    /// When a session proxy is active (usage tracking or sub-agent routing
    /// enabled), the provider's `baseURL` is overridden to point at the proxy
    /// so all traffic flows through it for usage accounting. The proxy
    /// dispatches based on the `"model"` field in each request body, which
    /// means the provider name is irrelevant for routing -- only the model
    /// name matters.
    fn provider_entry(
        &self,
        binding: &AgentModelBinding,
        model_names: &[&str],
        api_key_env: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let base_url = self.proxy_base_url(binding);
        let mut options = serde_json::json!({ "baseURL": base_url });
        if binding
            .api_key
            .as_ref()
            .is_some_and(|key| !key.0.is_empty())
        {
            options["apiKey"] = serde_json::Value::String(format!("{{env:{api_key_env}}}"));
        }

        // `limit` is all-or-nothing in OpenCode's schema: if present, both
        // `context` and `output` are required. granite-cli only tracks a
        // context length, so `limit` is left out entirely rather than
        // guessing an output cap.
        let mut models = serde_json::Map::new();
        for name in model_names {
            models.insert(
                (*name).to_string(),
                serde_json::json!({
                    "name": name,
                }),
            );
        }

        let mut entry = serde_json::json!({
            "npm": "@ai-sdk/openai-compatible",
            "name": binding.provider_name,
            "options": options,
            "models": serde_json::Value::Object(models),
        });

        // Shallow merge so a user override of e.g. `headers` doesn't clobber
        // the generated `options`/`models`, and vice versa. Applied uniformly
        // to every generated provider entry (main model's and every
        // sub-agent's) -- there is deliberately no per-sub-agent override
        // knob yet.
        if let (Some(overrides), Some(target)) = (
            self.config
                .provider_overrides
                .as_ref()
                .and_then(serde_json::Value::as_object),
            entry.as_object_mut(),
        ) {
            for (key, value) in overrides {
                target.insert(key.clone(), value.clone());
            }
        }
        Ok(entry)
    }

    /// Groups the main model binding (if any) and every bound sub-agent's
    /// model binding by `provider_name`, collecting each group's distinct
    /// model names -- so two sub-agents (or a sub-agent and the main model)
    /// that happen to share the same underlying granite-cli provider instance
    /// land in one `provider.<name>` entry with multiple `models`, instead of
    /// one overwriting the other. Order is main-model-first, then
    /// `bound_sub_agents` order, which is also the order `env_overlay` and
    /// `launch` use to number each group's API-key env var
    /// (`provider_api_key_env`) -- the two must stay in lock-step.
    fn provider_groups(&self) -> Vec<(&AgentModelBinding, Vec<&str>)> {
        fn add<'a>(
            groups: &mut Vec<(&'a AgentModelBinding, Vec<&'a str>)>,
            binding: &'a AgentModelBinding,
        ) {
            if let Some((_, model_names)) = groups
                .iter_mut()
                .find(|(b, _)| b.provider_name == binding.provider_name)
            {
                if !model_names.contains(&binding.model_name.as_str()) {
                    model_names.push(&binding.model_name);
                }
            } else {
                groups.push((binding, vec![binding.model_name.as_str()]));
            }
        }

        let mut groups = Vec::new();
        if let Some(binding) = &self.bound_agent_model {
            add(&mut groups, binding);
        }
        for (_, sub_agent) in &self.bound_sub_agents {
            add(&mut groups, &sub_agent.model);
        }
        groups
    }

    /// Builds the `agent.<name>` entries for every bound sub-agent:
    /// `description`, `prompt`, `model` (as `<provider>/<model>`, per
    /// <https://opencode.ai/docs/agents/>), and `tools` when a tool
    /// allow-list was given. `tools` is OpenCode's legacy-but-still-supported
    /// boolean map (<https://opencode.ai/docs/agents/>,
    /// <https://opencode.ai/docs/permissions/>) rather than the newer
    /// `permission` config: `permission`'s named categories default to
    /// "allow" for anything not mentioned, which can't express "only these
    /// tools, everything else off" the way `{"*": false, ...}` can -- the
    /// same allow-list semantics `SubAgentBinding.tools` already has for the
    /// `claude` launcher. A tool with no mapping is dropped with a warning
    /// (per sub-agent), matching `ClaudeLauncher::build_agents_json`.
    ///
    /// `known_type` maps onto OpenCode's own built-in agent names (`explore`,
    /// `plan` -- see <https://opencode.ai/docs/agents/>) the same way
    /// `ClaudeLauncher` overrides Claude Code's built-in `Explore`/`Plan`
    /// sub-agents.
    fn build_agent_config(&self, ui: &dyn Ui) -> serde_json::Map<String, serde_json::Value> {
        self.bound_sub_agents
            .iter()
            .map(|(name, binding)| {
                let mut entry = serde_json::json!({
                    "description": binding.description,
                    "prompt": binding.prompt,
                    "mode": "subagent",
                    "model": format!("{}/{}", binding.model.provider_name, binding.model.model_name),
                });
                if !binding.tools.is_empty() {
                    let mut tools = serde_json::Map::new();
                    tools.insert("*".to_string(), serde_json::Value::Bool(false));
                    for tool in &binding.tools {
                        match self.map_tool_name(tool) {
                            Some(mapped) => {
                                tools.insert(mapped, serde_json::Value::Bool(true));
                            }
                            None => ui.warn(&format!(
                                "sub-agent '{name}': tool {tool:?} has no mapping for the opencode launcher, skipping"
                            )),
                        }
                    }
                    entry["tools"] = serde_json::Value::Object(tools);
                }
                let mapped_name = match binding.known_type {
                    Some(KnownSubAgent::Explore) => "explore".to_string(),
                    Some(KnownSubAgent::Plan) => "plan".to_string(),
                    None => name.clone(),
                };
                (mapped_name, entry)
            })
            .collect()
    }

    /// Returns the baseURL to use for OpenCode provider entries. When a
    /// session proxy is active (usage tracking enabled), returns the proxy's
    /// local URL so all traffic flows through it for accounting -- the proxy
    /// dispatches by the `"model"` field in each request body. Otherwise
    /// delegates to `opencode_base_url` to compute the provider's real URL.
    fn proxy_base_url(&self, binding: &AgentModelBinding) -> String {
        match &self.model_proxy {
            Some(handle) => handle.local_base_url.clone(),
            None => opencode_base_url(binding),
        }
    }

    /// Resolves `{env:VAR_NAME}` placeholders in a JSON value with the current
    /// process environment. Leaves unknown variables as empty strings (matching
    /// OpenCode's own behavior). Recurses into objects and arrays.
    fn resolve_env_vars(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => {
                if s.starts_with("{env:") && s.ends_with('}') {
                    let var_name = &s[5..s.len() - 1];
                    serde_json::Value::String(std::env::var(var_name).unwrap_or_default())
                } else {
                    value.clone()
                }
            }
            serde_json::Value::Object(map) => {
                let resolved: serde_json::Map<_, _> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::resolve_env_vars(v)))
                    .collect();
                serde_json::Value::Object(resolved)
            }
            serde_json::Value::Array(arr) => {
                let resolved: Vec<_> = arr.iter().map(Self::resolve_env_vars).collect();
                serde_json::Value::Array(resolved)
            }
            other => other.clone(),
        }
    }

    /// Loads and parses a user's OpenCode config file (JSON or JSONC).
    /// Strips `//` and `/* ... */` comments before parsing. Returns `None` if
    /// the file doesn't exist or can't be parsed.
    fn load_user_config(path: &Path) -> Option<serde_json::Value> {
        let content = std::fs::read_to_string(path).ok()?;
        let content = Self::strip_jsonc_comments(&content);
        serde_json::from_str(&content).ok()
    }

    /// Strip JSONC comments from `content`, respecting string literals (doesn't
    /// strip `//` or `/*` inside quoted strings).
    fn strip_jsonc_comments(content: &str) -> String {
        let mut result = String::with_capacity(content.len());
        let mut chars = content.chars().peekable();
        let mut in_string = false;
        let mut escape = false;

        while let Some(c) = chars.next() {
            if escape {
                result.push(c);
                escape = false;
                continue;
            }
            if c == '\\' && in_string {
                result.push(c);
                escape = true;
                continue;
            }
            if c == '"' {
                in_string = !in_string;
                result.push(c);
                continue;
            }
            if in_string {
                result.push(c);
                continue;
            }
            if c == '/' {
                if let Some(&next) = chars.peek() {
                    if next == '/' {
                        // Line comment: skip to end of line
                        while let Some(&ch) = chars.peek() {
                            if ch == '\n' {
                                break;
                            }
                            chars.next();
                        }
                        continue;
                    } else if next == '*' {
                        // Block comment: skip to */
                        chars.next(); // consume '*'
                        while let Some(ch) = chars.next() {
                            if ch == '*' {
                                if chars.peek() == Some(&'/') {
                                    chars.next();
                                    break;
                                }
                            }
                        }
                        continue;
                    }
                }
            }
            result.push(c);
        }
        result
    }

    /// Extracts the `provider` object from a parsed OpenCode config. Returns
    /// `None` if the config has no provider key or it's not an object.
    fn extract_providers(config: &serde_json::Value) -> Option<&serde_json::Map<String, serde_json::Value>> {
        config.get("provider").and_then(|v| v.as_object())
    }

    /// Discovers all providers the user has configured in their global and
    /// project OpenCode configs. Reads `~/.config/opencode/opencode.json`
    /// (global) and `{working_dir}/opencode.json` (project), extracts
    /// `provider` entries, and returns the merged provider map (project
    /// overrides global for conflicting keys).
    fn discover_user_providers(ctx: &LaunchContext) -> Option<serde_json::Map<String, serde_json::Value>> {
        let mut providers = serde_json::Map::new();

        // Load global config (~/.config/opencode/opencode.json)
        if let Some(global_config) = Self::load_global_opencode_config() {
            let resolved = Self::resolve_env_vars(&global_config);
            if let Some(global_providers) = Self::extract_providers(&resolved) {
                for (key, value) in global_providers {
                    providers.insert(key.clone(), value.clone());
                }
            }
        }

        // Load project config (overwrites global for conflicting keys)
        let project_path = ctx.working_dir.join("opencode.json");
        if let Some(project_config) = Self::load_user_config(&project_path) {
            let resolved = Self::resolve_env_vars(&project_config);
            if let Some(project_providers) = Self::extract_providers(&resolved) {
                for (key, value) in project_providers {
                    providers.insert(key.clone(), value.clone());
                }
            }
        }

        if providers.is_empty() {
            None
        } else {
            Some(providers)
        }
    }

    /// Returns the path to OpenCode's global config file.
    fn global_opencode_config_path() -> Option<PathBuf> {
        let home = std::env::var("XDG_CONFIG_HOME").ok().or_else(|| {
            std::env::var("HOME").ok()
        });
        let path = match home.as_deref() {
            Some(xdg) if !xdg.is_empty() => format!("{xdg}/opencode/opencode.json"),
            _ => format!("/.config/opencode/opencode.json"),
        };
        let path = format!("%s{}", path.trim_start_matches('/'));
        let home_path = home.map(|h| format!("{h}{path}"));
        home_path.map(PathBuf::from)
    }

    /// Loads OpenCode's global config file and returns parsed JSON, or None.
    fn load_global_opencode_config() -> Option<serde_json::Value> {
        let path = Self::global_opencode_config_path()?;
        Self::load_user_config(&path)
    }

    /// Registers discovered user providers on the session proxy so the proxy
    /// can route and track traffic for them. For each provider with a
    /// `baseURL`, registers the provider with its known model names and a
    /// prefix-based fallback route. The prefix is derived from the base URL
    /// (e.g. "api.openai.com" -> "gpt" prefix, "api.anthropic.com" -> "claude"
    /// prefix) so that unknown model names from this provider still route
    /// correctly. Also sets the proxy's default target to the first discovered
    /// provider if no other default is set.
    fn register_user_providers_on_proxy(
        &self,
        proxy_handle: &ProxyHandle,
        user_providers: &serde_json::Map<String, serde_json::Value>,
    ) {
        for (provider_name, provider_entry) in user_providers {
            let Some(options) = provider_entry
                .get("options")
                .and_then(|o| o.as_object())
            else {
                continue;
            };

            let Some(base_url) = options.get("baseURL")
                .and_then(|b| b.as_str())
            else {
                continue;
            };

            // Skip if baseURL already points at the proxy
            if base_url.contains("127.0.0.1") || base_url.contains("localhost") {
                continue;
            }

            // Compute a model-name prefix from the base URL for fallback routing
            let prefix = model_prefix_from_url(base_url);

            // Extract model names from the provider's models map
            let model_names: Vec<String> = provider_entry
                .get("models")
                .and_then(|m| m.as_object())
                .map(|models| {
                    models.keys().cloned().collect()
                })
                .unwrap_or_default();

            let label = format!("user-provider-{}", provider_name);
            if let Err(e) = proxy_handle.register_provider(
                &prefix,
                base_url.to_string(),
                model_names,
                label,
            ) {
                alog_channel!(
                    MessageLevel::Debug3,
                    "failed to register provider '{}' on proxy: {}",
                    provider_name,
                    e
                );
            }
        }
    }

    /// Discovers env-based providers by checking for known API key environment
    /// variables. Returns a map of provider name -> provider entry pointing at
    /// the well-known base URL for each provider. These are providers that
    /// OpenCode auto-loads when the corresponding env var is set, but don't
    /// appear explicitly in the user's config files.
    fn discover_env_providers() -> serde_json::Map<String, serde_json::Value> {
        let mut providers = serde_json::Map::new();

        // OpenAI
        if std::env::var("OPENAI_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@openai/openai".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("openai".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://api.openai.com/v1".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:OPENAI_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("openai".to_string(), serde_json::Value::Object(entry));
        }

        // Anthropic
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@anthropic-ai/anthropic".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("anthropic".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://api.anthropic.com".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:ANTHROPIC_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("anthropic".to_string(), serde_json::Value::Object(entry));
        }

        // Google Gemini
        if std::env::var("GOOGLE_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@google/generative-ai".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("google".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://generativelanguage.googleapis.com/v1beta".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:GOOGLE_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("google".to_string(), serde_json::Value::Object(entry));
        }

        // Groq
        if std::env::var("GROQ_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@ai-sdk/openai-compatible".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("groq".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://api.groq.com/openai/v1".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:GROQ_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("groq".to_string(), serde_json::Value::Object(entry));
        }

        // OpenRouter
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@ai-sdk/openai-compatible".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("openrouter".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://openrouter.ai/api/v1".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:OPENROUTER_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("openrouter".to_string(), serde_json::Value::Object(entry));
        }

        // Cohere
        if std::env::var("CO_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@ai-sdk/openai-compatible".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("cohere".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://api.cohere.com/v1".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:CO_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("cohere".to_string(), serde_json::Value::Object(entry));
        }

        // Mistral
        if std::env::var("MISTRAL_API_KEY").is_ok() {
            let mut entry = serde_json::Map::new();
            entry.insert(
                "npm".to_string(),
                serde_json::Value::String("@ai-sdk/openai-compatible".to_string()),
            );
            entry.insert(
                "name".to_string(),
                serde_json::Value::String("mistral".to_string()),
            );
            let mut options = serde_json::Map::new();
            options.insert(
                "baseURL".to_string(),
                serde_json::Value::String("https://api.mistral.ai/v1".to_string()),
            );
            options.insert(
                "apiKey".to_string(),
                serde_json::Value::String("{env:MISTRAL_API_KEY}".to_string()),
            );
            entry.insert("options".to_string(), serde_json::Value::Object(options));
            providers.insert("mistral".to_string(), serde_json::Value::Object(entry));
        }

        providers
    }

    /// Merges user-discovered and env-based providers into the generated config.
    /// For each provider, creates a provider entry with `baseURL` pointing at
    /// the proxy so all traffic flows through it. The proxy knows the real
    /// upstream URL from `register_user_providers_on_proxy` and forwards
    /// requests with usage tracking.
    ///
    /// Granite-cli provider entries (built from bindings) take precedence: if a
    /// user also has a provider with the same name, our granite-cli entry
    /// wins (since it's added to the map after the user entries).
    fn merge_user_providers_into_config(
        &self,
        user_providers: &serde_json::Map<String, serde_json::Value>,
        env_providers: &serde_json::Map<String, serde_json::Value>,
        granite_providers: &serde_json::Map<String, serde_json::Value>,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut merged = serde_json::Map::new();

        // Add user-discovered providers (redirected to proxy)
        for (name, entry) in user_providers {
            if let Some(entry_value) = entry.as_object() {
                let mut proxied = entry_value.clone();
                if let Some(options) = proxied.get_mut("options") {
                    if let Some(options_obj) = options.as_object_mut() {
                        if let Some(real_url) = options_obj.get("baseURL").and_then(|b| b.as_str()) {
                            // Only redirect if it's a real external URL (not already proxy)
                            if !real_url.contains("127.0.0.1") && !real_url.contains("localhost") {
                                options_obj.insert(
                                    "baseURL".to_string(),
                                    serde_json::Value::String(
                                        self.model_proxy.as_ref()
                                            .map(|h| h.local_base_url.clone())
                                            .unwrap_or_else(|| real_url.to_string()),
                                    ),
                                );
                            }
                        }
                    }
                }
                merged.insert(name.clone(), serde_json::Value::Object(proxied));
            }
        }

        // Add env-based providers (redirected to proxy)
        for (name, entry) in env_providers {
            if let Some(entry_value) = entry.as_object() {
                let mut proxied = entry_value.clone();
                if let Some(options) = proxied.get_mut("options") {
                    if let Some(options_obj) = options.as_object_mut() {
                        let proxy_url = self.model_proxy
                            .as_ref()
                            .map(|h| h.local_base_url.clone())
                            .unwrap_or_else(|| {
                                options_obj.get("baseURL")
                                    .and_then(|b| b.as_str())
                                    .unwrap_or("unknown")
                                    .to_string()
                            });
                        options_obj.insert(
                            "baseURL".to_string(),
                            serde_json::Value::String(proxy_url),
                        );
                    }
                }
                merged.insert(name.clone(), serde_json::Value::Object(proxied));
            }
        }

        // Add granite-cli providers (overrides user providers with same name)
        for (name, entry) in granite_providers {
            merged.insert(name.clone(), entry.clone());
        }

        merged
    }
}

/// Derives a model-name prefix from a provider's base URL for use in proxy
/// prefix-based fallback routing. The prefix is the shortest recognizable
/// substring that uniquely identifies the provider's models, e.g. "gpt" for
/// OpenAI, "claude" for Anthropic. This allows the proxy to route unknown
/// model names (e.g. "gpt-4o-turbo") to the correct provider even when the
/// exact model name wasn't registered in the user's config.
fn model_prefix_from_url(base_url: &str) -> String {
    let url = base_url.to_lowercase();
    if url.contains("openai") || url.contains("api.openai.com") {
        "gpt".to_string()
    } else if url.contains("anthropic") || url.contains("api.anthropic.com") {
        "claude".to_string()
    } else if url.contains("google") || url.contains("generativelanguage") {
        "gemini".to_string()
    } else if url.contains("groq") {
        "llama".to_string() // Groq commonly hosts Llama models
    } else if url.contains("openrouter") {
        "gpt".to_string() // OpenRouter hosts many models; default to gpt prefix
    } else if url.contains("cohere") {
        "command".to_string() // Cohere's models start with "command"
    } else if url.contains("mistral") {
        "mistral".to_string()
    } else {
        // Default: use a common model prefix or the hostname itself
        url.split('/').find(|s| !s.is_empty() && !s.contains('.') && s.len() <= 20)
            .unwrap_or("model")
            .to_string()
    }
}

/// The env var an OpenCode provider entry's `apiKey` interpolates from, for
/// the `index`-th distinct provider in `provider_groups()` order. Index `0`
/// (conventionally the main model's provider, when bound) keeps the original
/// unsuffixed name for backwards compatibility; every additional distinct
/// provider (a sub-agent's, when it differs from the main model's) gets its
/// own suffixed var so multiple secrets can be injected into one launch
/// without colliding.
fn provider_api_key_env(index: usize) -> String {
    if index == 0 {
        API_KEY_ENV.to_string()
    } else {
        format!("{API_KEY_ENV}_{index}")
    }
}

/// OpenCode's `baseURL` is the API root the SDK appends operation paths to
/// (e.g. `/chat/completions`), so drop that trailing operation from the
/// binding's full endpoint path and keep the version prefix.
fn opencode_base_url(binding: &AgentModelBinding) -> String {
    let root = binding.base_url.trim_end_matches('/');
    let prefix = binding
        .endpoint_path
        .strip_suffix("/chat/completions")
        .unwrap_or("");
    format!("{root}{prefix}")
}

/// The granite-cli-owned config file this launcher instance writes and points
/// `OPENCODE_CONFIG` at. Lives under the launcher state dir rather than the
/// user's own OpenCode config directory -- it is never read by anything else.
fn opencode_config_path(ctx: &LaunchContext) -> anyhow::Result<PathBuf> {
    Ok(crate::config::Config::launcher_state_dir(&ctx.launcher_id)?.join(CONFIG_FILE))
}

/// Builds the top-level `opencode.json` shape: the main model (if bound)
/// selected via the top-level `model` key (`provider/model`) so it applies
/// uniformly across the TUI, `run`, `attach`, and GitHub Action; a `provider`
/// block with one entry per distinct provider (main model's and/or each
/// sub-agent's, pre-built by the caller via `provider_groups`/
/// `provider_entry`); an `agent` block (if any sub-agents are bound, pre-built
/// via `build_agent_config`); and an `mcp` block (if any MCP servers are
/// bound), using opencode's `McpLocalConfig`/`McpRemoteConfig` shape (see
/// <https://opencode.ai/config.json>).
fn generate_config(
    binding: Option<&AgentModelBinding>,
    providers: serde_json::Map<String, serde_json::Value>,
    agent: serde_json::Map<String, serde_json::Value>,
    mcp_bindings: &[(String, McpBinding)],
) -> serde_json::Value {
    let mut config = serde_json::json!({ "$schema": "https://opencode.ai/config.json" });
    if let Some(binding) = binding {
        config["model"] =
            serde_json::Value::String(format!("{}/{}", binding.provider_name, binding.model_name));
    }
    if !providers.is_empty() {
        config["provider"] = serde_json::Value::Object(providers);
    }
    if !agent.is_empty() {
        config["agent"] = serde_json::Value::Object(agent);
    }
    if !mcp_bindings.is_empty() {
        let mut mcp = serde_json::Map::new();
        for (name, binding) in mcp_bindings {
            mcp.insert(name.clone(), {
                match binding {
                    McpBinding::Stdio { command, args, env } => {
                        let mut full_command = vec![command.clone()];
                        full_command.extend(args.iter().cloned());
                        serde_json::json!({
                            "type": "local",
                            "command": full_command,
                            "environment": env,
                        })
                    }
                    McpBinding::Http { url, headers } | McpBinding::Sse { url, headers } => {
                        serde_json::json!({
                            "type": "remote",
                            "url": url,
                            "headers": headers,
                        })
                    }
                }
            });
        }
        config["mcp"] = serde_json::Value::Object(mcp);
    }
    config
}

fn write_opencode_config(path: &Path, config: &serde_json::Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    let mut content = serde_json::to_string_pretty(config)?;
    content.push('\n');
    std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Named, Secret};
    use crate::utils::ui::base::tests::CaptureUi;

    fn launcher(cfg: serde_json::Value) -> OpenCodeLauncher {
        OpenCodeLauncher::new("opencode", &cfg, &crate::config::Config::default())
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
        }
    }

    fn bound(cfg: serde_json::Value, binding: AgentModelBinding) -> OpenCodeLauncher {
        let mut l = launcher(cfg);
        l.bound_agent_model = Some(binding);
        l
    }

    fn ctx(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "opencode".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: std::collections::HashMap::new(),
            dry_run,
        }
    }

    // -- command resolution ----------------------------------------------------

    #[test]
    fn command_defaults_to_opencode() {
        assert_eq!(launcher(serde_json::json!({})).command(), "opencode");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = launcher(serde_json::json!({ "command_path": "/opt/bin/opencode" }));
        assert_eq!(l.command(), "/opt/bin/opencode");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = launcher(serde_json::json!({ "command_path": "/no/such/path/opencode" }));
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = launcher(serde_json::json!({ "command_path": "ls" }));
        assert!(l.validate_command().is_ok());
    }

    // -- metadata / schema -----------------------------------------------------

    #[test]
    fn metadata_name_is_opencode_cli() {
        let meta = OpenCodeLauncher::metadata();
        assert_eq!(meta.name, "OpenCode CLI");
        assert_eq!(meta.default_command, "opencode");
        assert!(
            meta.supported_capabilities
                .contains(&BindingType::AgentModel)
        );
    }

    #[test]
    fn metadata_supports_sub_agent_binding() {
        let meta = OpenCodeLauncher::metadata();
        assert!(meta.supported_capabilities.contains(&BindingType::SubAgent));
    }

    #[test]
    fn instance_id_round_trips_from_construction() {
        let l = OpenCodeLauncher::new(
            "opencode-local",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert_eq!(l.instance_id(), "opencode-local");
    }

    #[test]
    fn config_schema_exposes_only_command_path_and_overrides() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<OpenCodeLauncher>("opencode");
        let schema = factory.config_schema("opencode").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("provider_overrides"));
        // The OpenCode provider name comes from the binding, never from
        // launcher config.
        assert!(!props.contains_key("provider_name"));
    }

    // -- provider entry --------------------------------------------------------

    #[test]
    fn provider_entry_describes_bound_model() {
        let b = binding();
        let entry = launcher(serde_json::json!({}))
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert_eq!(entry["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(entry["options"]["baseURL"], "http://localhost:11434/v1");
        assert_eq!(entry["models"]["granite4.1:8b"]["name"], "granite4.1:8b");
        // No output-token data means no `limit` at all: OpenCode requires
        // both `context` and `output` together when `limit` is present.
        assert!(entry["models"]["granite4.1:8b"].get("limit").is_none());
        // No key means no apiKey field at all.
        assert!(entry["options"].get("apiKey").is_none());
    }

    #[test]
    fn provider_entry_describes_multiple_models_for_one_provider() {
        let b = binding();
        let entry = launcher(serde_json::json!({}))
            .provider_entry(&b, &["granite4.1:8b", "granite4.1:3b"], API_KEY_ENV)
            .unwrap();
        assert_eq!(entry["models"]["granite4.1:8b"]["name"], "granite4.1:8b");
        assert_eq!(entry["models"]["granite4.1:3b"]["name"], "granite4.1:3b");
    }

    #[test]
    fn provider_entry_interpolates_env_when_key_present() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let entry = launcher(serde_json::json!({}))
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert_eq!(
            entry["options"]["apiKey"],
            "{env:GRANITE_CLI_OPENCODE_API_KEY}"
        );
    }

    #[test]
    fn provider_entry_uses_the_given_api_key_env_name() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let entry = launcher(serde_json::json!({}))
            .provider_entry(
                &b,
                &[b.model_name.as_str()],
                "GRANITE_CLI_OPENCODE_API_KEY_1",
            )
            .unwrap();
        assert_eq!(
            entry["options"]["apiKey"],
            "{env:GRANITE_CLI_OPENCODE_API_KEY_1}"
        );
    }

    #[test]
    fn provider_entry_omits_api_key_for_empty_secret() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("")),
            ..binding()
        };
        let entry = launcher(serde_json::json!({}))
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert!(entry["options"].get("apiKey").is_none());
    }

    #[test]
    fn provider_entry_merges_overrides() {
        let l = launcher(serde_json::json!({
            "provider_overrides": { "headers": { "X-Custom": "1" } }
        }));
        let b = binding();
        let entry = l
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert_eq!(entry["headers"]["X-Custom"], "1");
        // Generated keys survive the merge.
        assert_eq!(entry["options"]["baseURL"], "http://localhost:11434/v1");
    }

    #[test]
    fn provider_entry_overrides_win_on_conflict() {
        let l = launcher(serde_json::json!({
            "provider_overrides": { "npm": "@ai-sdk/openai" }
        }));
        let b = binding();
        let entry = l
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert_eq!(entry["npm"], "@ai-sdk/openai");
    }

    // -- provider_api_key_env ---------------------------------------------------

    #[test]
    fn provider_api_key_env_keeps_unsuffixed_name_at_index_zero() {
        assert_eq!(provider_api_key_env(0), "GRANITE_CLI_OPENCODE_API_KEY");
    }

    #[test]
    fn provider_api_key_env_suffixes_by_index_beyond_zero() {
        assert_eq!(provider_api_key_env(1), "GRANITE_CLI_OPENCODE_API_KEY_1");
        assert_eq!(provider_api_key_env(2), "GRANITE_CLI_OPENCODE_API_KEY_2");
    }

    // -- provider_groups ---------------------------------------------------------

    fn sub_agent_binding(
        description: &str,
        provider_name: &str,
        model_name: &str,
        tools: Vec<ToolName>,
    ) -> SubAgentBinding {
        SubAgentBinding {
            description: description.to_string(),
            prompt: "You are a helpful sub-agent.".to_string(),
            tools,
            model: AgentModelBinding {
                provider_name: provider_name.to_string(),
                model_name: model_name.to_string(),
                ..binding()
            },
            known_type: None,
        }
    }

    #[test]
    fn provider_groups_is_empty_with_nothing_bound() {
        let l = launcher(serde_json::json!({}));
        assert!(l.provider_groups().is_empty());
    }

    #[test]
    fn provider_groups_includes_main_model_first() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_agent_model = Some(binding());
        let groups = l.provider_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0.provider_name, "my-ollama");
        assert_eq!(groups[0].1, vec!["granite4.1:8b"]);
    }

    #[test]
    fn provider_groups_gives_a_distinct_provider_its_own_group() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_agent_model = Some(binding());
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding("Reviews code", "other-provider", "other-model", vec![]),
        )];
        let groups = l.provider_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].0.provider_name, "other-provider");
        assert_eq!(groups[1].1, vec!["other-model"]);
    }

    #[test]
    fn provider_groups_merges_model_names_sharing_a_provider() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_agent_model = Some(binding());
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding("Reviews code", "my-ollama", "granite4.1:3b", vec![]),
        )];
        let groups = l.provider_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, vec!["granite4.1:8b", "granite4.1:3b"]);
    }

    #[test]
    fn provider_groups_dedupes_identical_model_name_for_shared_provider() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_agent_model = Some(binding());
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding("Reviews code", "my-ollama", "granite4.1:8b", vec![]),
        )];
        let groups = l.provider_groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1, vec!["granite4.1:8b"]);
    }

    // -- map_tool_name -----------------------------------------------------------

    #[test]
    fn map_tool_name_covers_every_canonical_variant_and_formats_mcp_references() {
        let l = launcher(serde_json::json!({}));
        assert_eq!(
            l.map_tool_name(&ToolName::FileRead),
            Some("read".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileWrite),
            Some("write".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileEdit),
            Some("edit".to_string())
        );
        assert_eq!(l.map_tool_name(&ToolName::Search), Some("grep".to_string()));
        assert_eq!(
            l.map_tool_name(&ToolName::FileSearch),
            Some("glob".to_string())
        );
        assert_eq!(l.map_tool_name(&ToolName::Shell), Some("bash".to_string()));
        assert_eq!(
            l.map_tool_name(&ToolName::WebFetch),
            Some("webfetch".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::WebSearch),
            Some("websearch".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: None,
            }),
            Some("vision_*".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: Some("vlm_compare_images".to_string()),
            }),
            Some("vision_vlm_compare_images".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Other("SomeRawTool".to_string())),
            Some("SomeRawTool".to_string())
        );
    }

    // -- build_agent_config -------------------------------------------------------

    #[test]
    fn build_agent_config_includes_description_prompt_and_model_but_omits_empty_tools() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding("Reviews code", "my-ollama", "granite4.1:8b", vec![]),
        )];
        let ui = CaptureUi::default();
        let agent = l.build_agent_config(&ui);
        let entry = &agent["reviewer"];
        assert_eq!(entry["description"], "Reviews code");
        assert_eq!(entry["prompt"], "You are a helpful sub-agent.");
        assert_eq!(entry["model"], "my-ollama/granite4.1:8b");
        assert!(entry.get("tools").is_none());
    }

    #[test]
    fn build_agent_config_denies_by_default_and_allows_only_listed_tools() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding(
                "Reviews code",
                "my-ollama",
                "granite4.1:8b",
                vec![ToolName::FileRead, ToolName::Search],
            ),
        )];
        let ui = CaptureUi::default();
        let agent = l.build_agent_config(&ui);
        assert_eq!(
            agent["reviewer"]["tools"],
            serde_json::json!({ "*": false, "read": true, "grep": true })
        );
    }

    #[test]
    fn build_agent_config_covers_every_bound_sub_agent_by_instance_id() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_sub_agents = vec![
            (
                "reviewer".to_string(),
                sub_agent_binding("Reviews code", "my-ollama", "model-a", vec![]),
            ),
            (
                "summarizer".to_string(),
                sub_agent_binding("Summarizes text", "my-ollama", "model-b", vec![]),
            ),
        ];
        let ui = CaptureUi::default();
        let agent = l.build_agent_config(&ui);
        assert_eq!(agent.len(), 2);
        assert_eq!(agent["reviewer"]["model"], "my-ollama/model-a");
        assert_eq!(agent["summarizer"]["model"], "my-ollama/model-b");
    }

    #[test]
    fn build_agent_config_maps_known_types_onto_opencodes_own_builtin_agent_names() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_sub_agents = vec![(
            "my-explorer".to_string(),
            SubAgentBinding {
                known_type: Some(KnownSubAgent::Explore),
                ..sub_agent_binding("Explores code", "my-ollama", "granite4.1:8b", vec![])
            },
        )];
        let ui = CaptureUi::default();
        let agent = l.build_agent_config(&ui);
        assert!(agent.contains_key("explore"));
        assert!(!agent.contains_key("my-explorer"));
    }

    // -- base url ----------------------------------------------------------

    #[test]
    fn base_url_keeps_version_prefix_and_drops_operation() {
        assert_eq!(opencode_base_url(&binding()), "http://localhost:11434/v1");
    }

    #[test]
    fn base_url_trims_trailing_slash_from_provider_url() {
        let b = AgentModelBinding {
            base_url: "http://localhost:1234/".to_string(),
            ..binding()
        };
        assert_eq!(opencode_base_url(&b), "http://localhost:1234/v1");
    }

    // -- generate_config -----------------------------------------------------

    #[test]
    fn generate_config_nests_entry_under_provider_name_and_sets_default_model() {
        let mut providers = serde_json::Map::new();
        providers.insert(
            "my-ollama".to_string(),
            serde_json::json!({ "npm": "@ai-sdk/openai-compatible" }),
        );
        let config = generate_config(Some(&binding()), providers, serde_json::Map::new(), &[]);
        assert_eq!(config["$schema"], "https://opencode.ai/config.json");
        assert_eq!(config["model"], "my-ollama/granite4.1:8b");
        assert_eq!(
            config["provider"]["my-ollama"]["npm"],
            "@ai-sdk/openai-compatible"
        );
    }

    #[test]
    fn generate_config_writes_mcp_block_without_a_model_binding() {
        let mcp_binding = McpBinding::Http {
            url: "http://127.0.0.1:9999".to_string(),
            headers: Default::default(),
        };
        let config = generate_config(
            None,
            serde_json::Map::new(),
            serde_json::Map::new(),
            &[("vision".to_string(), mcp_binding)],
        );
        assert!(config.get("model").is_none());
        assert!(config.get("provider").is_none());
        assert_eq!(config["mcp"]["vision"]["type"], "remote");
        assert_eq!(config["mcp"]["vision"]["url"], "http://127.0.0.1:9999");
    }

    #[test]
    fn generate_config_writes_agent_block_when_sub_agents_present() {
        let mut agent = serde_json::Map::new();
        agent.insert(
            "reviewer".to_string(),
            serde_json::json!({ "description": "Reviews code" }),
        );
        let config = generate_config(None, serde_json::Map::new(), agent, &[]);
        assert!(config.get("model").is_none());
        assert_eq!(config["agent"]["reviewer"]["description"], "Reviews code");
    }

    #[test]
    fn generate_config_omits_agent_key_when_no_sub_agents_bound() {
        let config = generate_config(
            Some(&binding()),
            serde_json::Map::new(),
            serde_json::Map::new(),
            &[],
        );
        assert!(config.get("agent").is_none());
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
    async fn env_overlay_redirects_config_and_exports_api_key() {
        let b = AgentModelBinding {
            api_key: Some(Secret::from("sk-test")),
            ..binding()
        };
        let overlay = bound(serde_json::json!({}), b)
            .env_overlay(&ctx(false))
            .await
            .unwrap();

        let config = overlay
            .iter()
            .find(|b| b.key == "OPENCODE_CONFIG")
            .expect("config redirect");
        assert!(
            config
                .value
                .ends_with("launcher-state/opencode/opencode.json"),
            "{}",
            config.value
        );

        let key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_OPENCODE_API_KEY")
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
                .any(|b| b.key == "GRANITE_CLI_OPENCODE_API_KEY")
        );
    }

    #[tokio::test]
    async fn env_overlay_exports_one_api_key_per_distinct_provider_when_sub_agents_present() {
        let main = AgentModelBinding {
            api_key: Some(Secret::from("main-key")),
            ..binding()
        };
        let mut l = bound(serde_json::json!({}), main);
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            SubAgentBinding {
                model: AgentModelBinding {
                    provider_name: "other-provider".to_string(),
                    model_name: "other-model".to_string(),
                    api_key: Some(Secret::from("sub-key")),
                    ..binding()
                },
                ..sub_agent_binding("Reviews code", "other-provider", "other-model", vec![])
            },
        )];

        let overlay = l.env_overlay(&ctx(false)).await.unwrap();

        let main_key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_OPENCODE_API_KEY")
            .expect("main model's api key");
        assert_eq!(main_key.value, "main-key");

        let sub_key = overlay
            .iter()
            .find(|b| b.key == "GRANITE_CLI_OPENCODE_API_KEY_1")
            .expect("sub-agent's own api key, on its own suffixed env var");
        assert_eq!(sub_key.value, "sub-key");
    }

    #[tokio::test]
    async fn env_overlay_redirects_config_when_only_a_sub_agent_is_bound() {
        let mut l = launcher(serde_json::json!({}));
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding("Reviews code", "my-ollama", "granite4.1:8b", vec![]),
        )];
        let overlay = l.env_overlay(&ctx(false)).await.unwrap();
        assert!(
            overlay.iter().any(|b| b.key == "OPENCODE_CONFIG"),
            "a sub-agent alone (no main model, no MCP) must still redirect OPENCODE_CONFIG"
        );
    }

    // -- launch ----------------------------------------------------------------

    // Deliberately reads whatever `GRANITE_CLI_HOME` is ambient rather than
    // setting it: env mutation would race the other tests in this binary that
    // point that var at their own tempdirs.
    #[tokio::test]
    async fn dry_run_launch_reports_without_writing_anything() {
        let state_dir = crate::config::Config::launcher_state_dir("opencode").unwrap();
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
                .any(|m| m.contains("Would write OpenCode config")),
            "expected a dry-run notice, got {infos:?}"
        );
        assert!(
            infos
                .iter()
                .any(|m| m.contains(r#""model": "my-ollama/granite4.1:8b""#)),
            "expected the generated config to select the model, got {infos:?}"
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
        assert!(infos.iter().any(|m| m.contains("args: --version")));
        assert!(
            !infos
                .iter()
                .any(|m| m.contains("Would write OpenCode config"))
        );
        // Without a binding there is no generated config, so OpenCode keeps
        // using its own config chain.
        assert!(!infos.iter().any(|m| m.contains(CONFIG_ENV)));
    }

    #[tokio::test]
    async fn dry_run_launch_with_sub_agents_writes_agent_and_provider_blocks() {
        let mut l = bound(serde_json::json!({ "command_path": "ls" }), binding());
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding(
                "Reviews code",
                "other-provider",
                "other-model",
                vec![ToolName::FileRead],
            ),
        )];
        let ui = CaptureUi::default();
        l.launch(&[], &ctx(true), &ui).await.unwrap();

        let infos = ui.infos.borrow();
        let dump = infos.join("\n");
        assert!(dump.contains(r#""reviewer""#), "{dump}");
        assert!(dump.contains(r#""my-ollama/granite4.1:8b""#), "{dump}");
        assert!(dump.contains(r#""other-provider""#), "{dump}");
        // Both providers get their own entry, keyed by provider name.
        assert!(dump.contains(r#""my-ollama": {"#), "{dump}");
        assert!(dump.contains(r#""other-provider": {"#), "{dump}");
    }

    #[tokio::test]
    async fn dry_run_launch_with_only_a_sub_agent_still_writes_a_config() {
        let mut l = launcher(serde_json::json!({ "command_path": "ls" }));
        l.bound_sub_agents = vec![(
            "reviewer".to_string(),
            sub_agent_binding("Reviews code", "my-ollama", "granite4.1:8b", vec![]),
        )];
        let ui = CaptureUi::default();
        l.launch(&[], &ctx(true), &ui).await.unwrap();

        let infos = ui.infos.borrow();
        assert!(
            infos
                .iter()
                .any(|m| m.contains("Would write OpenCode config")),
            "a sub-agent alone (no main model, no MCP) must still trigger config generation, got {infos:?}"
        );
    }

    /// Minimal `Capability` double that always resolves to a fixed
    /// `SubAgentBinding`, mirroring `ClaudeLauncher`'s test of the same name.
    struct FakeSubAgentCapability {
        instance_id: String,
        binding: SubAgentBinding,
    }

    impl crate::registry::Named for FakeSubAgentCapability {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    #[async_trait]
    impl Capability for FakeSubAgentCapability {
        fn name(&self) -> &str {
            "Fake Sub-Agent"
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn dependencies(&self) -> Vec<crate::capabilities::Dependency> {
            vec![]
        }
        fn binding_types(&self) -> HashSet<BindingType> {
            HashSet::from([BindingType::SubAgent])
        }
        async fn bind(
            &self,
            _request: crate::capabilities::BindingRequest,
        ) -> anyhow::Result<Binding> {
            Ok(Binding::SubAgent(self.binding.clone()))
        }
    }

    #[tokio::test]
    async fn bind_capability_pushes_sub_agent_binding() {
        let mut l = launcher(serde_json::json!({}));
        let cap = FakeSubAgentCapability {
            instance_id: "reviewer".to_string(),
            binding: sub_agent_binding("Reviews code", "my-ollama", "granite4.1:8b", vec![]),
        };
        l.bind_capability(&cap).await.unwrap();
        assert_eq!(l.bound_sub_agents.len(), 1);
        assert_eq!(l.bound_sub_agents[0].0, "reviewer");
        assert_eq!(l.bound_sub_agents[0].1.model.model_name, "granite4.1:8b");
    }

    // -- proxy base url --------------------------------------------------------

    #[tokio::test]
    async fn proxy_base_url_returns_proxy_url_when_model_proxy_is_set() {
        let b = binding();
        let server = crate::proxy::ProxyServer::start().unwrap();
        let mut l = launcher(serde_json::json!({}));
        l.model_proxy = Some(server.handle.clone());
        assert_eq!(
            l.proxy_base_url(&b),
            server.handle.local_base_url
        );
        server.shutdown().await;
    }

    #[test]
    fn proxy_base_url_returns_regular_url_when_no_model_proxy() {
        let b = binding();
        let l = launcher(serde_json::json!({}));
        assert_eq!(l.proxy_base_url(&b), opencode_base_url(&b));
    }

    #[tokio::test]
    async fn provider_entry_uses_proxy_url_when_model_proxy_is_active() {
        let server = crate::proxy::ProxyServer::start().unwrap();
        let mut l = launcher(serde_json::json!({}));
        l.model_proxy = Some(server.handle.clone());
        let b = binding();
        let entry = l
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert_eq!(entry["options"]["baseURL"], server.handle.local_base_url);
        server.shutdown().await;
    }

    #[test]
    fn provider_entry_uses_real_url_when_no_model_proxy() {
        let l = launcher(serde_json::json!({}));
        let b = binding();
        let entry = l
            .provider_entry(&b, &[b.model_name.as_str()], API_KEY_ENV)
            .unwrap();
        assert_eq!(entry["options"]["baseURL"], opencode_base_url(&b));
    }

    #[tokio::test]
    async fn dry_run_launch_with_proxy_redirects_base_url() {
        let server = crate::proxy::ProxyServer::start().unwrap();
        let mut l = bound(serde_json::json!({ "command_path": "ls" }), binding());
        l.model_proxy = Some(server.handle.clone());
        let ui = CaptureUi::default();
        l.launch(&[], &ctx(true), &ui).await.unwrap();

        let infos = ui.infos.borrow();
        let dump = infos.join("\n");
        assert!(dump.contains(&server.handle.local_base_url), "{dump}");
        server.shutdown().await;
    }

    // -- JSONC parsing -----------------------------------------------------------

    #[test]
    fn strip_jsonc_comments_strips_line_comments() {
        let input = r#"{ "key": "value" // trailing comment }"#;
        let result = OpenCodeLauncher::strip_jsonc_comments(input);
        assert!(
            !result.contains("//"),
            "line comments should be stripped: {result}"
        );
        assert!(result.contains("\"value\""), "value should remain");
    }

    #[test]
    fn strip_jsonc_comments_strips_block_comments() {
        let input = r#"{ "key": "value" /* block comment */ }"#;
        let result = OpenCodeLauncher::strip_jsonc_comments(input);
        assert!(
            !result.contains("/*"),
            "block comments should be stripped: {result}"
        );
        assert!(result.contains("\"value\""));
    }

    #[test]
    fn strip_jsonc_comments_preserves_comments_in_strings() {
        let input = r#"{ "key": "// not a comment" }"#;
        let result = OpenCodeLauncher::strip_jsonc_comments(input);
        assert!(result.contains("// not a comment"));
    }

    #[test]
    fn strip_jsonc_comments_handles_multiline_block_comments() {
        let input = r#"{
            "key": "value" /*
                multiline
                comment
            */
        }"#;
        let result = OpenCodeLauncher::strip_jsonc_comments(input);
        assert!(!result.contains("multiline"));
        assert!(result.contains("\"value\""));
    }

    // -- env var resolution ------------------------------------------------------

    #[test]
    fn resolve_env_vars_resolves_string_placeholders() {
        unsafe { std::env::set_var("TEST_VAR_123", "resolved_value"); }
        let input = serde_json::json!({ "url": "{env:TEST_VAR_123}" });
        let result = OpenCodeLauncher::resolve_env_vars(&input);
        assert_eq!(result["url"], "resolved_value");
        unsafe { std::env::remove_var("TEST_VAR_123"); }
    }

    #[test]
    fn resolve_env_vars_uses_empty_string_for_unset_vars() {
        unsafe { std::env::remove_var("NONEXISTENT_VAR_XYZ"); }
        let input = serde_json::json!({ "url": "{env:NONEXISTENT_VAR_XYZ}" });
        let result = OpenCodeLauncher::resolve_env_vars(&input);
        assert_eq!(result["url"], "");
    }

    #[test]
    fn resolve_env_vars_leaves_non_placeholder_strings_untouched() {
        let input = serde_json::json!({ "url": "https://example.com" });
        let result = OpenCodeLauncher::resolve_env_vars(&input);
        assert_eq!(result["url"], "https://example.com");
    }

    #[test]
    fn resolve_env_vars_recurses_into_objects() {
        unsafe { std::env::set_var("TEST_URL_456", "https://resolved.com/v1"); }
        let input = serde_json::json!({
            "provider": {
                "options": {
                    "baseURL": "{env:TEST_URL_456}"
                }
            }
        });
        let result = OpenCodeLauncher::resolve_env_vars(&input);
        assert_eq!(result["provider"]["options"]["baseURL"], "https://resolved.com/v1");
        unsafe { std::env::remove_var("TEST_URL_456"); }
    }

    #[test]
    fn resolve_env_vars_recurses_into_arrays() {
        let input = serde_json::json!([1, "{env:HOME}", true]);
        let result = OpenCodeLauncher::resolve_env_vars(&input);
        assert_eq!(result[0], 1);
        assert_eq!(result[1], std::env::var("HOME").unwrap_or_default());
        assert_eq!(result[2], true);
    }

    // -- env providers discovery -------------------------------------------------

    #[test]
    fn discover_env_providers_returns_empty_when_no_keys_set() {
        // Save original values
        let saved = [
            ("OPENAI_API_KEY", std::env::var("OPENAI_API_KEY").ok()),
            ("ANTHROPIC_API_KEY", std::env::var("ANTHROPIC_API_KEY").ok()),
            ("GOOGLE_API_KEY", std::env::var("GOOGLE_API_KEY").ok()),
        ];
        // Clear the env vars
        for (key, _) in &saved {
            unsafe { std::env::remove_var(key); }
        }

        let providers = OpenCodeLauncher::discover_env_providers();
        assert!(
            !providers.contains_key("openai"),
            "openai should not be discovered"
        );
        assert!(
            !providers.contains_key("anthropic"),
            "anthropic should not be discovered"
        );
        assert!(
            !providers.contains_key("google"),
            "google should not be discovered"
        );

        // Restore original values
        for (key, orig) in &saved {
            if let Some(val) = orig {
                unsafe { std::env::set_var(key, val); }
            }
        }
    }

    // -- user config discovery ---------------------------------------------------

    #[test]
    fn extract_providers_returns_none_for_missing_provider_key() {
        let config = serde_json::json!({ "model": "openai/gpt-4o" });
        assert!(OpenCodeLauncher::extract_providers(&config).is_none());
    }

    #[test]
    fn extract_providers_returns_none_for_non_object_provider() {
        let config = serde_json::json!({ "provider": "invalid" });
        assert!(OpenCodeLauncher::extract_providers(&config).is_none());
    }

    #[test]
    fn extract_providers_returns_some_for_valid_provider_object() {
        let config = serde_json::json!({
            "provider": {
                "openai": { "name": "openai", "options": {} }
            }
        });
        let providers = OpenCodeLauncher::extract_providers(&config);
        assert!(providers.is_some());
        let providers = providers.unwrap();
        assert!(providers.contains_key("openai"));
    }
}
