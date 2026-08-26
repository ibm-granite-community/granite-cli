// Standard
use std::collections::HashSet;
use std::path::PathBuf;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    Binding, BindingType, Capability, KnownSubAgent, McpBinding, SubAgentBinding, ToolName,
};
use crate::launchers::base::HasLauncherMetadata as HasClaudeLauncherMetadata;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::{
    mcp_binding_request, register_mcp_server, remove_mcp_server,
};
use crate::proxy::ProxyHandle;
use crate::registry::ConfigConstructable;
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;

use_channel!("CLAUD");

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct ClaudeLauncherConfig {
    /// Override path to the `claude` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,
}

pub struct ClaudeLauncher {
    instance_id: String,
    config: ClaudeLauncherConfig,
    bound_agent_model: Option<crate::capabilities::AgentModelBinding>,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, registered/removed around `run_command` in `launch()`.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// `(name, binding)` for every `SubAgentCapability` bound to this
    /// launcher -- `name` is the capability's own `instance_id`, used as the
    /// sub-agent's name in the `--agents` JSON map. Each sub-agent's route
    /// was already registered on `model_proxy` by `ModelSource::take`; when
    /// non-empty, `launch()` (via `wire_model_proxy`) points
    /// `ANTHROPIC_BASE_URL` at that proxy so each sub-agent's model reaches
    /// its own resolved provider.
    bound_sub_agents: Vec<(String, SubAgentBinding)>,
    /// The session-scoped model proxy, if one was booted for this launch
    /// (see `run_launch`) -- present whenever usage tracking or sub-agent
    /// routing is needed.
    model_proxy: Option<ProxyHandle>,
}

impl ConfigConstructable for ClaudeLauncher {
    type Config = ClaudeLauncherConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        global_config: &crate::config::Config,
    ) -> Self {
        let config: ClaudeLauncherConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
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

impl crate::registry::Named for ClaudeLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
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
            let request = crate::capabilities::BindingRequest::SubAgent(
                crate::capabilities::SubAgentBindingRequest {
                    api_type: crate::providers::ApiType::Anthropic,
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

        // Claude knows it expects Anthropic API type
        let request = crate::capabilities::BindingRequest::AgentModel(
            crate::capabilities::AgentModelBindingRequest {
                api_type: crate::providers::ApiType::Anthropic,
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
        resolve_shell_command(&self.config.command_path, "claude")
    }

    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        if let Some(binding) = &self.bound_agent_model {
            let mut api_key_val = match &binding.api_key {
                Some(api_key) => api_key.clone().0,
                _ => "".to_string(),
            };
            if api_key_val.is_empty() {
                api_key_val = "unset".to_string(); // Claude treats empty strings like unset
            }
            let bindings = vec![
                EnvBinding {
                    key: "ANTHROPIC_BASE_URL".to_string(),
                    value: binding.base_url.clone(),
                },
                EnvBinding {
                    key: "ANTHROPIC_MODEL".to_string(),
                    value: binding.model_name.clone(),
                },
                EnvBinding {
                    key: "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
                    value: binding
                        .context_length
                        .map_or(String::new(), |v| v.to_string()),
                },
                EnvBinding {
                    key: "ANTHROPIC_AUTH_TOKEN".to_string(),
                    value: api_key_val,
                },
            ];
            // verify_ssl is dropped per user's note
            Ok(bindings)
        } else {
            Ok(vec![])
        }
    }

    /// Maps a canonical `ToolName` onto Claude Code's own built-in tool
    /// names (confirmed against the installed `claude` binary's compiled
    /// tool-name table) and its `mcp__<server>[__<tool>]` MCP tool-naming
    /// convention.
    fn map_tool_name(&self, tool: &ToolName) -> Option<String> {
        Some(match tool {
            ToolName::FileRead => "Read".to_string(),
            ToolName::FileWrite => "Write".to_string(),
            ToolName::FileEdit => "Edit".to_string(),
            ToolName::Search => "Grep".to_string(),
            ToolName::FileSearch => "Glob".to_string(),
            ToolName::Shell => "Bash".to_string(),
            ToolName::WebFetch => "WebFetch".to_string(),
            ToolName::WebSearch => "WebSearch".to_string(),
            ToolName::Mcp { server, tool: None } => format!("mcp__{server}"),
            ToolName::Mcp {
                server,
                tool: Some(t),
            } => format!("mcp__{server}__{t}"),
            ToolName::Other(raw) => raw.clone(),
        })
    }

    /// Registers each bound MCP server with `claude mcp add-json` (scoped
    /// `local` so it only applies to this invocation) before exec'ing, and
    /// best-effort removes them again afterwards -- failure to clean up is
    /// logged, not propagated, since the launch itself already succeeded or
    /// failed on its own terms by that point.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let mut overlay = self.env_overlay(ctx).await?;
        self.wire_model_proxy(ctx, &mut overlay)?;
        alog_channel!(MessageLevel::Debug4, "Env overlay: {:#?}", overlay);

        let mut full_args = Vec::new();
        if !self.bound_sub_agents.is_empty() {
            full_args.push("--agents".to_string());
            full_args.push(self.build_agents_json(ui));
        }
        full_args.extend_from_slice(args);

        const SCOPE: &[&str] = &["--scope", "local"];
        for (name, binding) in &self.bound_mcp_bindings {
            register_mcp_server(&binary, name, binding, SCOPE, ctx, ui)?;
        }

        alog_channel!(MessageLevel::Debug3, "Binary: {:#?}", &binary);
        alog_channel!(MessageLevel::Debug3, "Full args: {:#?}", full_args);
        let result = run_command(binary.clone(), &overlay, &full_args, ctx, ui).await;

        for (name, _) in &self.bound_mcp_bindings {
            remove_mcp_server(&binary, name, SCOPE, ctx, ui);
        }

        result
    }
}

impl ClaudeLauncher {
    /// Whenever the shared session proxy is running -- `-u`/`--usage-tracking`
    /// was requested, or any `SubAgentCapability` is bound (which forces the
    /// proxy on regardless of `-u`) -- overrides `overlay`'s
    /// `ANTHROPIC_BASE_URL` entry to point at it. This runs unconditionally,
    /// not just when sub-agents are bound: with `-u` alone and no
    /// `AgentModelCapability` configured, `env_overlay()` never sets
    /// `ANTHROPIC_BASE_URL` at all (there's no bound model to read it from),
    /// so without this override the launched process would talk straight to
    /// the real upstream on its own ambient environment and the proxy would
    /// never see any traffic to track -- the point of routing everything
    /// through the proxy is that its built-in default (ambient
    /// `ANTHROPIC_BASE_URL`/real Anthropic passthrough) still tracks that
    /// traffic, now labeled per the actual model name observed on each
    /// request (see `RoutingTable::target_and_label_for`).
    ///
    /// If any `SubAgentCapability` is also bound, points the proxy's default
    /// at whatever route was already registered under the main model's own
    /// name (if `AgentModelCapability` is bound too), so Claude Code's
    /// other, non-sub-agent traffic keeps reaching the user's configured
    /// main model rather than leaking to the real upstream. Note this does
    /// NOT register a route for each sub-agent's model itself --
    /// `ModelSource::take` already did that, using each model's real
    /// (unwrapped) provider, at the point `SubAgentCapability::new` resolved
    /// it. By the time a binding reaches here, `binding.model.base_url`/
    /// `api_key` have already been redirected to point at this same proxy
    /// (since the model went through `ModelSource::take` too) --
    /// re-deriving a route from them would register the proxy as its own
    /// upstream, an infinite loop; the same reasoning is why the main
    /// model's default is looked up by name rather than rebuilt from
    /// `bound_agent_model` directly.
    ///
    /// Under `--dry-run` (where `run_launch` never boots a proxy) with
    /// sub-agents bound, `overlay` still gets a placeholder value so the
    /// dry-run output stays informative.
    fn wire_model_proxy(
        &self,
        ctx: &LaunchContext,
        overlay: &mut Vec<EnvBinding>,
    ) -> anyhow::Result<()> {
        match &self.model_proxy {
            Some(handle) => {
                if let Some(main) = &self.bound_agent_model
                    && let Err(e) = handle.set_default_from_route(&main.model_name)
                {
                    alog_channel!(MessageLevel::Warning, "failed to set default route: {e}");
                }
                set_env_binding(overlay, "ANTHROPIC_BASE_URL", handle.local_base_url.clone());
            }
            None if !self.bound_sub_agents.is_empty() && ctx.dry_run => {
                set_env_binding(
                    overlay,
                    "ANTHROPIC_BASE_URL",
                    "<sub-agent router: not started under --dry-run>".to_string(),
                );
            }
            None if !self.bound_sub_agents.is_empty() => {
                // `run_launch` boots a proxy whenever any sub-agent
                // capability is enabled, so this should be unreachable
                // outside dry-run; degrade gracefully rather than failing
                // the whole launch over a routing bug.
                alog_channel!(
                    MessageLevel::Warning,
                    "sub-agents bound but no proxy handle available; sub-agent routing disabled for this launch"
                );
            }
            // No proxy running and no sub-agents bound: nothing to wire up
            // (either `-u` wasn't passed, or this is a dry run) -- leave
            // `overlay` exactly as `env_overlay()` produced it.
            None => {}
        }

        Ok(())
    }

    /// Builds the `--agents` JSON: `{ name: { description, prompt, model,
    /// tools? } }` for every bound sub-agent. `tools` is omitted entirely
    /// when empty -- an empty array would mean "no tools" to Claude Code,
    /// whereas omitting means "inherit all," the correct default. Each
    /// `ToolName` is mapped to Claude's own tool-name string via
    /// `map_tool_name`; a tool with no mapping is dropped with a warning
    /// (per sub-agent, so one unmappable tool doesn't sink the rest of that
    /// sub-agent's list, let alone the whole launch).
    fn build_agents_json(&self, ui: &dyn Ui) -> String {
        let agents: serde_json::Map<String, serde_json::Value> = self
            .bound_sub_agents
            .iter()
            .map(|(name, binding)| {
                let mut entry = serde_json::json!({
                    "description": binding.description,
                    "prompt": binding.prompt,
                    "model": binding.model.model_name,
                });
                let mapped_tools: Vec<String> = binding
                    .tools
                    .iter()
                    .filter_map(|tool| {
                        let mapped = self.map_tool_name(tool);
                        if mapped.is_none() {
                            ui.warn(&format!(
                                "sub-agent '{name}': tool {tool:?} has no mapping for the claude launcher, skipping"
                            ));
                        }
                        mapped
                    })
                    .collect();
                if !mapped_tools.is_empty() {
                    entry["tools"] = serde_json::json!(mapped_tools);
                }
                // Claude allows names to directly override some of the builtin
                // sub-agents. Here we map from known types to the internal name
                // so the override sticks.
                let mapped_name = match binding.known_type {
                    Some(KnownSubAgent::Explore) => "Explore".to_string(),
                    _ => name.clone(),
                };
                alog_channel!(MessageLevel::Debug2, "Using agent name {} for {}", &mapped_name, &name);
                (mapped_name, entry)
            })
            .collect();
        serde_json::Value::Object(agents).to_string()
    }
}

/// Overwrites `key`'s value in `overlay` if `env_overlay` already produced
/// an entry for it (the case when `AgentModelCapability` is also bound), or
/// appends one (the case when it isn't, so `env_overlay` produced nothing at
/// all).
fn set_env_binding(overlay: &mut Vec<EnvBinding>, key: &str, value: String) {
    match overlay.iter_mut().find(|b| b.key == key) {
        Some(binding) => binding.value = value,
        None => overlay.push(EnvBinding {
            key: key.to_string(),
            value,
        }),
    }
}

impl HasClaudeLauncherMetadata for ClaudeLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Claude CLI".to_string(),
            description: "Anthropic's Claude CLI tool".to_string(),
            default_command: "claude".to_string(),
            supported_capabilities: HashSet::from([
                BindingType::AgentModel,
                BindingType::Mcp,
                BindingType::SubAgent,
            ]),
            tags: vec!["claude".to_string(), "anthropic".to_string()],
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_to_claude() {
        let l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert_eq!(l.command(), "claude");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({
                "command_path": "/opt/bin/claude"
            }),
            &crate::config::Config::default(),
        );
        assert_eq!(l.command(), "/opt/bin/claude");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({
                "command_path": "/no/such/path/claude"
            }),
            &crate::config::Config::default(),
        );
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({
                "command_path": "ls"
            }),
            &crate::config::Config::default(),
        );
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
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<ClaudeLauncher>("claude");
        let schema = factory.config_schema("claude").unwrap();
        // Schema should reference ClaudeLauncherConfig properties
        let props = schema.get("properties").and_then(|p| p.as_object());
        assert!(props.is_some());
        assert!(props.unwrap().contains_key("command_path"));
    }

    #[test]
    fn metadata_supports_sub_agent_binding() {
        let meta = ClaudeLauncher::metadata();
        assert!(meta.supported_capabilities.contains(&BindingType::SubAgent));
    }

    #[test]
    fn map_tool_name_covers_every_canonical_variant_and_formats_mcp_references() {
        let l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileRead),
            Some("Read".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileWrite),
            Some("Write".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::FileEdit),
            Some("Edit".to_string())
        );
        assert_eq!(l.map_tool_name(&ToolName::Search), Some("Grep".to_string()));
        assert_eq!(
            l.map_tool_name(&ToolName::FileSearch),
            Some("Glob".to_string())
        );
        assert_eq!(l.map_tool_name(&ToolName::Shell), Some("Bash".to_string()));
        assert_eq!(
            l.map_tool_name(&ToolName::WebFetch),
            Some("WebFetch".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::WebSearch),
            Some("WebSearch".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: None,
            }),
            Some("mcp__vision".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: Some("vlm_compare_images".to_string()),
            }),
            Some("mcp__vision__vlm_compare_images".to_string())
        );
        assert_eq!(
            l.map_tool_name(&ToolName::Other("SomeRawClaudeTool".to_string())),
            Some("SomeRawClaudeTool".to_string())
        );
    }

    fn sub_agent_binding(
        description: &str,
        model_name: &str,
        tools: Vec<ToolName>,
    ) -> SubAgentBinding {
        SubAgentBinding {
            description: description.to_string(),
            prompt: "You are a helpful sub-agent.".to_string(),
            tools,
            model: crate::capabilities::AgentModelBinding {
                api_type: crate::providers::ApiType::Anthropic,
                provider_name: "my-ollama".to_string(),
                base_url: "http://localhost:11434".to_string(),
                model_name: model_name.to_string(),
                endpoint_path: "/v1/messages".to_string(),
                api_key: None,
                verify_ssl: true,
                context_length: Some(4096),
            },
            known_type: None,
        }
    }

    fn launcher_with(
        bound_agent_model: Option<crate::capabilities::AgentModelBinding>,
        bound_sub_agents: Vec<(String, SubAgentBinding)>,
    ) -> ClaudeLauncher {
        let mut l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        l.bound_agent_model = bound_agent_model;
        l.bound_sub_agents = bound_sub_agents;
        l
    }

    fn test_launch_context(dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "my-claude".to_string(),
            working_dir: std::env::current_dir().unwrap(),
            base_env: std::collections::HashMap::new(),
            dry_run,
        }
    }

    #[test]
    fn build_agents_json_includes_description_prompt_and_model_but_omits_empty_tools() {
        let ui = crate::utils::ui::backends::plain::PlainOutput;
        let l = launcher_with(
            None,
            vec![(
                "reviewer".to_string(),
                sub_agent_binding("Reviews code", "granite-3.1-8b-instruct", vec![]),
            )],
        );
        let json: serde_json::Value = serde_json::from_str(&l.build_agents_json(&ui)).unwrap();
        let entry = &json["reviewer"];
        assert_eq!(entry["description"], "Reviews code");
        assert_eq!(entry["prompt"], "You are a helpful sub-agent.");
        assert_eq!(entry["model"], "granite-3.1-8b-instruct");
        assert!(entry.get("tools").is_none());
    }

    #[test]
    fn build_agents_json_includes_tools_when_present() {
        let ui = crate::utils::ui::backends::plain::PlainOutput;
        let l = launcher_with(
            None,
            vec![(
                "reviewer".to_string(),
                sub_agent_binding(
                    "Reviews code",
                    "granite-3.1-8b-instruct",
                    vec![ToolName::FileRead, ToolName::Search],
                ),
            )],
        );
        let json: serde_json::Value = serde_json::from_str(&l.build_agents_json(&ui)).unwrap();
        assert_eq!(
            json["reviewer"]["tools"],
            serde_json::json!(["Read", "Grep"])
        );
    }

    #[test]
    fn build_agents_json_covers_every_bound_sub_agent_by_instance_id() {
        let ui = crate::utils::ui::backends::plain::PlainOutput;
        let l = launcher_with(
            None,
            vec![
                (
                    "reviewer".to_string(),
                    sub_agent_binding("Reviews code", "model-a", vec![]),
                ),
                (
                    "summarizer".to_string(),
                    sub_agent_binding("Summarizes text", "model-b", vec![]),
                ),
            ],
        );
        let json: serde_json::Value = serde_json::from_str(&l.build_agents_json(&ui)).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 2);
        assert_eq!(json["reviewer"]["model"], "model-a");
        assert_eq!(json["summarizer"]["model"], "model-b");
    }

    #[test]
    fn wire_model_proxy_is_a_noop_without_a_handle_or_sub_agents() {
        let l = launcher_with(None, vec![]);
        let mut overlay = vec![];
        l.wire_model_proxy(&test_launch_context(false), &mut overlay)
            .unwrap();
        assert!(overlay.is_empty());
    }

    #[tokio::test]
    async fn wire_model_proxy_points_at_the_proxy_even_with_no_bound_model_or_sub_agents() {
        // The scenario this covers: `-u`/`--usage-tracking` alone, with no
        // `AgentModelCapability` or `SubAgentCapability` configured at all.
        // `env_overlay()` never sets `ANTHROPIC_BASE_URL` in that case (no
        // bound model to read it from), so without this, the launched
        // process would never reach the proxy and nothing would ever be
        // tracked.
        let server = crate::proxy::ProxyServer::start().unwrap();
        let mut l = launcher_with(None, vec![]);
        l.model_proxy = Some(server.handle.clone());

        let mut overlay = vec![];
        l.wire_model_proxy(&test_launch_context(false), &mut overlay)
            .unwrap();
        assert_eq!(overlay.len(), 1);
        assert_eq!(overlay[0].key, "ANTHROPIC_BASE_URL");
        assert_eq!(overlay[0].value, server.handle.local_base_url);

        server.shutdown().await;
    }

    #[test]
    fn wire_model_proxy_sets_placeholder_under_dry_run_without_a_handle() {
        let l = launcher_with(
            None,
            vec![(
                "reviewer".to_string(),
                sub_agent_binding("Reviews code", "granite-3.1-8b-instruct", vec![]),
            )],
        );
        let mut overlay = vec![];
        l.wire_model_proxy(&test_launch_context(true), &mut overlay)
            .unwrap();
        assert_eq!(overlay.len(), 1);
        assert_eq!(overlay[0].key, "ANTHROPIC_BASE_URL");
        assert_eq!(
            overlay[0].value,
            "<sub-agent router: not started under --dry-run>"
        );
    }

    #[test]
    fn wire_model_proxy_warns_and_leaves_overlay_untouched_without_a_handle_outside_dry_run() {
        let l = launcher_with(
            None,
            vec![(
                "reviewer".to_string(),
                sub_agent_binding("Reviews code", "granite-3.1-8b-instruct", vec![]),
            )],
        );
        let mut overlay = vec![];
        l.wire_model_proxy(&test_launch_context(false), &mut overlay)
            .unwrap();
        assert!(overlay.is_empty());
    }

    #[tokio::test]
    async fn wire_model_proxy_points_default_at_the_main_models_registered_route() {
        async fn echo(
            headers: axum::http::HeaderMap,
            body: axum::body::Bytes,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let api_key = headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            axum::Json(serde_json::json!({ "model": value.get("model"), "x_api_key": api_key }))
                .into_response()
        }

        async fn spawn_echo_server() -> std::net::SocketAddr {
            let app = axum::Router::new().route("/v1/messages", axum::routing::post(echo));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            addr
        }

        let sub_agent_addr = spawn_echo_server().await;
        let main_addr = spawn_echo_server().await;

        let server = crate::proxy::ProxyServer::start().unwrap();
        // Simulates what `ModelSource::take` already did for each model at
        // capability-construction time, using each one's real (unwrapped)
        // provider -- this is the ONLY place routes get registered; the
        // launcher itself must not try to re-derive them from bindings,
        // since those already point back at this same proxy once wrapped.
        server
            .handle
            .register_route(
                "granite-3.1-8b-instruct".to_string(),
                crate::proxy::UpstreamTarget {
                    base_url: format!("http://{sub_agent_addr}"),
                    verify_ssl: true,
                    auth: crate::proxy::UpstreamAuth::Inject(Some(crate::registry::Secret(
                        "sub-key".to_string(),
                    ))),
                },
                "reviewer".to_string(),
            )
            .unwrap();
        server
            .handle
            .register_route(
                "main-model".to_string(),
                crate::proxy::UpstreamTarget {
                    base_url: format!("http://{main_addr}"),
                    verify_ssl: true,
                    auth: crate::proxy::UpstreamAuth::Inject(Some(crate::registry::Secret(
                        "main-key".to_string(),
                    ))),
                },
                "main-model".to_string(),
            )
            .unwrap();

        let mut l = launcher_with(
            // As it would look once wrapped: base_url points at the proxy,
            // api_key is cleared -- neither is used by the fix, only
            // `model_name` is (to look up the already-registered route).
            Some(crate::capabilities::AgentModelBinding {
                api_type: crate::providers::ApiType::Anthropic,
                provider_name: "main".to_string(),
                base_url: server.handle.local_base_url.clone(),
                model_name: "main-model".to_string(),
                endpoint_path: "/v1/messages".to_string(),
                api_key: None,
                verify_ssl: true,
                context_length: Some(4096),
            }),
            vec![(
                "reviewer".to_string(),
                sub_agent_binding("Reviews code", "granite-3.1-8b-instruct", vec![]),
            )],
        );
        l.model_proxy = Some(server.handle.clone());

        let mut overlay = vec![];
        l.wire_model_proxy(&test_launch_context(false), &mut overlay)
            .unwrap();
        assert_eq!(overlay[0].key, "ANTHROPIC_BASE_URL");
        assert_eq!(overlay[0].value, server.handle.local_base_url);

        let client = reqwest::Client::new();
        // The sub-agent's own registered route is untouched.
        let sub_resp: serde_json::Value = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({"model": "granite-3.1-8b-instruct"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sub_resp["x_api_key"], "sub-key");

        // An unmatched model now falls through to the main model's own
        // already-registered route, not back into the proxy itself.
        let main_resp: serde_json::Value = client
            .post(format!("{}/v1/messages", server.handle.local_base_url))
            .json(&serde_json::json!({"model": "some-other-internal-model"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(main_resp["x_api_key"], "main-key");

        server.shutdown().await;
    }

    #[test]
    fn set_env_binding_overwrites_existing_entry() {
        let mut overlay = vec![EnvBinding {
            key: "ANTHROPIC_BASE_URL".to_string(),
            value: "http://original".to_string(),
        }];
        set_env_binding(
            &mut overlay,
            "ANTHROPIC_BASE_URL",
            "http://router".to_string(),
        );
        assert_eq!(overlay.len(), 1);
        assert_eq!(overlay[0].value, "http://router");
    }

    #[test]
    fn set_env_binding_appends_when_absent() {
        let mut overlay = vec![];
        set_env_binding(
            &mut overlay,
            "ANTHROPIC_BASE_URL",
            "http://router".to_string(),
        );
        assert_eq!(overlay.len(), 1);
        assert_eq!(overlay[0].key, "ANTHROPIC_BASE_URL");
        assert_eq!(overlay[0].value, "http://router");
    }

    /// Minimal `Capability` double that always resolves to a fixed
    /// `SubAgentBinding`, so `bind_capability`'s dispatch can be exercised
    /// without going through real model/provider resolution.
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
        let mut l = ClaudeLauncher::new(
            "my-claude",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        let cap = FakeSubAgentCapability {
            instance_id: "reviewer".to_string(),
            binding: sub_agent_binding("Reviews code", "granite-3.1-8b-instruct", vec![]),
        };
        l.bind_capability(&cap).await.unwrap();
        assert_eq!(l.bound_sub_agents.len(), 1);
        assert_eq!(l.bound_sub_agents[0].0, "reviewer");
        assert_eq!(
            l.bound_sub_agents[0].1.model.model_name,
            "granite-3.1-8b-instruct"
        );
    }
}
