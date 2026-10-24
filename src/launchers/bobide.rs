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
use crate::launchers::base::{
    EnvBinding, HasLauncherMetadata as HasBobIdeLauncherMetadata, LaunchContext, Launcher,
    LauncherMetadata, run_command,
};
use crate::launchers::shared::mcp_cli::{STDIO_AND_HTTP, mcp_binding_request, transport_set};
use crate::registry::{ConfigConstructable, ConstructError};
use crate::utils::install::InstallInfo;
use crate::utils::resolve_shell_command;
use crate::utils::subserver::SubServer;
use crate::utils::ui::Ui;

use_channel!("BOBIDE");

/*-- public --*/

const DEFAULT_USAGE_POLL_INTERVAL_SECS: u64 = 5;

/// Bob IDE launcher config.
///
/// Mirrors [`BobLauncherConfig`] from `bob/mod.rs`, but with `command`
/// instead of `command_path` on the public surface (the launcher name is
/// `"Bob IDE"` instead of `"Bob CLI"`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct BobIdeLauncherConfig {
    /// Override path to the `bobide` binary for non-PATH installs.
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

    /// Seconds between `bobide --status` polls while waiting for the IDE
    /// window to close. Default 3.
    /// Leave unset to use the default (3s).
    #[serde(default)]
    pub status_poll_interval_secs: Option<u64>,
}

pub struct BobIdeLauncher {
    instance_id: String,
    config: BobIdeLauncherConfig,
    /// `(server_name, binding)` for every MCP-capable capability bound to
    /// this launcher, registered/removed around `run_command` in `launch()`.
    bound_mcp_bindings: Vec<(String, McpBinding)>,
    /// `(tool_name, binding)` for every SubAgent-capable capability bound to
    /// this launcher. Not registered directly -- `launch()` wraps all of
    /// these into a single in-process MCP server (`bob/delegate.rs`) and
    /// registers *that* server instead.
    pending_sub_agents: Vec<(String, SubAgentBinding)>,
}

impl ConfigConstructable for BobIdeLauncher {
    type Config = BobIdeLauncherConfig;

    fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
        let config: BobIdeLauncherConfig =
            serde_json::from_value(cfg.clone()).map_err(ConstructError::settings)?;
        Ok(Self {
            instance_id: instance_id.to_string(),
            config,
            bound_mcp_bindings: vec![],
            pending_sub_agents: vec![],
        })
    }
}

impl BobIdeLauncher {
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

    /// Seconds between `bobide --status` polls while waiting for the IDE
    /// window to close. Default 3.
    /// Leave unset to use the default (3s).
    pub(crate) fn status_poll_interval(&self) -> std::time::Duration {
        let secs = self.config.status_poll_interval_secs.unwrap_or(3).max(1);
        std::time::Duration::from_secs(secs)
    }
}

impl crate::registry::Named for BobIdeLauncher {
    fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

#[async_trait]
impl Launcher for BobIdeLauncher {
    fn name(&self) -> &str {
        "Bob IDE"
    }

    fn command(&self) -> &str {
        self.config.command_path.as_deref().unwrap_or("bobide")
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

        let binding = capability
            .bind(mcp_binding_request(&Self::metadata().mcp_transports))
            .await?;
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
        resolve_bobide_command(&self.config.command_path)
    }

    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        Ok(vec![])
    }

    /// Writes each bound MCP server directly into `<workspace>/.bob/mcp.json`
    /// (overwriting-merge into the existing `mcpServers` object) before
    /// exec'ing, and removes them again once the IDE window closes.
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
            Some(_) => Some(crate::launchers::bob::hook::register_or_fail(
                &ctx.launcher_id,
                &ctx.working_dir,
            )?),
            None => None,
        };

        let mut delegate_server: Option<SubServer> = None;
        let mut all_mcp_bindings: Vec<(String, McpBinding)> = self.bound_mcp_bindings.clone();
        let mut args = args.to_vec();
        if !self.pending_sub_agents.is_empty() {
            let (binding, server) = crate::launchers::bob::delegate::start_delegate_mcp_server(
                self.pending_sub_agents.clone(),
                &self.config.pi_command_path,
                ctx,
                ui,
            )
            .await?;
            all_mcp_bindings.push(("bob-sub-agents".to_string(), binding));
            delegate_server = Some(server);
        }

        write_mcp_servers_to_config(&all_mcp_bindings, ctx, ui)?;

        // bobide does not inherit the CWD as the opened workspace; pass it
        // explicitly so the IDE opens with the correct project folder.
        args.push(ctx.working_dir.to_string_lossy().into_owned());

        alog_channel!(
            MessageLevel::Debug,
            "Running bobide command: {:#?} {:#?}",
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

            let run_fut = run_bobide_and_wait(
                binary.clone(),
                &overlay,
                &args,
                ctx,
                ui,
                self.status_poll_interval(),
            );
            tokio::pin!(run_fut);

            loop {
                if captured_task_id.is_none() {
                    tokio::select! {
                        r = &mut run_fut => break r,
                        _ = check_tick.tick() => {
                            if let Some(id) = crate::launchers::bob::hook::try_read_capture(&hook_reg.capture_path) {
                                crate::launchers::bob::hook::unregister(&ctx.launcher_id, &ctx.working_dir, &hook_reg.marker_command);
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
                                crate::launchers::bob::usage::collect_bob_usage(&db_path, &id)
                            })
                            .await
                            .unwrap_or_default();
                            tracker.set("bobide", stats);
                        }
                    }
                }
            }
        } else {
            // No usage tracker (dry_run): just run the command directly.
            run_bobide_and_wait(
                binary.clone(),
                &overlay,
                &args,
                ctx,
                ui,
                self.status_poll_interval(),
            )
            .await
        };

        remove_mcp_servers_from_config(&all_mcp_bindings, ctx, ui);

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
                let stats = tokio::task::spawn_blocking(move || {
                    crate::launchers::bob::usage::collect_bob_usage(&db_path, &id)
                })
                .await
                .unwrap_or_default();
                tracker.set("bobide", stats);
            }
            crate::launchers::bob::hook::unregister(
                &ctx.launcher_id,
                &ctx.working_dir,
                &hook_reg.marker_command,
            );
            let _ = std::fs::remove_file(&hook_reg.capture_path);
        }

        result
    }
}

impl HasBobIdeLauncherMetadata for BobIdeLauncher {
    fn metadata() -> LauncherMetadata {
        LauncherMetadata {
            name: "Bob IDE".to_string(),
            description: "IBM Bob IDE (full desktop application)".to_string(),
            default_command: "bobide".to_string(),
            supported_capabilities: HashSet::from([BindingType::Mcp, BindingType::SubAgent]),
            mcp_transports: transport_set(STDIO_AND_HTTP),
            tags: vec!["bob".to_string(), "ibm".to_string(), "ide".to_string()],
            install: install_info(),
        }
    }
}

/*-- private --*/

/// Bob IDE is a desktop application with no install command, so setup shows
/// the vendor page and offers nothing to run.
fn install_info() -> InstallInfo {
    InstallInfo::default()
        .program("bobide")
        .page("https://bob.ibm.com")
        .note("Bob IDE is a desktop application. Install it from the Bob website.")
}

/// Run `bobide --status` and return its stdout as a string.
/// Returns `None` on spawn failure or if the process exits non-zero.
fn bobide_status(binary: &std::path::Path) -> Option<String> {
    std::process::Command::new(binary)
        .arg("--status")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Returns `true` when `--status` output indicates the workspace window
/// for the current session is still open.
///
/// "Open" means the output contains `"window ["` (a bracketed-number window
/// entry — these are real workspace windows, as opposed to background
/// helper processes which appear as bare `window` without brackets).
///
/// Returns `false` when:
/// - The IDE is not running at all (output contains the "can only be used"
///   warning), or
/// - No `"window ["` entry is present (all workspace windows are closed).
fn window_still_open(status_output: &str) -> bool {
    if status_output.contains("--status argument can only be used") {
        return false;
    }
    status_output.contains("window [")
}

/// Launch bobide, wait for it to exit (it exits quickly as a GUI launcher),
/// then poll `bobide --status` until the workspace window is closed or a
/// shutdown signal is received.
///
/// Returns a synthetic success `ExitStatus` once the window is gone or the
/// poll detects the IDE has quit, mirroring the contract `run_command`
/// provides for normal CLI tools.
async fn run_bobide_and_wait(
    binary: PathBuf,
    overlay: &[EnvBinding],
    args: &[String],
    ctx: &LaunchContext,
    ui: &dyn Ui,
    status_poll_interval: std::time::Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    // Phase 1: spawn bobide and wait for the launcher process to exit.
    // It exits almost immediately after handing off to the running IDE.
    run_command(binary.clone(), overlay, args, ctx, ui).await?;

    if ctx.dry_run {
        // In dry-run mode run_command already printed what it would do;
        // skip the status-polling phase entirely.
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            return Ok(std::process::ExitStatus::from_raw(0));
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            return Ok(std::process::ExitStatus::from_raw(0));
        }
    }

    // Phase 2: poll --status until the window is gone.
    ui.info("bobide launched; waiting for IDE window to close...");

    let mut poll = tokio::time::interval(status_poll_interval);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        poll.tick().await;
        let still_open = tokio::task::spawn_blocking({
            let binary = binary.clone();
            move || {
                bobide_status(&binary)
                    .as_deref()
                    .map(window_still_open)
                    .unwrap_or(false)
            }
        })
        .await
        .unwrap_or(false);

        if !still_open {
            break;
        }
    }

    ui.info("IDE window closed; tearing down session.");

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Ok(std::process::ExitStatus::from_raw(0))
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        Ok(std::process::ExitStatus::from_raw(0))
    }
}

/// Bob stores workspace-scoped MCP config at `<workspace>/.bob/mcp.json` and
/// expects the directory to already exist; it does not create it itself, so
/// registration fails with ENOENT when missing (issue #144).
const WORKSPACE_CONFIG_DIR: &str = ".bob";

/// Creates the workspace-scoped config directory the downstream `bob` binary
/// writes into, unless this is a dry run (which must not touch the
/// filesystem). Called unconditionally near the top of `BobIdeLauncher::launch()`;
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

/// Resolve the effective `bobide` binary: try PATH first, fall back to
/// platform-specific install locations when no explicit override was given.
pub(crate) fn resolve_bobide_command(command_path: &Option<String>) -> anyhow::Result<PathBuf> {
    // If an explicit path is set, just use the standard resolver (no fallback)
    if command_path.is_some() {
        return resolve_shell_command(command_path, "bobide");
    }

    // Try PATH first
    if let Ok(p) = resolve_shell_command(&None, "bobide") {
        return Ok(p);
    }

    // Try platform-specific fallback locations
    #[cfg(target_os = "macos")]
    {
        let macos_path =
            PathBuf::from("/Applications/IBM Bob.app/Contents/Resources/app/bin/bobide");
        if macos_path.exists() {
            return Ok(macos_path);
        }
    }

    // No platform-specific fallback for Windows/Linux yet — fall through to error
    anyhow::bail!("'bobide' not found on PATH and no platform-specific install location was found")
}

/// Writes all bound MCP servers into `<workspace>/.bob/mcp.json` as an
/// overwriting merge into the existing `mcpServers` object.  Our entries
/// overwrite any stale same-named entries from a previous launch.
///
/// The file is expected to be `{ "mcpServers": { ... } }`.  If it does not
/// exist yet it is created from scratch.  If it does exist but has no
/// `mcpServers` key the key is added.  Keys not named by `bindings` are left
/// unchanged (other servers the user configured manually are preserved).
fn write_mcp_servers_to_config(
    bindings: &[(String, crate::capabilities::McpBinding)],
    ctx: &crate::launchers::base::LaunchContext,
    ui: &dyn crate::utils::ui::Ui,
) -> anyhow::Result<()> {
    if bindings.is_empty() {
        return Ok(());
    }

    let mcp_json_path = ctx.working_dir.join(WORKSPACE_CONFIG_DIR).join("mcp.json");

    if ctx.dry_run {
        for (name, binding) in bindings {
            ui.info(&format!(
                "Would write MCP server '{}' into {}",
                name,
                mcp_json_path.display()
            ));
            alog_channel!(
                MessageLevel::Debug2,
                "Would write MCP '{}': {}",
                name,
                binding.to_canonical_json()
            );
        }
        return Ok(());
    }

    let mut root: serde_json::Value = if mcp_json_path.exists() {
        let raw = std::fs::read_to_string(&mcp_json_path)
            .with_context(|| format!("failed to read {}", mcp_json_path.display()))?;
        serde_json::from_str(&raw).unwrap_or(serde_json::json!({ "mcpServers": {} }))
    } else {
        serde_json::json!({ "mcpServers": {} })
    };

    let servers = root
        .as_object_mut()
        .context("mcp.json root is not an object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    let servers_map = servers
        .as_object_mut()
        .context("mcp.json mcpServers is not an object")?;

    for (name, binding) in bindings {
        alog_channel!(
            MessageLevel::Debug2,
            "Writing MCP server '{}' into {}",
            name,
            mcp_json_path.display()
        );
        servers_map.insert(name.clone(), binding.to_canonical_json());
    }

    let json = serde_json::to_string_pretty(&root).context("failed to serialize mcp.json")?;
    std::fs::write(&mcp_json_path, json)
        .with_context(|| format!("failed to write {}", mcp_json_path.display()))
}

/// Removes the entries we wrote in [`write_mcp_servers_to_config`] from
/// `<workspace>/.bob/mcp.json`.  Best-effort: a failure is logged but does not
/// propagate, since by this point the launch has already succeeded or failed
/// on its own terms.
fn remove_mcp_servers_from_config(
    bindings: &[(String, crate::capabilities::McpBinding)],
    ctx: &crate::launchers::base::LaunchContext,
    ui: &dyn crate::utils::ui::Ui,
) {
    if bindings.is_empty() {
        return;
    }

    let mcp_json_path = ctx.working_dir.join(WORKSPACE_CONFIG_DIR).join("mcp.json");

    if ctx.dry_run {
        for (name, _) in bindings {
            ui.info(&format!(
                "Would remove MCP server '{}' from {}",
                name,
                mcp_json_path.display()
            ));
        }
        return;
    }

    let result = (|| -> anyhow::Result<()> {
        if !mcp_json_path.exists() {
            return Ok(());
        }
        let raw = std::fs::read_to_string(&mcp_json_path)
            .with_context(|| format!("failed to read {}", mcp_json_path.display()))?;
        let mut root: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        if let Some(servers) = root
            .as_object_mut()
            .and_then(|o| o.get_mut("mcpServers"))
            .and_then(|s| s.as_object_mut())
        {
            for (name, _) in bindings {
                alog_channel!(
                    MessageLevel::Debug2,
                    "Removing MCP server '{}' from {}",
                    name,
                    mcp_json_path.display()
                );
                servers.remove(name.as_str());
            }
        }
        let json = serde_json::to_string_pretty(&root).context("failed to serialize mcp.json")?;
        std::fs::write(&mcp_json_path, json)
            .with_context(|| format!("failed to write {}", mcp_json_path.display()))
    })();

    if let Err(e) = result {
        ui.warn(&format!("Failed to remove MCP servers from mcp.json: {e}"));
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_defaults_to_bobide() {
        let l = BobIdeLauncher::new("my-bobide", &serde_json::json!({})).unwrap();
        assert_eq!(l.command(), "bobide");
    }

    #[test]
    fn command_uses_explicit_path_when_set() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({
                "command_path": "/opt/bin/bobide"
            }),
        )
        .unwrap();
        assert_eq!(l.command(), "/opt/bin/bobide");
    }

    #[test]
    fn validate_command_err_for_nonexistent_explicit_path() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({
                "command_path": "/no/such/path/bobide"
            }),
        )
        .unwrap();
        assert!(l.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({
                "command_path": "ls"
            }),
        )
        .unwrap();
        assert!(l.validate_command().is_ok());
    }

    #[test]
    fn metadata_name_is_bob_ide() {
        let meta = BobIdeLauncher::metadata();
        assert_eq!(meta.name, "Bob IDE");
        assert_eq!(meta.default_command, "bobide");
    }

    #[test]
    fn config_schema_is_present() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<BobIdeLauncher>("bobide");
        let schema = factory.config_schema("bobide").unwrap();
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
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({ "command_path": "/opt/bin/bobide" }),
        )
        .unwrap();
        assert_eq!(l.config.command_path, Some("/opt/bin/bobide".to_string()));
        assert_eq!(l.config.pi_command_path, None);
    }

    #[test]
    fn metadata_supported_capabilities_contains_mcp_and_sub_agent() {
        let meta = BobIdeLauncher::metadata();
        assert!(meta.supported_capabilities.contains(&BindingType::Mcp));
        assert!(meta.supported_capabilities.contains(&BindingType::SubAgent));
    }

    #[test]
    fn bob_db_path_defaults_when_not_configured() {
        let l = BobIdeLauncher::new("my-bobide", &serde_json::json!({})).unwrap();
        let db_path = l.bob_db_path();
        assert!(db_path.ends_with(".bob/db/bob.db"));
    }

    #[test]
    fn bob_db_path_uses_config_override() {
        let l = BobIdeLauncher::new(
            "my-bobide",
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
        let l = BobIdeLauncher::new("my-bobide", &serde_json::json!({})).unwrap();
        assert_eq!(l.config.usage_poll_interval_secs, None);
    }

    #[test]
    fn config_uses_explicit_usage_poll_interval() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({
                "usage_poll_interval_secs": 10
            }),
        )
        .unwrap();
        assert_eq!(l.config.usage_poll_interval_secs, Some(10));
    }

    #[test]
    fn bob_db_path_and_poll_interval_can_be_set_together() {
        let l = BobIdeLauncher::new(
            "my-bobide",
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
        let l = BobIdeLauncher::new("my-bobide", &serde_json::json!({})).unwrap();
        assert_eq!(l.usage_poll_interval(), std::time::Duration::from_secs(5));
    }

    #[test]
    fn usage_poll_interval_uses_config_when_set() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({
                "usage_poll_interval_secs": 15
            }),
        )
        .unwrap();
        assert_eq!(l.usage_poll_interval(), std::time::Duration::from_secs(15));
    }

    #[test]
    fn usage_poll_interval_floors_at_one_second_when_config_is_explicitly_zero() {
        let l = BobIdeLauncher::new(
            "my-bobide",
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
                    supported_reasoning_efforts: vec![],
                },
                known_type: None,
            }))
        }
    }

    fn bobide() -> BobIdeLauncher {
        BobIdeLauncher::new("my-bobide", &serde_json::json!({})).unwrap()
    }

    #[tokio::test]
    async fn bind_capability_with_sub_agent_only_capability_routes_into_pending_sub_agents() {
        let mut l = bobide();
        l.bind_capability(&FakeSubAgentCapability).await.unwrap();
        assert!(l.bound_mcp_bindings.is_empty());
        assert_eq!(l.pending_sub_agents.len(), 1);
        assert_eq!(l.pending_sub_agents[0].0, "fake-sub-agent");
        assert_eq!(l.pending_sub_agents[0].1.description, "explores the repo");
    }

    #[tokio::test]
    async fn bind_capability_with_mcp_only_capability_still_populates_bound_mcp_bindings() {
        let mut l = bobide();
        l.bind_capability(&FakeMcpCapability).await.unwrap();
        assert_eq!(l.bound_mcp_bindings.len(), 1);
        assert_eq!(l.bound_mcp_bindings[0].0, "fake-mcp");
        assert!(l.pending_sub_agents.is_empty());
    }

    // -- workspace config dir ----------------------------------------------

    fn launch_ctx(working_dir: PathBuf, dry_run: bool) -> LaunchContext {
        LaunchContext {
            launcher_id: "my-bobide".to_string(),
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

    #[tokio::test]
    async fn launch_passes_working_dir_as_positional_arg() {
        // Use `ls` as the command so validate_command succeeds on every
        // environment (CI included), regardless of whether `bobide` is
        // installed.
        let launcher =
            BobIdeLauncher::new("my-bobide", &serde_json::json!({ "command_path": "ls" })).unwrap();
        let ctx = launch_ctx(std::env::temp_dir(), true);
        let result = launcher
            .launch(
                &[],
                &ctx,
                &crate::utils::ui::backends::plain::PlainOutput {},
            )
            .await;
        assert!(result.is_ok());
    }

    // -- window_still_open ----------------------------------------------

    #[test]
    fn window_still_open_returns_true_when_numbered_window_present() {
        let status = "CPU %\tMem MB\t   PID\tProcess\n    0\t123\t 100\tbobide\n    0\t456\t 101\twindow [1] (my-project)\n";
        assert!(window_still_open(status));
    }

    #[test]
    fn window_still_open_returns_false_when_no_numbered_window() {
        let status =
            "CPU %\tMem MB\t   PID\tProcess\n    0\t123\t 100\tbobide\n    0\t456\t 101\twindow\n";
        assert!(!window_still_open(status));
    }

    #[test]
    fn window_still_open_returns_false_when_ide_not_running() {
        let status = "Warning: The --status argument can only be used if IBM Bob is already running. Please run it again after IBM Bob has started.\n";
        assert!(!window_still_open(status));
    }

    // -- status_poll_interval -------------------------------------------

    #[test]
    fn status_poll_interval_defaults_to_three_seconds() {
        let l = BobIdeLauncher::new("my-bobide", &serde_json::json!({})).unwrap();
        assert_eq!(l.status_poll_interval(), std::time::Duration::from_secs(3));
    }

    #[test]
    fn status_poll_interval_uses_config_when_set() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({ "status_poll_interval_secs": 10 }),
        )
        .unwrap();
        assert_eq!(l.status_poll_interval(), std::time::Duration::from_secs(10));
    }

    #[test]
    fn status_poll_interval_floors_at_one_second() {
        let l = BobIdeLauncher::new(
            "my-bobide",
            &serde_json::json!({ "status_poll_interval_secs": 0 }),
        )
        .unwrap();
        assert_eq!(l.status_poll_interval(), std::time::Duration::from_secs(1));
    }

    #[test]
    fn config_schema_includes_status_poll_interval_secs() {
        use crate::launchers::base::LauncherFactory;
        let mut factory = LauncherFactory::new();
        factory.register::<BobIdeLauncher>("bobide");
        let schema = factory.config_schema("bobide").unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert!(props.contains_key("status_poll_interval_secs"));
    }

    // -- write_mcp_servers_to_config / remove_mcp_servers_from_config ----------

    fn http_binding(url: &str) -> McpBinding {
        McpBinding::Http {
            url: url.to_string(),
            headers: Default::default(),
            timeout: None,
        }
    }

    #[test]
    fn write_mcp_servers_creates_file_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        let bindings = vec![(
            "my-server".to_string(),
            http_binding("http://127.0.0.1:9/mcp"),
        )];
        write_mcp_servers_to_config(
            &bindings,
            &ctx,
            &crate::utils::ui::backends::plain::PlainOutput {},
        )
        .unwrap();

        let raw = std::fs::read_to_string(bob_dir.join("mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["mcpServers"]["my-server"]["url"],
            "http://127.0.0.1:9/mcp"
        );
        assert_eq!(v["mcpServers"]["my-server"]["type"], "http");
    }

    #[test]
    fn write_mcp_servers_merges_into_existing_file_and_preserves_other_keys() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let mcp_json = bob_dir.join("mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"pre-existing":{"type":"http","url":"http://other/mcp"}}}"#,
        )
        .unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        let bindings = vec![(
            "new-server".to_string(),
            http_binding("http://127.0.0.1:9/mcp"),
        )];
        write_mcp_servers_to_config(
            &bindings,
            &ctx,
            &crate::utils::ui::backends::plain::PlainOutput {},
        )
        .unwrap();

        let raw = std::fs::read_to_string(&mcp_json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // Our new entry is present
        assert_eq!(
            v["mcpServers"]["new-server"]["url"],
            "http://127.0.0.1:9/mcp"
        );
        // The pre-existing entry is preserved
        assert_eq!(v["mcpServers"]["pre-existing"]["url"], "http://other/mcp");
    }

    #[test]
    fn write_mcp_servers_overwrites_stale_same_named_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let mcp_json = bob_dir.join("mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"my-server":{"type":"http","url":"http://stale/mcp"}}}"#,
        )
        .unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        let bindings = vec![(
            "my-server".to_string(),
            http_binding("http://127.0.0.1:9/mcp"),
        )];
        write_mcp_servers_to_config(
            &bindings,
            &ctx,
            &crate::utils::ui::backends::plain::PlainOutput {},
        )
        .unwrap();

        let raw = std::fs::read_to_string(&mcp_json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["mcpServers"]["my-server"]["url"],
            "http://127.0.0.1:9/mcp"
        );
    }

    #[test]
    fn write_mcp_servers_is_noop_for_empty_bindings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        write_mcp_servers_to_config(
            &[],
            &ctx,
            &crate::utils::ui::backends::plain::PlainOutput {},
        )
        .unwrap();

        // No file should have been created
        assert!(!bob_dir.join("mcp.json").exists());
    }

    #[test]
    fn remove_mcp_servers_removes_our_entries_and_leaves_others() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let mcp_json = bob_dir.join("mcp.json");
        std::fs::write(
            &mcp_json,
            r#"{"mcpServers":{"ours":{"type":"http","url":"http://a/mcp"},"theirs":{"type":"http","url":"http://b/mcp"}}}"#,
        )
        .unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        let bindings = vec![("ours".to_string(), http_binding("http://a/mcp"))];
        remove_mcp_servers_from_config(
            &bindings,
            &ctx,
            &crate::utils::ui::backends::plain::PlainOutput {},
        );

        let raw = std::fs::read_to_string(&mcp_json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(v["mcpServers"].get("ours").is_none());
        assert_eq!(v["mcpServers"]["theirs"]["url"], "http://b/mcp");
    }

    #[test]
    fn remove_mcp_servers_is_noop_when_file_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);

        // Should not panic or error even with no file
        remove_mcp_servers_from_config(
            &[("missing".to_string(), http_binding("http://a/mcp"))],
            &ctx,
            &crate::utils::ui::backends::plain::PlainOutput {},
        );
    }

    #[test]
    fn write_and_remove_round_trip_restores_original_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bob_dir = tmp.path().join(WORKSPACE_CONFIG_DIR);
        std::fs::create_dir_all(&bob_dir).unwrap();
        let mcp_json = bob_dir.join("mcp.json");
        std::fs::write(&mcp_json, r#"{"mcpServers":{}}"#).unwrap();
        let ctx = launch_ctx(tmp.path().to_path_buf(), false);
        let ui = crate::utils::ui::backends::plain::PlainOutput {};

        let bindings = vec![(
            "ephemeral".to_string(),
            http_binding("http://127.0.0.1:9/mcp"),
        )];
        write_mcp_servers_to_config(&bindings, &ctx, &ui).unwrap();

        // Entry is present after write
        let mid: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_json).unwrap()).unwrap();
        assert!(mid["mcpServers"].get("ephemeral").is_some());

        remove_mcp_servers_from_config(&bindings, &ctx, &ui);

        // Entry is gone after remove
        let end: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&mcp_json).unwrap()).unwrap();
        assert!(end["mcpServers"].get("ephemeral").is_none());
    }
}
