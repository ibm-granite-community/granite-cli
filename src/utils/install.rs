//! Install instructions for provider and launcher types, and the install
//! environment that every install goes through.
//!
//! [`InstallInfo`] is plain data that each type's `metadata()` returns. A
//! [`Platform`] records the operating system, whether this is WSL, and which
//! of the programs an [`InstallMethod`] needs are on PATH; [`assess`] uses it
//! to say which methods can run on this machine. [`InstallEnv`] is the only
//! way the install code looks up a program, runs a command or checks a
//! provider's health, so tests can replace all three with a fake.

// Standard
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// Third Party
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// Local
use crate::providers::HealthStatus;

/*-- public --*/

/// An operating system an install method is for.
///
/// This and the other install instruction types are `pub` because they
/// become fields of the `pub` `ProviderMetadata` and `LauncherMetadata`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

impl Os {
    /// Every operating system, for a method that runs anywhere.
    pub(crate) const ANY: &'static [Os] = &[Os::Mac, Os::Linux, Os::Windows];
    /// macOS and Linux, for a method that needs a POSIX shell.
    pub(crate) const UNIX: &'static [Os] = &[Os::Mac, Os::Linux];

    /// The operating system this binary was built for. Every target that is
    /// not macOS or Windows is taken to be Linux.
    pub(crate) fn current() -> Self {
        if cfg!(target_os = "macos") {
            Os::Mac
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Linux
        }
    }
}

impl std::fmt::Display for Os {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Os::Mac => write!(f, "macOS"),
            Os::Linux => write!(f, "Linux"),
            Os::Windows => write!(f, "Windows"),
        }
    }
}

/// The package manager or downloader an install method uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MethodKind {
    Brew,
    Winget,
    Npm,
    Uv,
    /// A vendor script: fetched with `curl` and piped to a shell on macOS
    /// and Linux, or run with PowerShell on Windows.
    Script,
}

impl MethodKind {
    /// The programs that must be on PATH for a method of this kind to run on
    /// `os`. `sh` and `bash` are taken to be present on macOS and Linux.
    pub(crate) fn required_programs(self, os: Os) -> &'static [&'static str] {
        match self {
            MethodKind::Brew => &["brew"],
            MethodKind::Winget => &["winget"],
            MethodKind::Npm => &["npm"],
            MethodKind::Uv => &["uv"],
            MethodKind::Script if os == Os::Windows => &["powershell"],
            MethodKind::Script => &["curl"],
        }
    }

    /// Every program any method kind can need, which is what
    /// [`Platform::detect`] looks up.
    const ALL_PROGRAMS: &'static [&'static str] =
        &["brew", "winget", "npm", "uv", "curl", "powershell"];
}

/// One way to install a type: a command line for some operating systems.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallMethod {
    /// The operating systems the command is for.
    pub os: Vec<Os>,
    pub kind: MethodKind,
    /// The command line, run through the shell exactly as shown.
    pub command: String,
    /// True when the command may ask for input, such as a `sudo` password.
    /// It is shown with the method and does not stop it from running: in a
    /// terminal the user answers, and without one the question fails.
    pub attended: bool,
    /// Directories the method installs into that may not be on PATH yet.
    /// A leading `~/` stands for the home directory.
    pub bin_dirs: Vec<String>,
    /// Programs the command needs on PATH beyond those of its kind, such as
    /// `npm` for a vendor script that installs through npm.
    pub needs: Vec<String>,
}

impl InstallMethod {
    pub(crate) fn new(os: &[Os], kind: MethodKind, command: &str) -> Self {
        Self {
            os: os.to_vec(),
            kind,
            command: command.to_string(),
            attended: false,
            bin_dirs: Vec::new(),
            needs: Vec::new(),
        }
    }

    /// Mark the method as one that may ask for input.
    pub(crate) fn attended(mut self) -> Self {
        self.attended = true;
        self
    }

    /// Add a program the command needs on PATH.
    pub(crate) fn needs(mut self, program: &str) -> Self {
        self.needs.push(program.to_string());
        self
    }

    /// Add a directory the method installs into.
    pub(crate) fn bin_dir(mut self, dir: &str) -> Self {
        self.bin_dirs.push(dir.to_string());
        self
    }
}

/// A line telling the user how a provider's server is usually kept running
/// on some operating systems. granite-cli prints it and runs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartNote {
    pub os: Vec<Os>,
    pub text: String,
}

/// How to install a provider or launcher type, part of its registry
/// metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallInfo {
    /// The binary whose presence on PATH shows the type is installed. For a
    /// launcher this is its default command.
    pub program: Option<String>,
    /// The vendor's install page.
    pub page: Option<String>,
    /// A step that is not a command, shown with the manual instructions.
    pub note: Option<String>,
    /// For a local provider: the command that runs the server in the
    /// foreground.
    pub run_command: Option<String>,
    /// For a local provider: how the server is usually kept running.
    pub start_notes: Vec<StartNote>,
    /// Install methods, in the order they are offered.
    pub methods: Vec<InstallMethod>,
}

impl InstallInfo {
    pub(crate) fn program(mut self, program: &str) -> Self {
        self.program = Some(program.to_string());
        self
    }

    pub(crate) fn page(mut self, page: &str) -> Self {
        self.page = Some(page.to_string());
        self
    }

    pub(crate) fn note(mut self, note: &str) -> Self {
        self.note = Some(note.to_string());
        self
    }

    pub(crate) fn run_command(mut self, command: &str) -> Self {
        self.run_command = Some(command.to_string());
        self
    }

    pub(crate) fn start_note(mut self, os: &[Os], text: &str) -> Self {
        self.start_notes.push(StartNote {
            os: os.to_vec(),
            text: text.to_string(),
        });
        self
    }

    pub(crate) fn method(mut self, method: InstallMethod) -> Self {
        self.methods.push(method);
        self
    }

    /// The start notes that apply to `os`.
    pub(crate) fn start_notes_for(&self, os: Os) -> Vec<&str> {
        self.start_notes
            .iter()
            .filter(|n| n.os.contains(&os))
            .map(|n| n.text.as_str())
            .collect()
    }
}

/// What granite-cli knows about this machine when it picks install methods.
#[derive(Debug, Clone)]
pub(crate) struct Platform {
    pub(crate) os: Os,
    pub(crate) wsl: bool,
    /// Each program in `MethodKind::ALL_PROGRAMS` and in the `needs` of the
    /// methods `detect` was given, with where it was found.
    found: HashMap<String, Option<PathBuf>>,
}

impl Platform {
    /// Ask `env` for the operating system, WSL, every program a method kind
    /// can need, and every program a method in `infos` lists in `needs`.
    pub(crate) fn detect(env: &dyn InstallEnv, infos: &[&InstallInfo]) -> Self {
        let extra = infos
            .iter()
            .flat_map(|i| i.methods.iter())
            .flat_map(|m| m.needs.iter().map(String::as_str));
        let found = MethodKind::ALL_PROGRAMS
            .iter()
            .copied()
            .chain(extra)
            .map(|p| (p.to_string(), env.find(p)))
            .collect();
        Self {
            os: env.os(),
            wsl: env.is_wsl(),
            found,
        }
    }

    /// Whether `program` was found. A program `detect` did not look up is
    /// a caller's mistake, such as assessing one type's methods with the
    /// platform detected for another, and would read as "not found"; debug
    /// builds, and so the tests, stop on it.
    fn has(&self, program: &str) -> bool {
        debug_assert!(
            self.found.contains_key(program),
            "'{program}' was not looked up: detect the platform for the same install info"
        );
        self.found.get(program).is_some_and(|p| p.is_some())
    }
}

/// Why an install method cannot run on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// The method is for other operating systems.
    WrongOs,
    /// granite-cli is running under WSL (decision 8 of spec 0028).
    Wsl,
    /// A program the method needs is not on PATH.
    MissingProgram(String),
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unavailable::WrongOs => write!(f, "not for this operating system"),
            Unavailable::Wsl => write!(f, "not run under WSL"),
            Unavailable::MissingProgram(p) => write!(f, "{} not found", program_label(p)),
        }
    }
}

/// An install method and, when it cannot run here, the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MethodStatus<'a> {
    pub(crate) method: &'a InstallMethod,
    pub(crate) unavailable: Option<Unavailable>,
}

/// Every method in `info`, in declared order, with whether it can run on
/// `platform`. A method that may ask for input can run like any other.
pub(crate) fn assess<'a>(info: &'a InstallInfo, platform: &Platform) -> Vec<MethodStatus<'a>> {
    info.methods
        .iter()
        .map(|method| {
            let unavailable = if !method.os.contains(&platform.os) {
                Some(Unavailable::WrongOs)
            } else if platform.wsl {
                Some(Unavailable::Wsl)
            } else {
                method
                    .kind
                    .required_programs(platform.os)
                    .iter()
                    .copied()
                    .chain(method.needs.iter().map(String::as_str))
                    .find(|p| !platform.has(p))
                    .map(|missing| Unavailable::MissingProgram(missing.to_string()))
            };
            MethodStatus {
                method,
                unavailable,
            }
        })
        .collect()
}

/// The methods in `info` that can run on `platform`, in declared order.
pub(crate) fn applicable<'a>(info: &'a InstallInfo, platform: &Platform) -> Vec<&'a InstallMethod> {
    assess(info, platform)
        .into_iter()
        .filter(|s| s.unavailable.is_none())
        .map(|s| s.method)
        .collect()
}

/// The result of running an install command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOutcome {
    pub(crate) success: bool,
    /// The exit status as text, such as "exit status 243".
    pub(crate) status: String,
}

/// Everything the install code does to the machine: look up a program, run
/// a command, and check a provider's health.
#[async_trait]
pub(crate) trait InstallEnv: Send + Sync {
    /// The operating system to pick methods for.
    fn os(&self) -> Os;

    /// Whether granite-cli is running under WSL.
    fn is_wsl(&self) -> bool;

    /// Look `program` up on PATH.
    fn find(&self, program: &str) -> Option<PathBuf>;

    /// Look for `program` in `dir`, where a leading `~/` stands for the
    /// home directory.
    fn find_in(&self, dir: &str, program: &str) -> Option<PathBuf>;

    /// Run `command` through the shell, attached: it inherits granite-cli's
    /// terminal and standard input, and its output goes to standard error.
    /// An `Err` means the shell could not be started; a command that ran and
    /// failed is an `Ok` with `success` false.
    async fn run(&self, command: &str) -> anyhow::Result<RunOutcome>;

    /// Build a provider of `provider_type` from `config` and run its health
    /// check.
    async fn check_provider(
        &self,
        provider_type: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<HealthStatus>;
}

/// The install environment of the machine granite-cli runs on.
pub(crate) struct RealInstallEnv;

#[async_trait]
impl InstallEnv for RealInstallEnv {
    fn os(&self) -> Os {
        Os::current()
    }

    fn is_wsl(&self) -> bool {
        crate::config::Config::is_wsl()
    }

    /// A PATH lookup, the one `resolve_shell_command` makes for a launcher
    /// whose `command_path` is unset, including its retry with `.exe` for
    /// the Windows build under WSL, where `which` can find a Linux binary
    /// that the Windows build cannot run.
    fn find(&self, program: &str) -> Option<PathBuf> {
        let found = which::which(program).ok();
        if cfg!(windows)
            && found
                .as_ref()
                .is_some_and(|p| !p.to_string_lossy().to_lowercase().ends_with(".exe"))
            && let Ok(exe) = which::which(format!("{program}.exe"))
        {
            return Some(exe);
        }
        found
    }

    fn find_in(&self, dir: &str, program: &str) -> Option<PathBuf> {
        let dir = match dir.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()?.join(rest),
            None => PathBuf::from(dir),
        };
        let mut names = vec![program.to_string()];
        if cfg!(windows) {
            names.push(format!("{program}.exe"));
        }
        names
            .into_iter()
            .map(|name| dir.join(name))
            .find(|path| path.is_file())
    }

    /// The installer's standard output goes to granite-cli's standard
    /// error, so that granite-cli's own output stays usable in scripts.
    ///
    /// Without a terminal, on macOS and Linux, the installer runs in its own
    /// process group, so that [`stop_running_install`] can stop every
    /// process it started, both sides of a `curl … | bash` pipeline
    /// included. With a terminal it stays in granite-cli's group, which the
    /// terminal's `Ctrl-C` reaches, and where `sudo` can ask for a password:
    /// a process in a background group that reads the terminal is stopped
    /// until someone resumes it.
    async fn run(&self, command: &str) -> anyhow::Result<RunOutcome> {
        let mut cmd = shell_command(command);
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::io::stderr())
            .stderr(std::process::Stdio::inherit());
        let own_group = cfg!(unix) && !has_terminal();
        #[cfg(unix)]
        if own_group {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn()?;
        let _running = RunningInstall::new(child.id(), own_group);
        let status = child.wait().await?;
        Ok(RunOutcome {
            success: status.success(),
            status: status.to_string(),
        })
    }

    async fn check_provider(
        &self,
        provider_type: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<HealthStatus> {
        let provider = crate::providers::PROVIDER_REGISTRY
            .construct(provider_type, provider_type, config)
            .map_err(|e| anyhow::anyhow!("Failed to create provider: {e}"))?;
        Ok(provider.health_check().await?)
    }
}

/// Stop the installer [`RealInstallEnv`] is running, with every process it
/// started when it runs in its own process group. `main` calls this when a
/// signal ends granite-cli, so that a CI job timeout or `docker stop` leaves
/// nothing running.
pub(crate) fn stop_running_install() {
    let pid = RUNNING_PID.swap(0, Ordering::SeqCst);
    if pid == 0 {
        return;
    }
    let own_group = RUNNING_OWN_GROUP.load(Ordering::SeqCst);
    // Blocking on purpose: granite-cli exits right after this.
    let _ = if cfg!(windows) {
        std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .status()
    } else {
        // `sh`'s own `kill`, since a `kill` binary may be missing. The pid
        // is a number granite-cli got from the operating system.
        let target = if own_group {
            format!("-s KILL -- -{pid}")
        } else {
            format!("-s TERM {pid}")
        };
        std::process::Command::new("sh")
            .args(["-c", &format!("kill {target}")])
            .status()
    };
}

/*-- private --*/

/// The process id of the installer [`RealInstallEnv::run`] is waiting for,
/// or 0.
static RUNNING_PID: AtomicU32 = AtomicU32::new(0);
/// Whether that installer leads its own process group.
static RUNNING_OWN_GROUP: AtomicBool = AtomicBool::new(false);

/// Records the running installer for [`stop_running_install`] while it
/// lives.
struct RunningInstall;

impl RunningInstall {
    fn new(pid: Option<u32>, own_group: bool) -> Self {
        RUNNING_OWN_GROUP.store(own_group, Ordering::SeqCst);
        RUNNING_PID.store(pid.unwrap_or(0), Ordering::SeqCst);
        Self
    }
}

impl Drop for RunningInstall {
    fn drop(&mut self) {
        RUNNING_PID.store(0, Ordering::SeqCst);
    }
}

/// Whether granite-cli has a controlling terminal, which `sudo` and other
/// installers ask their questions on even when standard input is redirected.
fn has_terminal() -> bool {
    if cfg!(windows) {
        return true;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok()
}

/// The name a person knows a required program by, for "Homebrew not found".
fn program_label(program: &str) -> &str {
    match program {
        "brew" => "Homebrew",
        "powershell" => "PowerShell",
        other => other,
    }
}

/// `command` as a child of the shell [`shell_for`] picks.
fn shell_command(command: &str) -> tokio::process::Command {
    let (shell, args) = shell_for(command, cfg!(windows));
    let mut cmd = tokio::process::Command::new(shell);
    cmd.args(args).arg(command);
    cmd
}

/// The shell that runs `command`, and its arguments before the command:
/// `sh -c` on macOS and Linux. On Windows, PowerShell runs a vendor script
/// (`irm … | iex`), and `cmd /C` runs everything else. Under PowerShell,
/// `npm` resolves to `npm.ps1`, which the default execution policy blocks.
fn shell_for(command: &str, windows: bool) -> (&'static str, &'static [&'static str]) {
    if !windows {
        ("sh", &["-c"])
    } else if command.contains("| iex") {
        ("powershell", &["-NoProfile", "-Command"])
    } else {
        ("cmd", &["/C"])
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test_support::{FakeInstallEnv, FakeRun};

    fn info() -> InstallInfo {
        InstallInfo::default()
            .program("tool")
            .page("https://example.com/install")
            .method(InstallMethod::new(
                &[Os::Mac],
                MethodKind::Brew,
                "brew install tool",
            ))
            .method(
                InstallMethod::new(
                    Os::UNIX,
                    MethodKind::Script,
                    "curl -fsSL https://example.com/i.sh | sh",
                )
                .bin_dir("~/.tool/bin"),
            )
            .method(InstallMethod::new(
                Os::ANY,
                MethodKind::Npm,
                "npm install -g tool",
            ))
            .method(InstallMethod::new(
                &[Os::Windows],
                MethodKind::Winget,
                "winget install tool",
            ))
    }

    fn commands(methods: &[&InstallMethod]) -> Vec<String> {
        methods.iter().map(|m| m.command.clone()).collect()
    }

    fn reason<'a>(statuses: &'a [MethodStatus<'a>], command: &str) -> Option<&'a Unavailable> {
        statuses
            .iter()
            .find(|s| s.method.command == command)
            .and_then(|s| s.unavailable.as_ref())
    }

    #[test]
    fn mac_with_homebrew_and_curl_selects_brew_first() {
        let env = FakeInstallEnv::new(Os::Mac).with_programs(&["brew", "curl"]);
        let info = info();
        let methods = applicable(&info, &Platform::detect(&env, &[&info]));
        assert_eq!(
            commands(&methods),
            vec![
                "brew install tool",
                "curl -fsSL https://example.com/i.sh | sh"
            ]
        );
    }

    #[test]
    fn mac_without_homebrew_lists_brew_with_its_reason() {
        let env = FakeInstallEnv::new(Os::Mac).with_programs(&["curl"]);
        let info = info();
        let platform = Platform::detect(&env, &[&info]);
        let statuses = assess(&info, &platform);
        assert_eq!(
            reason(&statuses, "brew install tool"),
            Some(&Unavailable::MissingProgram("brew".to_string()))
        );
        assert_eq!(
            reason(&statuses, "brew install tool").unwrap().to_string(),
            "Homebrew not found"
        );
        assert_eq!(
            commands(&applicable(&info, &platform)),
            vec!["curl -fsSL https://example.com/i.sh | sh"]
        );
    }

    #[test]
    fn linux_with_npm_only_selects_npm() {
        let env = FakeInstallEnv::new(Os::Linux).with_programs(&["npm"]);
        let info = info();
        let platform = Platform::detect(&env, &[&info]);
        assert_eq!(
            commands(&applicable(&info, &platform)),
            vec!["npm install -g tool"]
        );
        let statuses = assess(&info, &platform);
        assert_eq!(
            reason(&statuses, "brew install tool"),
            Some(&Unavailable::WrongOs)
        );
        assert_eq!(
            reason(&statuses, "curl -fsSL https://example.com/i.sh | sh"),
            Some(&Unavailable::MissingProgram("curl".to_string()))
        );
    }

    #[test]
    fn windows_with_winget_selects_winget() {
        let env = FakeInstallEnv::new(Os::Windows).with_programs(&["winget", "curl"]);
        let info = info();
        assert_eq!(
            commands(&applicable(&info, &Platform::detect(&env, &[&info]))),
            vec!["winget install tool"]
        );
    }

    #[test]
    fn method_that_may_ask_for_input_runs_like_any_other() {
        let info = InstallInfo::default()
            .method(InstallMethod::new(&[Os::Linux], MethodKind::Script, "curl x | sh").attended());
        let env = FakeInstallEnv::new(Os::Linux).with_programs(&["curl"]);
        let platform = Platform::detect(&env, &[&info]);

        let statuses = assess(&info, &platform);
        assert_eq!(statuses[0].unavailable, None);
        assert!(statuses[0].method.attended);
        assert_eq!(applicable(&info, &platform).len(), 1);
    }

    #[test]
    fn method_needing_an_extra_program_waits_for_it() {
        let info = InstallInfo::default().method(
            InstallMethod::new(
                &[Os::Linux],
                MethodKind::Script,
                "curl x | bash -s -- --pm npm",
            )
            .needs("npm"),
        );
        let without = FakeInstallEnv::new(Os::Linux).with_programs(&["curl"]);
        let statuses = assess(&info, &Platform::detect(&without, &[&info]));
        assert_eq!(
            statuses[0].unavailable,
            Some(Unavailable::MissingProgram("npm".to_string()))
        );

        let with = FakeInstallEnv::new(Os::Linux).with_programs(&["curl", "npm"]);
        assert_eq!(
            applicable(&info, &Platform::detect(&with, &[&info])).len(),
            1
        );
    }

    #[test]
    fn every_method_is_unavailable_under_wsl() {
        let env = FakeInstallEnv::new(Os::Windows)
            .with_programs(&["winget", "npm", "curl", "powershell"])
            .wsl();
        let info = info();
        let platform = Platform::detect(&env, &[&info]);
        assert!(applicable(&info, &platform).is_empty());
        let statuses = assess(&info, &platform);
        assert_eq!(
            reason(&statuses, "npm install -g tool"),
            Some(&Unavailable::Wsl)
        );
        assert_eq!(
            reason(&statuses, "winget install tool"),
            Some(&Unavailable::Wsl)
        );
    }

    #[test]
    fn windows_runs_scripts_in_powershell_and_the_rest_in_cmd() {
        assert_eq!(shell_for("npm install -g tool", false), ("sh", &["-c"][..]));
        assert_eq!(
            shell_for("irm https://t.ps1 | iex", true),
            ("powershell", &["-NoProfile", "-Command"][..])
        );
        assert_eq!(shell_for("npm install -g tool", true), ("cmd", &["/C"][..]));
        assert_eq!(shell_for("winget install tool", true), ("cmd", &["/C"][..]));
    }

    #[test]
    fn start_notes_for_filters_by_os() {
        let info = InstallInfo::default()
            .start_note(&[Os::Mac], "brew services start tool")
            .start_note(&[Os::Linux], "systemctl start tool");
        assert_eq!(
            info.start_notes_for(Os::Mac),
            vec!["brew services start tool"]
        );
        assert!(info.start_notes_for(Os::Windows).is_empty());
    }

    #[tokio::test]
    async fn fake_env_records_commands_and_installs_what_it_was_told() {
        let env = FakeInstallEnv::new(Os::Mac).with_run(
            "npm install -g tool",
            FakeRun::ok().installs("tool", "/usr/local/bin/tool"),
        );
        let outcome = env.run("npm install -g tool").await.unwrap();

        assert!(outcome.success);
        assert_eq!(env.runs(), vec!["npm install -g tool".to_string()]);
        assert_eq!(env.find("tool"), Some(PathBuf::from("/usr/local/bin/tool")));
    }

    #[tokio::test]
    async fn fake_env_scripted_failure_installs_nothing_and_health_answers_in_order() {
        let env = FakeInstallEnv::new(Os::Linux)
            .with_run("bad", FakeRun::failed().installs("tool", "/x/tool"))
            .with_run(
                "script",
                FakeRun::ok().installs_in("~/.tool/bin", "tool", "/home/me/.tool/bin/tool"),
            )
            .with_health(&[false, true]);

        let failed = env.run("bad").await.unwrap();
        assert!(!failed.success);
        assert_eq!(env.find("tool"), None);

        env.run("script").await.unwrap();
        assert_eq!(env.find("tool"), None);
        assert_eq!(
            env.find_in("~/.tool/bin", "tool"),
            Some(PathBuf::from("/home/me/.tool/bin/tool"))
        );

        let config = serde_json::json!({});
        assert!(!env.check_provider("ollama", &config).await.unwrap().healthy);
        assert!(env.check_provider("ollama", &config).await.unwrap().healthy);
        assert!(!env.check_provider("ollama", &config).await.unwrap().healthy);
        assert_eq!(env.health_checks(), vec!["ollama"; 3]);
    }

    #[tokio::test]
    async fn fake_env_fails_a_command_it_was_not_given() {
        let env = FakeInstallEnv::new(Os::Linux);
        let outcome = env.run("rm -rf /").await.unwrap();
        assert!(!outcome.success);
        assert_eq!(env.runs().len(), 1);
    }
}
