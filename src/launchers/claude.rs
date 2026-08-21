// Standard
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

// Third Party
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    Binding, BindingType, Capability, McpBinding, SubAgentBinding, ToolName,
};
use crate::launchers::base::HasLauncherMetadata as HasClaudeLauncherMetadata;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::{
    mcp_binding_request, register_mcp_server, remove_mcp_server,
};
use crate::launchers::shared::model_router::{ModelRouter, UpstreamAuth, UpstreamTarget};
use crate::registry::ConfigConstructable;
use crate::utils::resolve_shell_command;
use crate::utils::ui::Ui;

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
    /// sub-agent's name in the `--agents` JSON map. When non-empty, `launch()`
    /// starts a `ModelRouter` in front of `ANTHROPIC_BASE_URL` so each
    /// sub-agent's model reaches its own resolved provider.
    bound_sub_agents: Vec<(String, SubAgentBinding)>,
}

impl ConfigConstructable for ClaudeLauncher {
    type Config = ClaudeLauncherConfig;

    fn new(
        instance_id: &str,
        cfg: &serde_json::Value,
        _global_config: &crate::config::Config,
    ) -> Self {
        let config: ClaudeLauncherConfig = serde_json::from_value(cfg.clone()).unwrap_or_default();
        Self {
            instance_id: instance_id.to_string(),
            config,
            bound_agent_model: None,
            bound_mcp_bindings: vec![],
            bound_sub_agents: vec![],
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
        let router = self.start_sub_agent_router(ctx, &mut overlay)?;

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

        let result = run_command(binary.clone(), &overlay, &full_args, ctx, ui).await;

        for (name, _) in &self.bound_mcp_bindings {
            remove_mcp_server(&binary, name, SCOPE, ctx, ui);
        }

        if let Some(router) = router {
            router.shutdown().await;
        }

        result
    }
}

impl ClaudeLauncher {
    /// If any `SubAgentCapability` is bound, starts a `ModelRouter` in front
    /// of whatever `ANTHROPIC_BASE_URL` would otherwise be and overrides
    /// `overlay`'s `ANTHROPIC_BASE_URL` entry to point at it, so a sub-agent's
    /// model reaches its own resolved provider while everything else (the
    /// main session's own traffic) keeps reaching the normal upstream. Under
    /// `--dry-run`, no socket is started, but `overlay` still gets a
    /// placeholder value so the dry-run output stays informative.
    fn start_sub_agent_router(
        &self,
        ctx: &LaunchContext,
        overlay: &mut Vec<EnvBinding>,
    ) -> anyhow::Result<Option<ModelRouter>> {
        if self.bound_sub_agents.is_empty() {
            return Ok(None);
        }

        let router = if ctx.dry_run {
            None
        } else {
            let routes = self
                .bound_sub_agents
                .iter()
                .map(|(_, binding)| {
                    (
                        binding.model.model_name.clone(),
                        UpstreamTarget {
                            base_url: binding.model.base_url.clone(),
                            verify_ssl: binding.model.verify_ssl,
                            auth: UpstreamAuth::Inject(binding.model.api_key.clone()),
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            let ambient_base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
            Some(ModelRouter::start(
                self.default_upstream_target(ambient_base_url.as_deref()),
                routes,
            )?)
        };

        let base_url_override = match &router {
            Some(router) => router.local_base_url.clone(),
            None => "<sub-agent router: not started under --dry-run>".to_string(),
        };
        set_env_binding(overlay, "ANTHROPIC_BASE_URL", base_url_override);

        Ok(router)
    }

    /// The default target a sub-agent router forwards non-matching (main
    /// session) traffic to: the main model's provider if `AgentModelCapability`
    /// is also bound, or the real Anthropic API otherwise -- using
    /// `ambient_anthropic_base_url` (the caller's own `ANTHROPIC_BASE_URL`,
    /// read from `start_sub_agent_router`'s process environment) if set,
    /// falling back to the well-known default. Taking this as a parameter
    /// rather than reading the environment directly here keeps the fallback
    /// logic deterministically testable.
    fn default_upstream_target(&self, ambient_anthropic_base_url: Option<&str>) -> UpstreamTarget {
        match &self.bound_agent_model {
            Some(binding) => UpstreamTarget {
                base_url: binding.base_url.clone(),
                verify_ssl: binding.verify_ssl,
                auth: UpstreamAuth::Inject(binding.api_key.clone()),
            },
            None => UpstreamTarget {
                base_url: ambient_anthropic_base_url
                    .map(str::to_string)
                    .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
                verify_ssl: true,
                auth: UpstreamAuth::Passthrough,
            },
        }
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
                (name.clone(), entry)
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
    fn default_upstream_target_falls_back_to_well_known_default_without_a_main_model() {
        let l = launcher_with(None, vec![]);
        let target = l.default_upstream_target(None);
        assert_eq!(target.base_url, "https://api.anthropic.com");
        assert!(matches!(target.auth, UpstreamAuth::Passthrough));
    }

    #[test]
    fn default_upstream_target_uses_ambient_base_url_without_a_main_model() {
        let l = launcher_with(None, vec![]);
        let target = l.default_upstream_target(Some("http://corporate-gateway.internal"));
        assert_eq!(target.base_url, "http://corporate-gateway.internal");
        assert!(matches!(target.auth, UpstreamAuth::Passthrough));
    }

    #[test]
    fn default_upstream_target_prefers_bound_main_model_over_ambient_env() {
        let main_model = crate::capabilities::AgentModelBinding {
            api_type: crate::providers::ApiType::Anthropic,
            provider_name: "my-ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            model_name: "granite-3.1-8b-instruct".to_string(),
            endpoint_path: "/v1/messages".to_string(),
            api_key: Some(crate::registry::Secret("real-key".to_string())),
            verify_ssl: true,
            context_length: Some(4096),
        };
        let l = launcher_with(Some(main_model), vec![]);
        let target = l.default_upstream_target(Some("http://corporate-gateway.internal"));
        assert_eq!(target.base_url, "http://localhost:11434");
        assert!(matches!(target.auth, UpstreamAuth::Inject(Some(_))));
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
