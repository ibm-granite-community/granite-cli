// Standard
use std::collections::HashSet;
use std::path::PathBuf;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use anyhow::Context;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{
    ApiType, Binding, BindingRequest, BindingType, McpBinding, ResolvedCapability, SubAgentBinding,
    SubAgentBindingRequest,
};
use crate::launchers::base::HasLauncherMetadata as HasBobLauncherMetadata;
use crate::launchers::base::{EnvBinding, LaunchContext, Launcher, LauncherMetadata, run_command};
use crate::launchers::shared::mcp_cli::{
    mcp_binding_request, register_mcp_server, remove_mcp_server,
};
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::resolve_shell_command;
use crate::utils::subserver::SubServer;
use crate::utils::ui::Ui;

mod delegate;
pub(crate) mod hook;
mod usage;

use_channel!("BOB");

/*-- public --*/

const DEFAULT_USAGE_POLL_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct BobLauncherConfig {
    /// Override path to the `bob` binary for non-PATH installs.
    /// Leave unset to use PATH lookup.
    #[serde(default)]
    pub command_path: Option<String>,

    /// Override path to the `pi` binary used to run any bound sub-agents
    /// (see `bob/delegate.rs`). Leave unset to use PATH lookup, falling back
    /// to a cached/downloaded copy if `pi` isn't found there either. Distinct
    /// from `command_path`, which overrides `bob` itself -- the two binaries
    /// are unrelated.
    #[serde(default)]
    pub pi_command_path: Option<String>,

    /// Override path to bob's SQLite database for usage polling.
    /// Leave unset to use `~/.bob/db/bob.db`.
    #[serde(default)]
    pub bob_db_path: Option<String>,

    /// Seconds between usage polls while bob is running. Default 5.
    /// Leave unset to use the default (5s).
    #[serde(default)]
    pub usage_poll_interval_secs: Option<u64>,
}

pub struct BobLauncher {
    instance_id: String,
    config: BobLauncherConfig,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, registered/removed around `run_command` in `launch()`.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// `(tool_name, binding)` for every SubAgent-capable capability bound to
    /// this launcher. Not registered directly -- `launch()` wraps all of
    /// these into a single in-process MCP server (`bob/delegate.rs`) and
    /// registers *that* server instead.
    pending_sub_agents: Vec<(String, SubAgentBinding)>,
}

impl ConfigConstructable for BobLauncher {
    type Config = BobLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: BobLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_mcp_bindings: vec![],
            pending_sub_agents: vec![],
        })
    }
}

impl BobLauncher {
    /// Resolve the effective DB path: use the config override if set,
    /// otherwise `~/.bob/db/bob.db`. Falls back to a relative path if
    /// `dirs::home_dir()` is `None`.
    pub(crate) fn bob_db_path(&self) -> PathBuf {
        if let Some(ref p) = self.config.bob_db_path {
            return PathBuf::from(p);
        }
        dirs::home_dir()
            .unwrap_or_default()
            .join(".bob")
            .join("db")
            .join("bob.db")
    }

    /// Resolve the effective usage-poll interval: the config override if set,
    /// otherwise the default (5s). Defensively floors at 1 second even if a
    /// config value of 0 somehow slips through (e.g. a hand-edited config
    /// file), since `tokio::time::interval` panics on a zero duration and this
    /// whole feature must never crash a launch over a config quirk.
    pub(crate) fn usage_poll_interval(&self) -> std::time::Duration {
        let secs = self
            .config
            .usage_poll_interval_secs
            .unwrap_or(DEFAULT_USAGE_POLL_INTERVAL_SECS)
            .max(1);
        std::time::Duration::from_secs(secs)
    }
}

impl crate::registry::Named for BobLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
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

    async fn bind_capability(&mut self, capability: &dyn ResolvedCapability) -> anyhow::Result<()> {
        let supported = Self::metadata().supported_capabilities;
        let capability_types = capability.binding_types();
        if !capability_types.is_subset(&supported) {
            anyhow::bail!(
                "capability supports {:?} which this launcher does not support",
                capability_types.difference(&supported).collect::<Vec<_>>()
            );
        }

        if capability_types.contains(&BindingType::SubAgent) {
            let request = BindingRequest::SubAgent(SubAgentBindingRequest {
                api_type: ApiType::OpenAI,
            });
            let binding = capability.bind(request).await?;
            match binding {
                Binding::SubAgent(binding) => {
                    self.pending_sub_agents
                        .push((capability.instance_id().to_string(), binding));
                }
                other => {
                    anyhow::bail!(
                        "expected a SubAgent binding, got {:?}",
                        other.binding_type()
                    )
                }
            }
            return Ok(());
        }

        let binding = capability.bind(mcp_binding_request()).await?;
        match binding {
            Binding::Mcp(binding) => {
                self.bound_mcp_bindings
                    .push((capability.instance_id().to_string(), binding));
            }
            other => anyhow::bail!("expected an Mcp binding, got {:?}", other.binding_type()),
        }
        Ok(())
    }

    fn validate_command(&self) -> anyhow::Result<PathBuf> {
        resolve_shell_command(&self.config.command_path, "bob")
    }

    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        Ok(vec![])
    }

    /// Registers each bound MCP server with `bob mcp add-json` (scoped to
    /// this workspace) before exec'ing, and best-effort removes them again
    /// afterwards.
    ///
    /// When `ctx.usage_tracker` is `Some`, also registers a Bob `SessionStart`
    /// lifecycle hook to capture the task ID, polls Bob's SQLite DB for usage
    /// while the session runs, and flushes a final read at the end.
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;

        // Ensure `<workspace>/.bob/` exists. Needed both for workspace-scoped
        // MCP config registration (issue #144) and for the usage-tracking hook's
        // `settings.json`. Early-returns when `dry_run` so this call is safe
        // to run unconditionally.
        ensure_workspace_config_dir(ctx)?;

        // Register the SessionStart usage-tracking hook *before* any MCP or
        // delegate-server resources are set up below, so that a collision
        // failure (another live granite-cli-managed Bob session is already
        // tracking usage in this workspace) aborts the launch with nothing to
        // tear down yet. `None` when there's no proxy running (dry_run) --
        // there's nothing to poll in that case, so skip the hook entirely.
        let hook_reg = match &ctx.usage_tracker {
            Some(_) => Some(hook::register_or_fail(&ctx.launcher_id, &ctx.working_dir)?),
            None => None,
        };

        let mut delegate_server: Option<SubServer> = None;
        let mut all_mcp_bindings: Vec<(String, McpBinding)> = self.bound_mcp_bindings.clone();
        let mut args = args.to_vec();
        if !self.pending_sub_agents.is_empty() {
            let (binding, server) = delegate::start_delegate_mcp_server(
                self.pending_sub_agents.clone(),
                &self.config.pi_command_path,
                ctx,
                ui,
            )
            .await?;
            // Disable internal sub-agents if providing them via MCP
            args.push("--disable-subagents".to_string());
            all_mcp_bindings.push(("bob-sub-agents".to_string(), binding));
            delegate_server = Some(server);
        }

        const SCOPE: &[&str] = &["-s", "workspace"];
        for (name, binding) in &all_mcp_bindings {
            register_mcp_server(&binary, name, binding, SCOPE, ctx, ui)?;
        }

        alog_channel!(
            MessageLevel::Debug,
            "Running bob command: {:#?} {:#?}",
            &binary,
            &args
        );

        // If a usage tracker + hook registration are available, wire up
        // Bob's own usage tracking via periodic DB reads while `run_command`
        // runs. `captured_task_id` is declared out here (not inside the
        // branch below) so the guaranteed final flush after MCP/delegate
        // cleanup can still see it.
        let mut captured_task_id: Option<String> = None;
        let result = if let (Some(tracker), Some(hook_reg)) = (&ctx.usage_tracker, &hook_reg) {
            // Build the DB path for usage polling.
            let bob_db_path = self.bob_db_path();
            let poll_interval = self.usage_poll_interval();

            // Race the run_command future against usage-tracking work.
            let mut check_tick = tokio::time::interval(std::time::Duration::from_millis(500));
            let mut poll_tick = tokio::time::interval(poll_interval);
            poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            let run_fut = run_command(binary.clone(), &overlay, &args, ctx, ui);
            tokio::pin!(run_fut);

            loop {
                if captured_task_id.is_none() {
                    tokio::select! {
                        r = &mut run_fut => break r,
                        _ = check_tick.tick() => {
                            if let Some(id) = hook::try_read_capture(&hook_reg.capture_path) {
                                hook::unregister(&ctx.launcher_id, &ctx.working_dir, &hook_reg.marker_command);
                                let _ = std::fs::remove_file(&hook_reg.capture_path);
                                captured_task_id = Some(id);
                            }
                        }
                    }
                } else {
                    tokio::select! {
                        r = &mut run_fut => break r,
                        _ = poll_tick.tick() => {
                            let id = captured_task_id.clone().unwrap();
                            let db_path = bob_db_path.clone();
                            let stats = tokio::task::spawn_blocking(move || {
                                usage::collect_bob_usage(&db_path, &id)
                            })
                            .await
                            .unwrap_or_default();
                            tracker.set("bob", stats);
                        }
                    }
                }
            }
        } else {
            // No usage tracker (dry_run): just run the command directly.
            run_command(binary.clone(), &overlay, &args, ctx, ui).await
        };

        for (name, _) in &all_mcp_bindings {
            remove_mcp_server(&binary, name, SCOPE, ctx, ui);
        }

        if let Some(server) = delegate_server {
            server.shutdown().await;
        }

        // Guaranteed final flush + hook cleanup, mirrored to happen last
        // (LIFO relative to registration above) and regardless of how
        // `run_command` finished.
        if let (Some(tracker), Some(hook_reg)) = (&ctx.usage_tracker, &hook_reg) {
            if let Some(id) = &captured_task_id {
                let db_path = self.bob_db_path();
                let id = id.clone();
                let stats =
                    tokio::task::spawn_blocking(move || usage::collect_bob_usage(&db_path, &id))
                        .await
                        .unwrap_or_default();
                tracker.set("bob", stats);
            }
            hook::unregister(&ctx.launcher_id, &ctx.working_dir, &hook_reg.marker_command);
            let _ = std::fs::remove_file(&hook_reg.capture_path);
        }

        result
    }
}

impl HasBobLauncherMetadata for BobLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Bob CLI".to_string(),
            description: "IBM Bob AI assistant CLI".to_string(),
            default_command: "bob".to_string(),
            supported_capabilities: HashSet::from([BindingType::Mcp, BindingType::SubAgent]),
            tags: vec!["bob".to_string(), "ibm".to_string()],
        }
    }
}

/*-- private --*/

/// Bob stores workspace-scoped MCP config at `<workspace>/.bob/mcp.json` and
/// expects the directory to already exist; it does not create it itself, so
/// registration fails with ENOENT when missing (issue #144).
const WORKSPACE_CONFIG_DIR: &str = ".bob";

/// Creates the workspace-scoped config directory the downstream `bob` binary
/// writes into, unless this is a dry run (which must not touch the
/// filesystem). Called unconditionally near the top of `BobLauncher::launch()`;
/// needed for both workspace-scoped MCP config registration (issue #144) and
/// the usage-tracking hook's `settings.json` file.
fn ensure_workspace_config_dir(ctx: &LaunchContext) -> anyhow::Result<()> {
    if ctx.dry_run {
        return Ok(());
    }
    let dir = ctx.working_dir.join(WORKSPACE_CONFIG_DIR);
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "failed to create bob workspace config directory `{}`; bob expects it to exist \
             for workspace-scoped MCP registration and does not create it itself",
            dir.display()
        )
    })
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_to_bob() {
        let l = BobLauncher::new("my-bob", &serde_json::json!({})).unwrap();
        assert_eq!(l.command(), "bob");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "command_path": "/opt/bin/bob"
            }),
        )
        .unwrap();
        assert_eq!(l.command(), "/opt/bin/bob");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "command_path": "/no/such/path/bob"
            }),
        )
        .unwrap();
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "command_path": "ls"
            }),
        )
        .unwrap();
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
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<BobLauncher>("bob");
        let schema = factory.config_schema("bob").unwrap();
        let props = schema.get("properties").and_then(|p| p.as_object());
        assert!(props.is_some());
        let props = props.unwrap();
        assert!(props.contains_key("command_path"));
        assert!(props.contains_key("pi_command_path"));
        assert!(props.contains_key("bob_db_path"));
        assert!(props.contains_key("usage_poll_interval_secs"));
    }

    #[test]
    fn pi_command_path_is_independent_of_bobs_own_command_path() {
        // Regression guard: overriding bob's own binary must not also be
        // treated as an override for the `pi` binary the sub-agent delegate
        // server resolves -- the two are unrelated commands.
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({ "command_path": "/opt/bin/bob" }),
        )
        .unwrap();
        assert_eq!(l.config.command_path, Some("/opt/bin/bob".to_string()));
        assert_eq!(l.config.pi_command_path, None);
    }

    #[test]
    fn metadata_supported_capabilities_contains_mcp_and_sub_agent() {
        let meta = BobLauncher::metadata();
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
        assert!(meta.supported_capabilities.contains(&BindingType::SubAgent));
    }

    #[test]
    fn bob_db_path_defaults_when_not_configured() {
        let l = BobLauncher::new("my-bob", &serde_json::json!({})).unwrap();
        let db_path = l.bob_db_path();
        assert!(db_path.ends_with(".bob/db/bob.db"));
    }

    #[test]
    fn bob_db_path_uses_config_override() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "bob_db_path": "/custom/path/bob.db"
            }),
        )
        .unwrap();
        let db_path = l.bob_db_path();
        assert_eq!(db_path, std::path::PathBuf::from("/custom/path/bob.db"));
    }

    #[test]
    fn config_defaults_usage_poll_interval_to_none() {
        let l = BobLauncher::new("my-bob", &serde_json::json!({})).unwrap();
        assert_eq!(l.config.usage_poll_interval_secs, None);
    }

    #[test]
    fn config_uses_explicit_usage_poll_interval() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "usage_poll_interval_secs": 10
            }),
        )
        .unwrap();
        assert_eq!(l.config.usage_poll_interval_secs, Some(10));
    }

    #[test]
    fn config_bob_db_path_and_poll_interval_can_be_set_together() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "bob_db_path": "/other/db.db",
                "usage_poll_interval_secs": 3
            }),
        )
        .unwrap();
        assert_eq!(l.config.bob_db_path, Some("/other/db.db".to_string()));
        assert_eq!(l.config.usage_poll_interval_secs, Some(3));
    }

    // -- usage_poll_interval accessor ------------------------------------------

    #[test]
    fn usage_poll_interval_defaults_to_five_seconds_when_unset() {
        let l = BobLauncher::new("my-bob", &serde_json::json!({})).unwrap();
        assert_eq!(l.usage_poll_interval(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn usage_poll_interval_uses_config_when_set() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "usage_poll_interval_secs": 15
            }),
        )
        .unwrap();
        assert_eq!(l.usage_poll_interval(), std::time::Duration::from_secs(15));
    }

    #[test]
    fn usage_poll_interval_floors_at_one_second_when_config_is_explicitly_zero() {
        let l = BobLauncher::new(
            "my-bob",
            &serde_json::json!({
                "usage_poll_interval_secs": 0
            }),
        )
        .unwrap();
        assert_eq!(l.usage_poll_interval(), std::time::Duration::from_secs(1));
    }

    // -- bind_capability routing -------------------------------------------

    struct FakeMcpCapability;

    impl crate::registry::Named for FakeMcpCapability {
        fn instance_id(&self) -> &str {
            "fake-mcp"
        }
    }

    impl crate::capabilities::CapabilityInfo for FakeMcpCapability {
        fn name(&self) -> &str {
            "Fake Mcp"
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn binding_types(&self) -> HashSet<BindingType> {
            HashSet::from([BindingType::Mcp])
        }
    }

    #[async_trait]
    impl crate::capabilities::ResolvedCapability for FakeMcpCapability {
        async fn bind(&self, _request: BindingRequest) -> anyhow::Result<Binding> {
            Ok(Binding::Mcp(McpBinding::Http {
                url: "http://127.0.0.1:1/mcp".to_string(),
                headers: Default::default(),
                timeout: None,
            }))
        }
    }

    struct FakeSubAgentCapability;

    impl crate::registry::Named for FakeSubAgentCapability {
        fn instance_id(&self) -> &str {
            "fake-sub-agent"
        }
    }

    impl crate::capabilities::CapabilityInfo for FakeSubAgentCapability {
        fn name(&self) -> &str {
            "Fake SubAgent"
        }
        fn description(&self) -> &str {
            "test double"
        }
        fn binding_types(&self) -> HashSet<BindingType> {
            HashSet::from([BindingType::SubAgent])
        }
    }

    #[async_trait]
    impl crate::capabilities::ResolvedCapability for FakeSubAgentCapability {
        async fn bind(&self, _request: BindingRequest) -> anyhow::Result<Binding> {
            Ok(Binding::SubAgent(SubAgentBinding {
                description: "explores the repo".to_string(),
                prompt: "You explore things.".to_string(),
                tools: vec![],
                model: crate::capabilities::AgentModelBinding {
                    api_type: ApiType::OpenAI,
                    provider_name: "my-ollama".to_string(),
                    base_url: "http://localhost:11434".to_string(),
                    model_name: "granite4.1:8b".to_string(),
                    endpoint_path: "/v1/chat/completions".to_string(),
                    api_key: None,
                    verify_ssl: true,
                    context_length: Some(131072),
                    custom_headers: None,
                },
                known_type: None,
            }))
        }
    }

    fn bob() -> BobLauncher {
        BobLauncher::new("my-bob", &serde_json::json!({})).unwrap()
    }

    #[tokio::test]
    async fn bind_capability_with_sub_agent_only_capability_routes_into_pending_sub_agents() {
        let mut l = bob();
        l.bind_capability(&FakeSubAgentCapability).await.unwrap();
        assert!(l.bound_mcp_bindings.is_empty());
        assert_eq!(l.pending_sub_agents.len(), 1);
        assert_eq!(l.pending_sub_agents[0].0, "fake-sub-agent");
        assert_eq!(l.pending_sub_agents[0].1.description, "explores the repo");
    }

    #[tokio::test]
    async fn bind_capability_with_mcp_only_capability_still_populates_bound_mcp_bindings() {
        let mut l = bob();
        l.bind_capability(&FakeMcpCapability).await.unwrap();
        assert_eq!(l.bound_mcp_bindings.len(), 1);
        assert_eq!(l.bound_mcp_bindings[0].0, "fake-mcp");
        assert!(l.pending_sub_agents.is_empty());
    }

    // -- workspace config dir ----------------------------------------------

    fn launch_ctx(working_dir: PathBuf, dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "my-bob".to_string(),
            working_dir,
            base_env: std::collections::HashMap::new(),
            dry_run,
            model_proxy: None,
            usage_tracker: None,
        }
    }

    #[test]
    fn ensure_workspace_config_dir_creates_missing_bob_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        ensure_workspace_config_dir(&ctx).unwrap();

        assert!(tmp.path().join(WORKSPACE_CONFIG_DIR).is_dir());
    }

    #[test]
    fn ensure_workspace_config_dir_leaves_existing_bob_dir_untouched() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir(&bob_dir).unwrap();
        let existing = bob_dir.join("mcp.json");
        std::fs::write(&existing, "{}").unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        ensure_workspace_config_dir(&ctx).unwrap();

        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "{}");
    }

    #[test]
    fn ensure_workspace_config_dir_dry_run_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), true);

        ensure_workspace_config_dir(&ctx).unwrap();

        assert!(!tmp.path().join(WORKSPACE_CONFIG_DIR).exists());
    }
}
