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
        capability: &dyn crate::capabilities::Capability,
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
    #[cfg(windows)]
    let (binary_fallback, args_fallback, overlay_fallback) =
        (binary.clone(), args.to_vec(), translated_overlay);

    tokio::task::spawn_blocking(move || -> anyhow::Result<std::process::ExitStatus> {
        let mut spawn_cmd = cmd;

        // In WSL, the parent's CWD may be a WSL path that gets translated to
        // a UNC path for Windows processes. If the binary is on a Windows
        // drive, set the CWD to that drive's root to avoid the UNC issue.
        #[cfg(windows)]
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

/// Metadata describing a launcher implementation (type-level, catalog entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherMetadata {
    pub name: String,
    pub description: String,
    /// The default binary name used for PATH lookup (e.g. `"claude"`, `"bob"`).
    pub default_command: String,
    /// Binding surfaces this launcher type can make use of.
    pub supported_capabilities: HashSet<BindingType>,
    pub tags: Vec<String>,
}

impl std::fmt::Display for LauncherMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.description)
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
}

/// A single environment variable binding contributed to the subprocess overlay.
#[derive(Debug, Clone)]
pub struct EnvBinding {
    pub key: String,
    pub value: String,
}

/*-- private --*/

define_factory!(Launcher, LauncherMetadata, LauncherFactory);

/*-- tests --*/

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::registry::ConfigConstructable;

    /// Minimal Launcher implementation used only in tests.
    pub(crate) struct FakeLauncher {
        instance_id: String,
        /// When `Some`, `validate_command` resolves to this path directly.
        command_path: Option<PathBuf>,
        command_name: String,
    }

    impl crate::registry::Named for FakeLauncher {
        fn instance_id(&self) -> &str {
            &self.instance_id
        }
    }

    impl ConfigConstructable for FakeLauncher {
        type Config = crate::registry::NoConfig;

        fn new(
            instance_id: &str,
            cfg: &serde_json::Value,
            _global_config: &crate::config::Config,
        ) -> Self {
            let command_name = cfg
                .get("command_name")
                .and_then(|v| v.as_str())
                .unwrap_or("fake-binary-that-does-not-exist")
                .to_string();
            let command_path = cfg
                .get("command_path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Self {
                instance_id: instance_id.to_string(),
                command_name,
                command_path,
            }
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
            _capability: &dyn crate::capabilities::Capability,
        ) -> anyhow::Result<()> {
            anyhow::bail!("Capability binding not supported");
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
                tags: vec![],
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
            &crate::config::Config::default(),
        );
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
            &crate::config::Config::default(),
        );
        assert!(launcher.validate_command().is_err());
    }

    #[test]
    fn validate_command_falls_back_to_path_for_bare_command_name() {
        let launcher = FakeLauncher::new(
            "my-fake",
            &serde_json::json!({
                "command_path": "ls"
            }),
            &crate::config::Config::default(),
        );
        assert!(launcher.validate_command().is_ok());
    }

    #[tokio::test]
    async fn env_overlay_default_is_empty() {
        let launcher = FakeLauncher::new(
            "my-fake",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        let ctx = LaunchContext {
            launcher_id: "test".to_string(),
            working_dir: PathBuf::from("/tmp"),
            base_env: HashMap::new(),
            dry_run: false,
        };
        let overlay = launcher.env_overlay(&ctx).await.unwrap();
        assert!(overlay.is_empty());
    }

    #[test]
    fn map_tool_name_default_passes_through_other_and_returns_none_for_everything_else() {
        let launcher = FakeLauncher::new(
            "my-fake",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
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
        let result = factory.construct(
            "fake",
            "my-fake",
            &serde_json::json!({}),
            &crate::config::Config::default(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn launcher_metadata_display() {
        let meta = LauncherMetadata {
            name: "Test".to_string(),
            description: "A test launcher".to_string(),
            default_command: "test".to_string(),
            supported_capabilities: HashSet::new(),
            tags: vec![],
        };
        assert_eq!(meta.to_string(), "A test launcher");
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
            working_dir: PathBuf::from("/tmp"),
            base_env: HashMap::new(),
            dry_run: false,
        };
        let status = run_command(
            PathBuf::from("/bin/echo"),
            &[],
            &["hello".to_string()],
            &ctx,
            &ui,
        )
        .await
        .unwrap();
        assert!(status.success());
    }
}
