// Standard
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

// Third Party
use alog::{MessageLevel, alog_channel, use_channel};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::capabilities::{BindingType, ToolName};
use crate::define_factory;
use crate::registry::ConfigConstructable;
use crate::utils::Searchable;
use crate::utils::ui::Ui;

use_channel!("LNCHR");

/*-- public --*/

/// Core trait for launcher implementations.
/// All launchers must implement this trait along with ConfigConstructable.
#[async_trait]
pub trait Launcher: crate::registry::Named + Send + Sync {
    fn name(&self) -> &str;

    /// The binary/command this instance will exec.
    /// Returns the full command string — either a bare binary name for PATH
    /// lookup (e.g. `"claude"`) or an absolute path set by the user in config.
    fn command(&self) -> &str;

    /// Bind a capability to this launcher instance.
    ///
    /// The implementation should validate that the capability's `binding_types()`
    /// are supported by this launcher type (per `metadata().supported_capabilities`),
    /// construct the appropriate `BindingRequest`, call `bind()`, and store the
    /// resolved `Binding` for use in `env_overlay` / `launch`.
    async fn bind_capability(
        &mut self,
        capability: &dyn crate::capabilities::ResolvedCapability,
    ) -> anyhow::Result<()>;

    /// Resolve the binary to an absolute path.
    ///
    /// Checks the optional `command_path` override baked into the instance at
    /// construction time first, then falls back to a PATH lookup.
    ///
    /// **Not called during construction** — callers invoke this explicitly so
    /// that `catalog` and `list` work even when the tool is not installed.
    fn validate_command(&self) -> anyhow::Result<PathBuf>;

    /// Build the environment variable overlay for this launch.
    ///
    /// The default implementation returns an empty vec; concrete launchers
    /// override this once capability hooks are wired up.
    async fn env_overlay(&self, _ctx: &LaunchContext) -> anyhow::Result<Vec<EnvBinding>> {
        Ok(vec![])
    }

    /// Maps a canonical `ToolName` to this launcher's own native tool-name
    /// string, if it has an equivalent. Only meaningful for launchers
    /// implementing `BindingType::SubAgent`; the default handles the escape
    /// hatch (`ToolName::Other`, passed through verbatim) and returns `None`
    /// for every canonical/MCP variant, mirroring `env_overlay`'s "no-op
    /// until you actually support the feature" default -- a launcher that
    /// hasn't implemented sub-agent support needs no changes.
    fn map_tool_name(&self, tool: &ToolName) -> Option<String> {
        match tool {
            ToolName::Other(raw) => Some(raw.clone()),
            _ => None,
        }
    }

    /// Exec the tool as a subprocess with the env overlay applied.
    ///
    /// The default implementation resolves the binary and env overlay, then
    /// delegates to `run_command` so that dry_run and execution share
    /// an identical code path.  Concrete launchers override only when they need
    /// non-standard behaviour (e.g. a TUI-based launcher that doesn't spawn a
    /// subprocess at all).
    async fn launch(
        &self,
        args: &[String],
        ctx: &LaunchContext,
        ui: &dyn Ui,
    ) -> anyhow::Result<std::process::ExitStatus> {
        let binary = self.validate_command()?;
        let overlay = self.env_overlay(ctx).await?;
        alog_channel!(MessageLevel::Debug2, "Env Overlay: {:#?}", overlay);
        run_command(binary, &overlay, args, ctx, ui).await
    }
}

/// Translate WSL paths to Windows paths in env var values.
///
/// When running a native Windows binary from WSL (e.g., `opencode.exe`),
/// env vars like `OPENCODE_CONFIG` may contain WSL paths (`/mnt/c/Users/...`)
/// that the PE binary cannot understand. Convert them to Windows format.
fn translate_env_for_windows(binary: &PathBuf, env: &[EnvBinding]) -> Vec<EnvBinding> {
    // Only translate when the target is a Windows PE binary (ends in .exe)
    alog_channel!(MessageLevel::Debug2, "Translating for binary {:#?}", binary);
    if !binary.to_string_lossy().to_lowercase().ends_with(".exe") {
        return env.to_vec();
    }
    let mut out: Vec<_> = env
        .iter()
        .map(|b| {
            let value = crate::config::translate_wsl_to_windows(&b.value)
                .unwrap_or_else(|| b.value.clone());
            EnvBinding {
                key: b.key.clone(),
                value,
            }
        })
        .collect();
    // In order for WSL env vars to be seen by Windows, they need to be added
    // to the special WSLENV variable
    let wslenv = out
        .iter()
        .map(|b| b.key.clone())
        .collect::<Vec<_>>()
        .join(":");
    out.push(EnvBinding {
        key: "WSLENV".to_string(),
        value: wslenv,
    });
    alog_channel!(MessageLevel::Debug3, "Translated env vars: {:#?}", out);
    out
}

/// Resolve a command and run it, handling dry_run and exit status.
///
/// This is the shared utility that both the default `Launcher::launch` and
/// any custom launcher implementations should use when spawning a subprocess.
/// If `ctx.dry_run` is true the resolved binary, args, and env overlay are
/// printed to the UI without executing. Otherwise the command is spawned as a
/// subprocess with the overlay applied and a non-success exit status is turned
/// into an error.
pub(crate) async fn run_command(
    binary: PathBuf,
    overlay: &[EnvBinding],
    args: &[String],
    ctx: &LaunchContext,
    ui: &dyn Ui,
) -> anyhow::Result<std::process::ExitStatus> {
    if ctx.dry_run {
        ui.info(&format!("Would exec: {}", binary.display()));
        ui.info(&format!(
            "  args: {}",
            if args.is_empty() {
                "(none)".to_string()
            } else {
                args.join(" ")
            }
        ));
        if overlay.is_empty() {
            ui.info("  env overlay: (none)");
        } else {
            for binding in overlay {
                ui.info(&format!("  env: {}={}", binding.key, binding.value));
            }
        }
        // Return a dummy success status so callers don't need to special-case
        // dry_run.  The exit status is meaningless in this mode anyway.
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

    // Translate WSL paths in env vars when spawning a native Windows binary.
    let translated_overlay = translate_env_for_windows(&binary, overlay);

    let mut cmd = std::process::Command::new(&binary);
    cmd.args(args);
    for binding in &translated_overlay {
        cmd.env(&binding.key, &binding.value);
    }

    // Spawn and wait on a blocking thread: `Child::wait` blocks the calling
    // thread until the subprocess exits, which would otherwise starve the
    // async runtime -- notably the usage-tracking proxy server, which needs
    // scheduler time concurrently with the child running.
    //
    // On Windows, some PE binaries (notably from official release builds run
    // in WSL) fail with os error 193 ("%1 is not a valid Win32 application")
    // when spawned directly. In those cases, fall back to invoking through
    // `cmd.exe /C`, which handles PE format compatibility correctly.
    let working_dir = ctx.working_dir.clone();

    #[cfg(windows)]
    let (binary_fallback, args_fallback, overlay_fallback) =
        (binary.clone(), args.to_vec(), translated_overlay);

    tokio::task::spawn_blocking(move || -> anyhow::Result<std::process::ExitStatus> {
        let mut spawn_cmd = cmd;

        // Always set the subprocess CWD to the caller's working directory so
        // the launched tool starts in the right place regardless of platform.
        spawn_cmd.current_dir(&working_dir);

        // In WSL, the parent's CWD is a WSL path that Windows child processes
        // cannot navigate to (it becomes an unusable UNC path). Override with
        // the drive root so the PE binary at least starts somewhere valid.
        // This must only fire under WSL -- on native Windows the working_dir
        // set above is already a proper Windows path and must not be replaced.
        #[cfg(windows)]
        if crate::config::Config::is_wsl() {
            if let Some(drive) = binary_fallback.parent().and_then(|p| {
                p.to_str().and_then(|s| {
                    let chars: Vec<char> = s.chars().collect();
                    if chars.len() >= 2 && chars[1] == ':' {
                        Some(format!("{}\\", &s[..2]))
                    } else {
                        None
                    }
                })
            }) {
                spawn_cmd.current_dir(drive);
            }
        }

        let mut child = match spawn_cmd.spawn() {
            Ok(child) => child,
            #[allow(unreachable_code)]
            Err(e) => {
                #[cfg(windows)]
                if let Some(193) = e.raw_os_error() {
                    let mut shell = std::process::Command::new("cmd.exe");
                    shell.arg("/C");
                    // Build the full command string for cmd.exe
                    let mut cmd_str = String::new();
                    cmd_str.push_str(&binary_fallback.to_string_lossy());
                    for arg in &args_fallback {
                        cmd_str.push(' ');
                        cmd_str.push_str(arg);
                    }
                    shell.arg(&cmd_str);
                    for binding in &overlay_fallback {
                        shell.env(&binding.key, &binding.value);
                    }
                    return shell.spawn()?.wait().map_err(anyhow::Error::from);
                }
                return Err(e.into());
            }
        };

        Ok(child.wait()?)
    })
    .await?
}

/// Resolve a command and run it, capturing stdout/stderr into returned strings.
///
/// Mirrors `run_command`'s structure but captures output instead of inheriting
/// stdio. Used for machine-consumed subprocess calls (e.g. a headless `pi
/// --print`) where the caller needs the subprocess's output as a string rather
/// than watching it in a terminal.
///
/// Returns `(exit_status, stdout, stderr)`. A non-success exit status is
/// returned as `Ok` -- the caller decides what a bad code means.
///
/// Only reached from [`run_command_captured_killable`]'s Windows branch, which
/// needs this path's WSL/PE-compatibility fallbacks.
#[cfg(windows)]
pub(crate) async fn run_command_captured(
    binary: PathBuf,
    overlay: &[EnvBinding],
    args: &[String],
    ctx: &LaunchContext,
) -> anyhow::Result<(std::process::ExitStatus, String, String)> {
    if ctx.dry_run {
        return Ok((dry_run_success_status(), String::new(), String::new()));
    }

    // Translate WSL paths in env vars when spawning a native Windows binary.
    let translated_overlay = translate_env_for_windows(&binary, overlay);

    let mut cmd = std::process::Command::new(&binary);
    cmd.args(args);
    for binding in &translated_overlay {
        cmd.env(&binding.key, &binding.value);
    }

    // Spawn and wait on a blocking thread: `Child::wait` blocks the calling
    // thread until the subprocess exits, which would otherwise starve the
    // async runtime -- notably the usage-tracking proxy server, which needs
    // scheduler time concurrently with the child running.
    //
    // On Windows, some PE binaries (notably from official release builds run
    // in WSL) fail with os error 193 ("%1 is not a valid Win32 application")
    // when spawned directly. In those cases, fall back to invoking through
    // `cmd.exe /C`, which handles PE format compatibility correctly.
    let working_dir = ctx.working_dir.clone();

    #[cfg(windows)]
    let (binary_fallback, args_fallback, overlay_fallback) =
        (binary.clone(), args.to_vec(), translated_overlay);

    tokio::task::spawn_blocking(
        move || -> anyhow::Result<(std::process::ExitStatus, String, String)> {
            let mut spawn_cmd = cmd;

            // Always set the subprocess CWD to the caller's working directory so
            // the launched tool starts in the right place regardless of platform.
            spawn_cmd.current_dir(&working_dir);

            // In WSL, the parent's CWD is a WSL path that Windows child processes
            // cannot navigate to (it becomes an unusable UNC path). Override with
            // the drive root so the PE binary at least starts somewhere valid.
            // This must only fire under WSL -- on native Windows the working_dir
            // set above is already a proper Windows path and must not be replaced.
            #[cfg(windows)]
            if crate::config::Config::is_wsl() {
                if let Some(drive) = binary_fallback.parent().and_then(|p| {
                    p.to_str().and_then(|s| {
                        let chars: Vec<char> = s.chars().collect();
                        if chars.len() >= 2 && chars[1] == ':' {
                            Some(format!("{}\\", &s[..2]))
                        } else {
                            None
                        }
                    })
                }) {
                    spawn_cmd.current_dir(drive);
                }
            }

            let output = match spawn_cmd.output() {
                Ok(output) => output,
                #[allow(unreachable_code)]
                Err(e) => {
                    #[cfg(windows)]
                    if let Some(193) = e.raw_os_error() {
                        let mut shell = std::process::Command::new("cmd.exe");
                        shell.arg("/C");
                        // Build the full command string for cmd.exe
                        let mut cmd_str = String::new();
                        cmd_str.push_str(&binary_fallback.to_string_lossy());
                        for arg in &args_fallback {
                            cmd_str.push(' ');
                            cmd_str.push_str(arg);
                        }
                        shell.arg(&cmd_str);
                        for binding in &overlay_fallback {
                            shell.env(&binding.key, &binding.value);
                        }
                        return shell
                            .output()
                            .map(|o| {
                                (
                                    o.status,
                                    String::from_utf8_lossy(&o.stdout).into_owned(),
                                    String::from_utf8_lossy(&o.stderr).into_owned(),
                                )
                            })
                            .map_err(anyhow::Error::from);
                    }
                    return Err(e.into());
                }
            };

            Ok((
                output.status,
                String::from_utf8_lossy(&output.stdout).into_owned(),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        },
    )
    .await?
}

/// Like [`run_command_captured`], but the child is killed if the returned
/// future is dropped before it completes -- which is how a cancelled
/// delegated sub-agent stops burning the model behind it. On Unix the child
/// leads its own process group and the whole group is killed, so anything it
/// spawned (e.g. `pi`'s `bash` tool) goes with it.
///
/// On Windows this defers to [`run_command_captured`], whose
/// WSL/PE-compatibility fallbacks this path does not replicate; there, dropping
/// the future does not stop the child.
pub(crate) async fn run_command_captured_killable(
    binary: PathBuf,
    overlay: &[EnvBinding],
    args: &[String],
    ctx: &LaunchContext,
) -> anyhow::Result<(std::process::ExitStatus, String, String)> {
    if ctx.dry_run {
        return Ok((dry_run_success_status(), String::new(), String::new()));
    }

    #[cfg(windows)]
    return run_command_captured(binary, overlay, args, ctx).await;

    #[cfg(not(windows))]
    {
        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(args)
            .current_dir(&ctx.working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for binding in overlay {
            cmd.env(&binding.key, &binding.value);
        }

        let child = cmd.spawn()?;
        let mut group = ProcessGroupKillGuard(child.id());
        let output = child.wait_with_output().await?;
        // The child has exited and been reaped; its pid may be reused, so the
        // guard must not signal it any more.
        group.0 = None;
        Ok((
            output.status,
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// Kills the process group led by the held pid when dropped while still armed.
/// `kill_on_drop` only signals the direct child; this reaches its descendants.
#[cfg(not(windows))]
struct ProcessGroupKillGuard(Option<u32>);

#[cfg(not(windows))]
impl Drop for ProcessGroupKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("kill")
                .args(["-s", "KILL", "--", &format!("-{pid}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

/// A successful exit status with no output, returned by the captured runners
/// in dry-run mode so callers don't need to special-case it.
fn dry_run_success_status() -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }
}

/// Metadata describing a launcher implementation (type-level, catalog entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherMetadata {
    pub name: String,
    pub description: String,
    /// The default binary name used for PATH lookup (e.g. `"claude"`, `"bob"`).
    pub default_command: String,
    /// Binding surfaces this launcher type can make use of.
    pub supported_capabilities: HashSet<BindingType>,
    /// MCP transports this launcher can register with its downstream tool.
    /// Empty for launchers that do not support `BindingType::Mcp`. Its
    /// `bind_capability` builds the MCP binding request from this field, and
    /// enable-time validation reads it, so the two cannot disagree.
    pub mcp_transports: HashSet<crate::capabilities::McpTransportKind>,
    pub tags: Vec<String>,
    /// How to install this type's tool. Its `program` is `default_command`.
    pub install: crate::utils::install::InstallInfo,
}

impl std::fmt::Display for LauncherMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description)
    }
}

impl Searchable for LauncherMetadata {
    fn search_fields(&self) -> Vec<&str> {
        let mut fields: Vec<&str> = vec![
            self.name.as_str(),
            self.description.as_str(),
            self.default_command.as_str(),
        ];
        fields.extend(self.tags.iter().map(String::as_str));
        fields
    }
}

/// Runtime context passed through the launch lifecycle.
pub struct LaunchContext {
    pub launcher_id: String,
    /// The working directory for the spawned subprocess (i.e. the CWD the
    /// tool process will see). Typically set to the user's current directory.
    pub working_dir: PathBuf,
    /// Env vars already resolved (e.g. provider URL, model ID) before any
    /// capability bindings are merged on top.
    pub base_env: HashMap<String, String>,
    /// If true, only display what would be launched without executing.
    pub dry_run: bool,
    /// Shared usage tracker for the session's proxy, if one is running. Lets a
    /// launcher submit self-reported usage that never passes through the proxy
    /// (e.g. Bob, which has no model-configuration capability and so never makes
    /// a request the proxy could intercept).
    pub usage_tracker: Option<std::sync::Arc<crate::proxy::UsageTracker>>,
    /// The session proxy for this launch, when one was started.
    pub model_proxy: Option<crate::proxy::ProxyHandle>,
}

/// A single environment variable binding contributed to the subprocess overlay.
#[derive(Debug, Clone)]
pub struct EnvBinding {
    pub key: String,
    pub value: String,
}

/*-- resolve, then bind -------------------------------------------------------*/

/// Why a launcher of this type cannot carry `capability`'s MCP server, or
/// `None` when it can (or when the capability does not fix a transport, or
/// the launcher takes no MCP bindings at all, which binding types already
/// cover). Both enable-time listing and config validation call this, and
/// `bind_capability` builds its request from the same `mcp_transports`, so
/// what is accepted here is what binds.
pub(crate) fn mcp_transport_conflict<C>(
    launcher: &LauncherMetadata,
    capability: &C,
) -> Option<McpTransportConflict>
where
    C: crate::capabilities::CapabilityInfo + ?Sized,
{
    if !launcher
        .supported_capabilities
        .contains(&crate::capabilities::BindingType::Mcp)
    {
        return None;
    }
    let transport = capability.mcp_transport()?;
    if launcher.mcp_transports.contains(&transport) {
        return None;
    }
    let mut offered: Vec<String> = launcher
        .mcp_transports
        .iter()
        .map(|t| format!("{t:?}"))
        .collect();
    offered.sort();
    Some(McpTransportConflict {
        transport: format!("{transport:?}"),
        offered,
    })
}

/// A capability's MCP transport that a launcher type cannot register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpTransportConflict {
    pub(crate) transport: String,
    /// What the launcher can register, sorted.
    pub(crate) offered: Vec<String>,
}

/// Resolves every capability a launcher enables, and only then binds them.
///
/// The first pass asks the capability source for each id in turn, so a
/// capability that cannot be built, or whose model does not resolve or does
/// not meet what it requires, stops the launch with that capability named and
/// nothing bound. The second pass runs `on_setup` and `bind_capability` over
/// what resolved, in the order the launcher lists them, which is where a
/// launch starts changing the launcher.
///
/// The resolved capabilities are returned so the caller can keep them alive:
/// one that owns a process-scoped resource, such as `vision-mcp`'s in-process
/// MCP server, has to outlive the launched process so `on_shutdown` tears it
/// down after it exits.
pub(crate) async fn resolve_and_bind(
    launcher: &mut dyn Launcher,
    enabled_capabilities: &[String],
    capabilities: &crate::capabilities::CapabilitySource,
) -> anyhow::Result<Vec<std::sync::Arc<dyn crate::capabilities::ResolvedCapability>>> {
    let mut resolved = Vec::with_capacity(enabled_capabilities.len());
    for capability_id in enabled_capabilities {
        resolved.push(capabilities.get(capability_id)?);
    }

    for capability in &resolved {
        capability.on_setup().await?;
        launcher.bind_capability(capability.as_ref()).await?;
    }

    Ok(resolved)
}

/*-- private --*/

define_factory!(Launcher, LauncherMetadata, LauncherFactory);

/*-- tests --*/

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::registry::{ConfigConstructable, ConstructError};

    /// Minimal Launcher implementation used only in tests.
    pub(crate) struct FakeLauncher {
        instance_id: String,
        /// When `Some`, `validate_command` resolves to this path directly.
        command_path: Option<PathBuf>,
        command_name: String,
        /// The capability ids `bind_capability` was called with, in order.
        pub(crate) bound: std::sync::Mutex<Vec<String>>,
    }

    impl crate::registry::Named for FakeLauncher {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    impl ConfigConstructable for FakeLauncher {
        type Config = crate::registry::NoConfig;

        fn new(instance_id: &str, cfg: &serde_json::Value) -> Result<Self, ConstructError> {
            let command_name = cfg
                .get("command_name")
                .and_then(|v| v.as_str())
                .unwrap_or("fake-binary-that-does-not-exist")
                .to_string();
            let command_path = cfg
                .get("command_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Ok(Self {
                instance_id: instance_id.to_string(),
                command_name,
                command_path,
                bound: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Launcher for FakeLauncher {
        fn name(&self) -> &str {
            "Fake Launcher"
        }

        fn command(&self) -> &str {
            &self.command_name
        }

        async fn bind_capability(
            &mut self,
            capability: &dyn crate::capabilities::ResolvedCapability,
        ) -> anyhow::Result<()> {
            self.bound
                .lock()
                .unwrap()
                .push(capability.instance_id().to_string());
            Ok(())
        }

        fn validate_command(&self) -> anyhow::Result<PathBuf> {
            crate::utils::resolve_shell_command(
                &self
                    .command_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                &self.command_name,
            )
        }
    }

    impl HasLauncherMetadata for FakeLauncher {
        fn metadata() -> LauncherMetadata {
            LauncherMetadata {
                name: "Fake Launcher".to_string(),
                description: "Test double".to_string(),
                default_command: "fake-binary-that-does-not-exist".to_string(),
                supported_capabilities: HashSet::new(),
                mcp_transports: HashSet::new(),
                tags: vec![],
                install: Default::default(),
            }
        }
    }

    #[test]
    fn validate_command_returns_err_for_unknown_binary() {
        let launcher = FakeLauncher::new(
            "my-fake",
            &serde_json::json!({
                "command_name": "this-binary-absolutely-does-not-exist-9x7z"
            }),
        )
        .unwrap();
        assert!(launcher.validate_command().is_err());
    }

    #[test]
    fn validate_command_returns_err_for_nonexistent_explicit_path() {
        let launcher = FakeLauncher::new(
            "my-fake",
            &serde_json::json!({
                "command_name": "fake",
                "command_path": "/this/path/does/not/exist/fake"
            }),
        )
        .unwrap();
        assert!(launcher.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let launcher = FakeLauncher::new(
            "my-fake",
            &serde_json::json!({
                "command_path": "ls"
            }),
        )
        .unwrap();
        assert!(launcher.validate_command().is_ok());
    }

    #[tokio::test]
    async fn env_overlay_default_is_empty() {
        let launcher = FakeLauncher::new("my-fake", &serde_json::json!({})).unwrap();
        let ctx = LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: HashMap::new(),
            dry_run: false,
            usage_tracker: None,
            model_proxy: None,
        };
        let overlay = launcher.env_overlay(&ctx).await.unwrap();
        assert!(overlay.is_empty());
    }

    #[test]
    fn map_tool_name_default_passes_through_other_and_returns_none_for_everything_else() {
        let launcher = FakeLauncher::new("my-fake", &serde_json::json!({})).unwrap();
        assert_eq!(
            launcher.map_tool_name(&ToolName::Other("SomeRawTool".to_string())),
            Some("SomeRawTool".to_string())
        );
        assert_eq!(launcher.map_tool_name(&ToolName::FileRead), None);
        assert_eq!(
            launcher.map_tool_name(&ToolName::Mcp {
                server: "vision".to_string(),
                tool: None,
            }),
            None
        );
    }

    #[test]
    fn launcher_factory_register_and_get() {
        let mut factory = LauncherFactory::new();
        factory.register::<FakeLauncher>("fake");
        assert!(factory.get("fake").is_some());
        assert!(factory.get("nonexistent").is_none());
    }

    #[test]
    fn launcher_factory_construct() {
        let mut factory = LauncherFactory::new();
        factory.register::<FakeLauncher>("fake");
        let result = factory.construct("fake", "my-fake", &serde_json::json!({}));
        assert!(result.is_ok());
    }

    #[test]
    fn launcher_metadata_display() {
        let meta = LauncherMetadata {
            name: "Test".to_string(),
            description: "A test launcher".to_string(),
            default_command: "test".to_string(),
            supported_capabilities: HashSet::new(),
            mcp_transports: HashSet::new(),
            tags: vec![],
            install: Default::default(),
        };
        assert_eq!(meta.to_string(), "A test launcher");
    }

    #[test]
    fn searchable_fields_includes_name_description_command_and_tags() {
        let meta = LauncherMetadata {
            name: "Claude Code".to_string(),
            description: "Anthropic's Claude Code CLI".to_string(),
            default_command: "claude".to_string(),
            supported_capabilities: HashSet::new(),
            mcp_transports: HashSet::new(),
            tags: vec!["claude".to_string(), "anthropic".to_string()],
            install: Default::default(),
        };
        let fields = meta.search_fields();
        assert!(fields.contains(&"Claude Code"));
        assert!(fields.contains(&"Anthropic's Claude Code CLI"));
        assert!(fields.contains(&"claude"));
        assert!(fields.contains(&"anthropic"));
    }

    #[tokio::test]
    async fn run_command_dry_run_returns_success() {
        use crate::utils::ui::backends::plain::PlainOutput;
        let ui = PlainOutput;
        let ctx = LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: HashMap::new(),
            dry_run: true,
            usage_tracker: None,
            model_proxy: None,
        };
        let status = run_command(
            PathBuf::from("/usr/bin/echo"),
            &[],
            &["hello".to_string()],
            &ctx,
            &ui,
        )
        .await
        .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn run_command_non_dry_run_executes() {
        use crate::utils::ui::backends::plain::PlainOutput;
        let ui = PlainOutput;
        let ctx = LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: std::env::temp_dir(),
            base_env: HashMap::new(),
            dry_run: false,
            usage_tracker: None,
            model_proxy: None,
        };
        #[cfg(unix)]
        let (binary, args) = (PathBuf::from("/bin/echo"), vec!["hello".to_string()]);
        #[cfg(windows)]
        let (binary, args) = (
            PathBuf::from("cmd"),
            vec!["/C".to_string(), "echo".to_string(), "hello".to_string()],
        );

        let status = run_command(binary, &[], &args, &ctx, &ui).await.unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    fn live_ctx() -> LaunchContext {
        LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: std::env::temp_dir(),
            base_env: HashMap::new(),
            dry_run: false,
            usage_tracker: None,
            model_proxy: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killable_runner_captures_output_and_status() {
        let (status, stdout, _) = run_command_captured_killable(
            PathBuf::from("/bin/sh"),
            &[],
            &["-c".to_string(), "echo hello".to_string()],
            &live_ctx(),
        )
        .await
        .unwrap();
        assert!(status.success());
        assert_eq!(stdout.trim(), "hello");
    }

    #[test]
    fn dry_run_killable_runner_returns_success_without_spawning() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let ctx = LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: PathBuf::from("/nonexistent"),
            base_env: HashMap::new(),
            dry_run: true,
            usage_tracker: None,
            model_proxy: None,
        };
        let (status, stdout, stderr) = rt
            .block_on(run_command_captured_killable(
                PathBuf::from("/nonexistent/binary"),
                &[],
                &[],
                &ctx,
            ))
            .unwrap();
        assert!(status.success());
        assert!(stdout.is_empty() && stderr.is_empty());
    }

    /// Dropping the future must kill the child *and* its descendants: the
    /// shell below backgrounds a grandchild and reports its pid.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_killable_runner_kills_the_child_and_its_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 300 & echo $! > '{}'; wait", pid_file.display());
        let args = ["-c".to_string(), script];
        let ctx = live_ctx();
        let run = run_command_captured_killable(PathBuf::from("/bin/sh"), &[], &args, &ctx);

        // Let the shell start and record its grandchild, then abandon the run.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::pin!(run);
            loop {
                tokio::select! {
                    _ = &mut run => break,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {
                        if pid_file.exists()
                            && !std::fs::read_to_string(&pid_file).unwrap_or_default().trim().is_empty()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .await;
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("the shell should have recorded its background child")
            .trim()
            .parse()
            .unwrap();

        // `run` was dropped when the block above ended. Give the kill a moment.
        let mut dead = false;
        for _ in 0..100 {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(dead, "descendant process {pid} survived the drop");
    }

    /// Two capabilities against one configured model, the second of which
    /// names a model that is not configured when `broken` is true.
    fn capabilities(broken: bool) -> crate::capabilities::CapabilitySource {
        use crate::config::{CapabilityConfig, Config, ModelConfig, ProviderConfig};

        let mut config = Config::default();
        config.providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        config.models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "ollama".to_string(),
                variant: None,
                config: serde_json::json!({}),
            },
        );
        for (id, model_id) in [
            ("chat", "granite-3.1-8b-instruct"),
            (
                "second",
                if broken {
                    "gone"
                } else {
                    "granite-3.1-8b-instruct"
                },
            ),
        ] {
            config.capabilities.insert(
                id.to_string(),
                CapabilityConfig {
                    capability_id: id.to_string(),
                    capability_type: "agent-model".to_string(),
                    config: serde_json::json!({ "model_id": model_id }),
                },
            );
        }
        crate::capabilities::CapabilitySource::from_config(&config)
    }

    #[tokio::test]
    async fn a_capability_that_does_not_resolve_leaves_nothing_bound() {
        let mut launcher = FakeLauncher::new("fake", &serde_json::json!({})).unwrap();
        let enabled = vec!["chat".to_string(), "second".to_string()];

        let err = resolve_and_bind(&mut launcher, &enabled, &capabilities(true))
            .await
            .err()
            .expect("the second capability does not resolve");

        assert!(err.to_string().contains("second"), "got: {err}");
        assert!(
            launcher.bound.lock().unwrap().is_empty(),
            "the first capability resolved, and nothing was bound"
        );
    }

    #[tokio::test]
    async fn healthy_capabilities_bind_in_the_order_the_launcher_lists_them() {
        let mut launcher = FakeLauncher::new("fake", &serde_json::json!({})).unwrap();
        let enabled = vec!["second".to_string(), "chat".to_string()];

        let resolved = resolve_and_bind(&mut launcher, &enabled, &capabilities(false))
            .await
            .unwrap();

        assert_eq!(resolved.len(), 2);
        assert_eq!(*launcher.bound.lock().unwrap(), enabled);
    }

    // -- mcp_transport_conflict ---------------------------------------------

    use crate::capabilities::{BindingType, CapabilityInfo, McpTransportKind};

    struct FixedTransport(Option<McpTransportKind>);

    impl crate::registry::Named for FixedTransport {
        fn instance_id(&self) -> &str {
            "fixed"
        }
    }

    impl CapabilityInfo for FixedTransport {
        fn name(&self) -> &str {
            "fixed"
        }
        fn description(&self) -> &str {
            "fixed"
        }
        fn binding_types(&self) -> HashSet<BindingType> {
            HashSet::from([BindingType::Mcp])
        }
        fn mcp_transport(&self) -> Option<McpTransportKind> {
            self.0
        }
    }

    fn launcher_meta(
        binding_types: &[BindingType],
        transports: &[McpTransportKind],
    ) -> LauncherMetadata {
        LauncherMetadata {
            name: "t".to_string(),
            description: "t".to_string(),
            default_command: "t".to_string(),
            supported_capabilities: binding_types.iter().copied().collect(),
            mcp_transports: transports.iter().copied().collect(),
            tags: vec![],
        }
    }

    #[test]
    fn conflict_names_the_transport_and_what_the_launcher_offers() {
        let launcher = launcher_meta(
            &[BindingType::Mcp],
            &[McpTransportKind::Stdio, McpTransportKind::Http],
        );
        let conflict =
            mcp_transport_conflict(&launcher, &FixedTransport(Some(McpTransportKind::Sse)))
                .expect("SSE is not offered");
        assert_eq!(conflict.transport, "Sse");
        assert_eq!(conflict.offered, ["Http", "Stdio"]);
    }

    #[test]
    fn no_conflict_when_the_transport_is_offered() {
        let launcher = launcher_meta(&[BindingType::Mcp], &[McpTransportKind::Sse]);
        assert!(
            mcp_transport_conflict(&launcher, &FixedTransport(Some(McpTransportKind::Sse)))
                .is_none()
        );
    }

    #[test]
    fn no_conflict_when_the_capability_fixes_no_transport() {
        let launcher = launcher_meta(&[BindingType::Mcp], &[McpTransportKind::Http]);
        assert!(mcp_transport_conflict(&launcher, &FixedTransport(None)).is_none());
    }

    #[test]
    fn no_conflict_when_the_launcher_takes_no_mcp_bindings() {
        // Binding types already exclude it; this check must not add a second,
        // misleading reason.
        let launcher = launcher_meta(&[BindingType::AgentModel], &[]);
        assert!(
            mcp_transport_conflict(&launcher, &FixedTransport(Some(McpTransportKind::Sse)))
                .is_none()
        );
    }

    #[test]
    fn every_launcher_declares_transports_exactly_when_it_supports_mcp() {
        for (name, meta) in crate::launchers::LAUNCHER_REGISTRY.entries() {
            assert_eq!(
                meta.supported_capabilities.contains(&BindingType::Mcp),
                !meta.mcp_transports.is_empty(),
                "launcher '{name}': mcp_transports must be non-empty iff it supports Mcp"
            );
        }
    }
}
