//! What `provider info`, `launcher info`, `provider setup`, `launcher setup`
//! and `setup` show and do about installing a provider or launcher type.

// Local
use crate::utils::install::{
    InstallEnv, InstallInfo, InstallMethod, Platform, Unavailable, applicable, assess,
};
use crate::utils::ui::Ui;

/*-- public --*/

/// Show the install detail of a type as an "Install" block, unless `info`
/// holds nothing.
pub(crate) fn show_install_detail(ctx: &crate::AppContext, info: &InstallInfo) {
    let fields = install_fields(&*ctx.install_env(), info);
    if fields.is_empty() {
        return;
    }
    let fields: Vec<(&str, String)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.clone()))
        .collect();
    ctx.ui.detail("Install", &fields);
}

/// The install detail of a type, as fields for `Ui::detail`: the program and
/// where it was found, each method for this operating system with whether it
/// runs here or why not, the run command, the start notes for this operating
/// system, the page and the note. Empty when `info` holds nothing.
///
/// Methods for other operating systems are left out. When a key repeats,
/// as "Install" does for several methods, each one is numbered ("Install 1",
/// "Install 2"), since the JSON backend keeps one value per key.
pub(crate) fn install_fields(env: &dyn InstallEnv, info: &InstallInfo) -> Vec<(String, String)> {
    let platform = Platform::detect(env, &[info]);
    let mut fields: Vec<(&'static str, String)> = Vec::new();

    if let Some(program) = &info.program {
        let found = match env.find(program) {
            Some(path) => format!("{program} ({})", path.display()),
            None => format!("{program} (not found on PATH)"),
        };
        fields.push(("Program", found));
    }

    for status in assess(info, &platform) {
        let verdict = match &status.unavailable {
            Some(Unavailable::WrongOs) => continue,
            Some(reason) => reason.to_string(),
            None if status.method.attended => {
                format!("✓ runs on this machine, {MAY_ASK}")
            }
            None => "✓ runs on this machine".to_string(),
        };
        fields.push(("Install", format!("{}  [{verdict}]", status.method.command)));
    }

    if let Some(run) = &info.run_command {
        fields.push(("Run", run.clone()));
    }
    for note in info.start_notes_for(platform.os) {
        fields.push(("Start", note.to_string()));
    }
    if let Some(page) = &info.page {
        fields.push(("Page", page.clone()));
    }
    if let Some(note) = &info.note {
        fields.push(("Note", note.clone()));
    }
    number_repeated_keys(fields)
}

/// What happened when granite-cli offered or ran an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallOutcome {
    /// The command succeeded. `path` is where the program was found, when
    /// the type names one; `off_path` is true when it was found only in one
    /// of the method's directories.
    Installed {
        path: Option<std::path::PathBuf>,
        off_path: bool,
    },
    /// The user chose not to install.
    Declined,
    /// The command failed, or succeeded without the program appearing.
    Failed,
    /// No method can run on this machine.
    Unavailable,
    /// The UI is not interactive, or cannot lend the terminal, so nothing
    /// was offered.
    NotOffered,
}

/// Offer to install a type whose program is missing.
///
/// Lists the methods that can run here, each as its full command, and a line
/// that installs nothing; choosing a command is the consent to run it. The
/// line that installs nothing is the default, so pressing Enter runs
/// nothing, and a prompt the user cancels counts as declining. A method
/// that may ask for input is marked. When the UI is not interactive,
/// or cannot lend the terminal to the installer, as in the TUI, nothing is
/// offered and the manual instructions are shown. `name` is the type's
/// display name.
pub(crate) async fn offer_install(
    ctx: &crate::AppContext,
    name: &str,
    info: &InstallInfo,
) -> anyhow::Result<InstallOutcome> {
    let env = ctx.install_env();
    let ui = &*ctx.ui;
    if !ui.is_interactive() || !ui.can_lend_terminal() {
        show_manual_instructions(ui, &*env, name, info);
        return Ok(InstallOutcome::NotOffered);
    }

    let platform = Platform::detect(&*env, &[info]);
    let methods = applicable(info, &platform);
    if methods.is_empty() {
        show_manual_instructions(ui, &*env, name, info);
        return Ok(InstallOutcome::Unavailable);
    }

    let mut items: Vec<String> = methods
        .iter()
        .map(|m| {
            if m.attended {
                format!("{}  ({MAY_ASK})", m.command)
            } else {
                m.command.clone()
            }
        })
        .collect();
    items.push(DONT_INSTALL.to_string());
    let choice = ui
        .select(&format!("Install {name} now?"), &items, items.len() - 1)
        .ok();
    let Some(method) = choice.and_then(|c| methods.get(c)) else {
        show_manual_instructions(ui, &*env, name, info);
        return Ok(InstallOutcome::Declined);
    };

    run_method(ui, &*env, name, info, method).await
}

/// Run `method`, which the user has already chosen.
pub(crate) async fn install_with(
    ctx: &crate::AppContext,
    name: &str,
    info: &InstallInfo,
    method: &InstallMethod,
) -> anyhow::Result<InstallOutcome> {
    run_method(&*ctx.ui, &*ctx.install_env(), name, info, method).await
}

/// The first method of `info` that can run here; or, when none can, the
/// reason the first method for this operating system cannot.
pub(crate) fn first_method(
    ctx: &crate::AppContext,
    info: &InstallInfo,
) -> Result<InstallMethod, Option<Unavailable>> {
    let env = ctx.install_env();
    let platform = Platform::detect(&*env, &[info]);
    let statuses = assess(info, &platform);
    if let Some(ok) = statuses.iter().find(|s| s.unavailable.is_none()) {
        return Ok(ok.method.clone());
    }
    Err(statuses
        .into_iter()
        .filter_map(|s| s.unavailable)
        .find(|u| *u != Unavailable::WrongOs))
}

/// Install a type with its first method that can run here, asking nothing.
/// Used by `setup --auto --install`. A method that may ask for input runs
/// too: without a terminal its question fails, and the failure is reported.
pub(crate) async fn install_unattended(
    ctx: &crate::AppContext,
    name: &str,
    info: &InstallInfo,
) -> anyhow::Result<InstallOutcome> {
    let env = ctx.install_env();
    let ui = &*ctx.ui;
    let platform = Platform::detect(&*env, &[info]);
    let Some(method) = applicable(info, &platform).into_iter().next() else {
        show_manual_instructions(ui, &*env, name, info);
        return Ok(InstallOutcome::Unavailable);
    };
    run_method(ui, &*env, name, info, method).await
}

/// What granite-cli knows about a provider's program when its server does
/// not answer, which decides the first line of [`show_not_answering`].
pub(crate) enum ServerProgram<'a> {
    /// granite-cli installed it in this run.
    Installed,
    /// The program was found on PATH at this path.
    FoundAt(&'a std::path::Path),
    /// The program was not found, and nothing was installed.
    Unknown,
}

/// Say that `name` is not answering at `endpoint`, then how to start it,
/// and ask "Check again" or "Continue without it", running the health check
/// of `provider_type` with `config` through the install environment on each
/// "Check again". Returns whether the provider ended healthy. granite-cli
/// runs nothing else: it does not start the server. "Continue without it" is
/// the default, so pressing Enter, cancelling the prompt, or a caller that
/// answers every prompt with its default, ends the loop.
///
/// When the UI is not interactive, only the message and instructions are
/// shown.
pub(crate) async fn wait_for_provider(
    ctx: &crate::AppContext,
    name: &str,
    program: ServerProgram<'_>,
    provider_type: &str,
    config: &serde_json::Value,
    endpoint: &str,
    info: &InstallInfo,
) -> anyhow::Result<bool> {
    let env = ctx.install_env();
    let ui = &*ctx.ui;
    // An install can start the server, as Ollama's Linux script does with
    // its service, so check before saying it is not answering.
    if matches!(program, ServerProgram::Installed)
        && matches!(env.check_provider(provider_type, config).await, Ok(s) if s.healthy)
    {
        ui.info(&ui.ok(&format!("✓ {name} is answering at {endpoint}.")));
        return Ok(true);
    }
    show_not_answering(ui, &*env, name, program, endpoint, info);
    if !ui.is_interactive() {
        return Ok(false);
    }

    let items = vec![CHECK_AGAIN.to_string(), CONTINUE_WITHOUT.to_string()];
    loop {
        if ui.select(WOULD_YOU_LIKE_TO, &items, 1).ok() != Some(0) {
            return Ok(false);
        }
        match env.check_provider(provider_type, config).await {
            Ok(status) if status.healthy => return Ok(true),
            Ok(status) => match status.error {
                Some(error) => ui.warn(&format!(
                    "{name} is still not answering at {endpoint}: {error}"
                )),
                None => ui.warn(&format!("{name} is still not answering at {endpoint}.")),
            },
            Err(e) => ui.warn(&format!("Could not run health check: {e}")),
        }
    }
}

/// Print that `name` is not answering at `endpoint`, with what is known
/// about its program, then how to start it: the run command and the start
/// notes for this operating system. granite-cli runs neither.
pub(crate) fn show_not_answering(
    ui: &dyn Ui,
    env: &dyn InstallEnv,
    name: &str,
    program: ServerProgram<'_>,
    endpoint: &str,
    info: &InstallInfo,
) {
    ui.warn(&match program {
        ServerProgram::Installed => {
            format!("{name} is installed but not answering at {endpoint}.")
        }
        ServerProgram::FoundAt(path) => format!(
            "{name} was found at {} but is not answering at {endpoint}.",
            path.display()
        ),
        ServerProgram::Unknown => format!("{name} is not answering at {endpoint}."),
    });
    let Some(run) = &info.run_command else {
        if let Some(note) = &info.note {
            ui.info(&format!("  {note}"));
        }
        return;
    };
    ui.info("  You can start it in another terminal with:");
    ui.info(&format!("      {run}"));
    for note in info.start_notes_for(env.os()) {
        ui.info(&format!("  {note}"));
    }
}

/// The question asked after [`show_not_answering`].
/// dialoguer's default theme adds the colon.
pub(crate) const WOULD_YOU_LIKE_TO: &str = "Would you like to";

/// Print how to install a type by hand: every method for this operating
/// system with what it needs, then the page and the note. This is the one
/// format manual instructions take everywhere.
pub(crate) fn show_manual_instructions(
    ui: &dyn Ui,
    env: &dyn InstallEnv,
    name: &str,
    info: &InstallInfo,
) {
    let platform = Platform::detect(env, &[info]);
    let methods: Vec<_> = assess(info, &platform)
        .into_iter()
        .filter(|s| s.unavailable != Some(Unavailable::WrongOs))
        .collect();
    if methods.is_empty() {
        ui.info(&format!("{name} cannot be installed from granite-cli."));
    } else {
        ui.info(&format!("To install {name} yourself:"));
        for status in methods {
            match &status.unavailable {
                Some(reason) => ui.info(&format!("  {}  ({reason})", status.method.command)),
                None => ui.info(&format!("  {}", status.method.command)),
            }
        }
    }
    if let Some(page) = &info.page {
        ui.info(&format!("  See {page}"));
    }
    if let Some(note) = &info.note {
        ui.info(&format!("  {note}"));
    }
}

/*-- private --*/

const DONT_INSTALL: &str = "Don't install (show manual instructions)";
const MAY_ASK: &str = "may ask for input";
const CHECK_AGAIN: &str = "Check again";
const CONTINUE_WITHOUT: &str = "Continue without it";

/// Run `method`, report the result, and look for the program afterwards:
/// on PATH first, then in the method's directories.
async fn run_method(
    ui: &dyn Ui,
    env: &dyn InstallEnv,
    name: &str,
    info: &InstallInfo,
    method: &InstallMethod,
) -> anyhow::Result<InstallOutcome> {
    ui.info(&format!("Running: {}", method.command));
    let outcome = match env.run(&method.command).await {
        Ok(outcome) => outcome,
        Err(e) => {
            ui.error(&ui.error_mark(&format!("Could not run the install command: {e}")));
            show_manual_instructions(ui, env, name, info);
            return Ok(InstallOutcome::Failed);
        }
    };

    if !outcome.success {
        ui.error(&ui.error_mark(&format!("Install failed ({}).", outcome.status)));
        show_manual_instructions(ui, env, name, info);
        return Ok(InstallOutcome::Failed);
    }

    let Some(program) = &info.program else {
        ui.info(&ui.ok("✓ Installed."));
        return Ok(InstallOutcome::Installed {
            path: None,
            off_path: false,
        });
    };
    if let Some(path) = env.find(program) {
        ui.info(&ui.ok(&format!("✓ Installed: {}", path.display())));
        return Ok(InstallOutcome::Installed {
            path: Some(path),
            off_path: false,
        });
    }
    for dir in &method.bin_dirs {
        if let Some(path) = env.find_in(dir, program) {
            ui.info(&ui.ok("✓ Installed."));
            ui.warn(&format!(
                "'{program}' is not on PATH in this shell. Found {}.",
                path.display()
            ));
            if let Some(parent) = path.parent() {
                ui.info(&format!(
                    "  Add {} to PATH to run {program} outside granite-cli.",
                    parent.display()
                ));
            }
            return Ok(InstallOutcome::Installed {
                path: Some(path),
                off_path: true,
            });
        }
    }

    ui.error(&ui.error_mark(&format!(
        "The install command finished, but '{program}' was not found."
    )));
    show_manual_instructions(ui, env, name, info);
    Ok(InstallOutcome::Failed)
}

/// Turn each key that appears more than once into "<key> 1", "<key> 2", in
/// order.
fn number_repeated_keys(fields: Vec<(&'static str, String)>) -> Vec<(String, String)> {
    let count = |key: &str| fields.iter().filter(|(k, _)| *k == key).count();
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    fields
        .iter()
        .map(|(k, v)| {
            if count(k) > 1 {
                let n = seen.entry(k).or_insert(0);
                *n += 1;
                (format!("{k} {n}"), v.clone())
            } else {
                (k.to_string(), v.clone())
            }
        })
        .collect()
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::install::{InstallMethod, MethodKind, Os};
    use crate::utils::test_support::{FakeInstallEnv, FakeRun};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn info() -> InstallInfo {
        InstallInfo::default()
            .program("tool")
            .page("https://example.com")
            .note("A note.")
            .run_command("tool serve")
            .start_note(&[Os::Mac], "brew services start tool")
            .start_note(&[Os::Linux], "systemctl start tool")
            .method(InstallMethod::new(
                &[Os::Mac],
                MethodKind::Brew,
                "brew install tool",
            ))
            .method(InstallMethod::new(
                &[Os::Linux],
                MethodKind::Npm,
                "npm i -g tool",
            ))
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(k, v)| pair(k, v)).collect()
    }

    #[test]
    fn fields_list_this_os_only_with_verdicts() {
        let env = FakeInstallEnv::new(Os::Mac).with_programs(&["brew", "tool"]);
        let fields = install_fields(&env, &info());
        assert_eq!(
            fields,
            pairs(&[
                ("Program", "tool (/usr/bin/tool)"),
                ("Install", "brew install tool  [✓ runs on this machine]"),
                ("Run", "tool serve"),
                ("Start", "brew services start tool"),
                ("Page", "https://example.com"),
                ("Note", "A note."),
            ])
        );
    }

    #[test]
    fn fields_give_the_reason_a_method_cannot_run() {
        let env = FakeInstallEnv::new(Os::Mac);
        let fields = install_fields(&env, &info());
        assert!(fields.contains(&pair("Program", "tool (not found on PATH)")));
        assert!(fields.contains(&pair("Install", "brew install tool  [Homebrew not found]")));
    }

    #[test]
    fn repeated_keys_are_numbered() {
        let info = InstallInfo::default()
            .method(InstallMethod::new(
                &[Os::Linux],
                MethodKind::Npm,
                "npm i -g a",
            ))
            .method(InstallMethod::new(
                &[Os::Linux],
                MethodKind::Uv,
                "uv tool install a",
            ))
            .page("https://example.com");
        let env = FakeInstallEnv::new(Os::Linux).with_programs(&["npm"]);
        let keys: Vec<String> = install_fields(&env, &info)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, vec!["Install 1", "Install 2", "Page"]);
    }

    #[test]
    fn a_method_that_may_ask_for_input_runs_and_says_so() {
        let info = InstallInfo::default()
            .method(InstallMethod::new(&[Os::Linux], MethodKind::Script, "curl x | sh").attended());
        let env = FakeInstallEnv::new(Os::Linux).with_programs(&["curl"]);
        assert_eq!(
            install_fields(&env, &info),
            pairs(&[(
                "Install",
                "curl x | sh  [✓ runs on this machine, may ask for input]"
            )])
        );
    }

    #[test]
    fn empty_info_has_no_fields() {
        let env = FakeInstallEnv::default();
        assert!(install_fields(&env, &InstallInfo::default()).is_empty());
    }

    // -- install flow ----------------------------------------------------------

    fn flow_ctx(
        env: FakeInstallEnv,
        ui: CaptureUi,
    ) -> (crate::AppContext, Arc<FakeInstallEnv>, Arc<CaptureUi>) {
        let env = Arc::new(env);
        let ui = Arc::new(ui);
        let mut ctx = crate::AppContext::new(crate::config::Config::default(), ui.clone());
        ctx.set_install_env(env.clone());
        (ctx, env, ui)
    }

    fn tool_info() -> InstallInfo {
        InstallInfo::default()
            .program("tool")
            .page("https://example.com/tool")
            .run_command("tool serve")
            .start_note(&[Os::Linux], "systemctl start tool")
            .method(InstallMethod::new(
                &[Os::Linux],
                MethodKind::Npm,
                "npm install -g tool",
            ))
            .method(
                InstallMethod::new(
                    &[Os::Linux],
                    MethodKind::Script,
                    "curl -fsSL https://t.sh | sh",
                )
                .bin_dir("~/.tool/bin"),
            )
            .method(
                InstallMethod::new(
                    &[Os::Linux],
                    MethodKind::Script,
                    "curl -fsSL https://sudo.sh | sh",
                )
                .attended(),
            )
    }

    fn all_text(ui: &CaptureUi) -> String {
        let mut lines = ui.infos.borrow().clone();
        lines.extend(ui.warns.borrow().iter().cloned());
        lines.extend(ui.errors.borrow().iter().cloned());
        lines.join("\n")
    }

    #[tokio::test]
    async fn declining_runs_nothing_and_shows_manual_instructions() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(3); // Don't install
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux).with_programs(&["npm", "curl"]),
            ui,
        );

        let outcome = offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(outcome, InstallOutcome::Declined);
        assert!(env.runs().is_empty());
        let (_, items, _) = &ui.select_prompts.borrow()[0];
        assert_eq!(
            items,
            &vec![
                "npm install -g tool".to_string(),
                "curl -fsSL https://t.sh | sh".to_string(),
                "curl -fsSL https://sudo.sh | sh  (may ask for input)".to_string(),
                DONT_INSTALL.to_string(),
            ]
        );
        assert!(all_text(&ui).contains("To install Tool yourself:"));
        assert!(all_text(&ui).contains("See https://example.com/tool"));
    }

    #[tokio::test]
    async fn pressing_enter_installs_nothing() {
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux).with_programs(&["npm", "curl"]),
            CaptureUi::default(),
        );

        let outcome = offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(outcome, InstallOutcome::Declined);
        assert!(env.runs().is_empty());
        let (_, items, default) = &ui.select_prompts.borrow()[0];
        assert_eq!(items[*default], DONT_INSTALL);
    }

    #[tokio::test]
    async fn choosing_a_method_runs_exactly_that_command() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(0);
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["npm", "curl"])
                .with_run(
                    "npm install -g tool",
                    FakeRun::ok().installs("tool", "/usr/local/bin/tool"),
                ),
            ui,
        );

        let outcome = offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(
            outcome,
            InstallOutcome::Installed {
                path: Some("/usr/local/bin/tool".into()),
                off_path: false
            }
        );
        assert_eq!(env.runs(), vec!["npm install -g tool".to_string()]);
        let text = all_text(&ui);
        assert!(text.contains("Running: npm install -g tool"));
        assert!(text.contains("✓ Installed: /usr/local/bin/tool"));
    }

    #[tokio::test]
    async fn choosing_a_later_method_runs_only_that_command() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(1);
        let (ctx, env, _ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["npm", "curl"])
                .with_run(
                    "curl -fsSL https://t.sh | sh",
                    FakeRun::ok().installs("tool", "/x/tool"),
                ),
            ui,
        );

        offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(env.runs(), vec!["curl -fsSL https://t.sh | sh".to_string()]);
    }

    #[tokio::test]
    async fn a_ui_that_cannot_lend_the_terminal_gets_manual_instructions() {
        let ui = CaptureUi::default();
        *ui.lends_terminal.borrow_mut() = Some(false);
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux).with_programs(&["npm", "curl"]),
            ui,
        );

        let outcome = offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(outcome, InstallOutcome::NotOffered);
        assert!(env.runs().is_empty());
        assert!(ui.select_prompts.borrow().is_empty());
        assert!(all_text(&ui).contains("To install Tool yourself:"));
    }

    #[tokio::test]
    async fn a_failure_shows_the_status_and_the_manual_instructions() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(0);
        let (ctx, _, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["npm"])
                .with_run("npm install -g tool", FakeRun::failed()),
            ui,
        );

        let outcome = offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(outcome, InstallOutcome::Failed);
        assert_eq!(*ui.errors.borrow(), vec!["Install failed (exit status 1)."]);
        assert!(all_text(&ui).contains("To install Tool yourself:"));
    }

    #[tokio::test]
    async fn a_program_found_only_in_a_method_directory_is_returned_with_a_warning() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(0);
        let (ctx, _, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["curl"])
                .with_run(
                    "curl -fsSL https://t.sh | sh",
                    FakeRun::ok().installs_in("~/.tool/bin", "tool", "/home/me/.tool/bin/tool"),
                ),
            ui,
        );

        let outcome = offer_install(&ctx, "Tool", &tool_info()).await.unwrap();

        assert_eq!(
            outcome,
            InstallOutcome::Installed {
                path: Some("/home/me/.tool/bin/tool".into()),
                off_path: true
            }
        );
        assert!(ui.warns.borrow()[0].contains("'tool' is not on PATH in this shell"));
        assert!(all_text(&ui).contains("Add /home/me/.tool/bin to PATH"));
    }

    #[tokio::test]
    async fn a_success_that_leaves_no_program_is_a_failure() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(0);
        let (ctx, _, _) = flow_ctx(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["npm"])
                .with_run("npm install -g tool", FakeRun::ok()),
            ui,
        );
        assert_eq!(
            offer_install(&ctx, "Tool", &tool_info()).await.unwrap(),
            InstallOutcome::Failed
        );
    }

    #[tokio::test]
    async fn a_non_interactive_ui_is_never_prompted() {
        let ui = CaptureUi::default();
        *ui.interactive.borrow_mut() = Some(false);
        let (ctx, env, ui) = flow_ctx(FakeInstallEnv::new(Os::Linux).with_programs(&["npm"]), ui);

        assert_eq!(
            offer_install(&ctx, "Tool", &tool_info()).await.unwrap(),
            InstallOutcome::NotOffered
        );
        assert!(
            !wait_for_provider(
                &ctx,
                "Tool",
                ServerProgram::Unknown,
                "tool",
                &serde_json::json!({}),
                "http://x",
                &tool_info()
            )
            .await
            .unwrap()
        );
        assert!(ui.select_prompts.borrow().is_empty());
        assert!(env.runs().is_empty());
        assert!(env.health_checks().is_empty());
        assert!(all_text(&ui).contains("  npm install -g tool"));
    }

    #[tokio::test]
    async fn no_applicable_method_goes_straight_to_manual_instructions() {
        let (ctx, env, ui) = flow_ctx(FakeInstallEnv::new(Os::Linux), CaptureUi::default());

        assert_eq!(
            offer_install(&ctx, "Tool", &tool_info()).await.unwrap(),
            InstallOutcome::Unavailable
        );
        assert!(ui.select_prompts.borrow().is_empty());
        assert!(env.runs().is_empty());
        let text = all_text(&ui);
        assert!(text.contains("  npm install -g tool  (npm not found)"));
        assert!(text.contains("  curl -fsSL https://t.sh | sh  (curl not found)"));
    }

    #[tokio::test]
    async fn a_type_with_no_methods_says_it_cannot_be_installed() {
        let (ctx, _, ui) = flow_ctx(FakeInstallEnv::new(Os::Linux), CaptureUi::default());
        let info = InstallInfo::default()
            .page("https://lmstudio.ai/download")
            .note("Open the app.");
        offer_install(&ctx, "LM Studio", &info).await.unwrap();
        assert_eq!(
            *ui.infos.borrow(),
            vec![
                "LM Studio cannot be installed from granite-cli.",
                "  See https://lmstudio.ai/download",
                "  Open the app.",
            ]
        );
    }

    #[tokio::test]
    async fn install_unattended_runs_the_first_method_even_if_it_may_ask() {
        let info = InstallInfo::default()
            .program("tool")
            .method(
                InstallMethod::new(
                    &[Os::Linux],
                    MethodKind::Script,
                    "curl -fsSL https://sudo.sh | sh",
                )
                .attended(),
            )
            .method(InstallMethod::new(
                &[Os::Linux],
                MethodKind::Npm,
                "npm install -g tool",
            ));
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux)
                .with_programs(&["npm", "curl"])
                .with_run(
                    "curl -fsSL https://sudo.sh | sh",
                    FakeRun::ok().installs("tool", "/x/tool"),
                ),
            CaptureUi::default(),
        );

        let outcome = install_unattended(&ctx, "Tool", &info).await.unwrap();

        assert!(matches!(outcome, InstallOutcome::Installed { .. }));
        assert_eq!(
            env.runs(),
            vec!["curl -fsSL https://sudo.sh | sh".to_string()]
        );
        assert!(ui.select_prompts.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_server_answering_right_after_its_install_is_not_reported_as_not_answering() {
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux).with_health(&[true]),
            CaptureUi::default(),
        );

        let healthy = wait_for_provider(
            &ctx,
            "Tool",
            ServerProgram::Installed,
            "tool",
            &serde_json::json!({}),
            "http://localhost:1",
            &tool_info(),
        )
        .await
        .unwrap();

        assert!(healthy);
        assert_eq!(env.health_checks(), vec!["tool"]);
        assert!(ui.warns.borrow().is_empty());
        assert!(ui.select_prompts.borrow().is_empty());
    }

    #[tokio::test]
    async fn check_again_until_the_provider_is_healthy() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().extend([0, 0]);
        let (ctx, env, ui) = flow_ctx(
            FakeInstallEnv::new(Os::Linux).with_health(&[false, true]),
            ui,
        );

        let healthy = wait_for_provider(
            &ctx,
            "Tool",
            ServerProgram::Unknown,
            "tool",
            &serde_json::json!({}),
            "http://localhost:1",
            &tool_info(),
        )
        .await
        .unwrap();

        assert!(healthy);
        assert_eq!(env.health_checks(), vec!["tool", "tool"]);
        assert!(env.runs().is_empty());
        assert_eq!(
            ui.warns.borrow()[0],
            "Tool is not answering at http://localhost:1."
        );
        assert!(
            ui.warns.borrow()[1].starts_with("Tool is still not answering at http://localhost:1: ")
        );
        let infos = ui.infos.borrow().clone();
        assert_eq!(
            infos[..3],
            [
                "  You can start it in another terminal with:",
                "      tool serve",
                "  systemctl start tool",
            ]
        );
        assert_eq!(ui.select_prompts.borrow()[0].0, WOULD_YOU_LIKE_TO);
    }

    #[test]
    fn the_not_answering_line_says_what_is_known_about_the_program() {
        let env = FakeInstallEnv::new(Os::Linux);
        let path = std::path::PathBuf::from("/usr/bin/tool");
        let cases = [
            (
                ServerProgram::Installed,
                "Tool is installed but not answering at http://localhost:1.",
            ),
            (
                ServerProgram::FoundAt(&path),
                "Tool was found at /usr/bin/tool but is not answering at http://localhost:1.",
            ),
            (
                ServerProgram::Unknown,
                "Tool is not answering at http://localhost:1.",
            ),
        ];
        for (program, expected) in cases {
            let ui = CaptureUi::default();
            show_not_answering(
                &ui,
                &env,
                "Tool",
                program,
                "http://localhost:1",
                &tool_info(),
            );
            assert_eq!(ui.warns.borrow().as_slice(), [expected]);
        }
    }

    /// A caller that answers every prompt with its default must leave the
    /// loop, or it would check again forever.
    #[tokio::test]
    async fn the_wait_prompt_defaults_to_continue_without_it() {
        let (ctx, env, ui) = flow_ctx(FakeInstallEnv::new(Os::Linux), CaptureUi::default());

        let healthy = wait_for_provider(
            &ctx,
            "Tool",
            ServerProgram::Unknown,
            "tool",
            &serde_json::json!({}),
            "http://localhost:1",
            &tool_info(),
        )
        .await
        .unwrap();

        assert!(!healthy);
        let (_, items, default) = ui.select_prompts.borrow()[0].clone();
        assert_eq!(items[default], CONTINUE_WITHOUT);
        assert!(env.health_checks().is_empty());
    }

    #[tokio::test]
    async fn continue_without_it_returns_unhealthy() {
        let ui = CaptureUi::default();
        ui.select_answers.borrow_mut().push_back(1);
        let (ctx, env, _) = flow_ctx(FakeInstallEnv::new(Os::Linux), ui);

        let healthy = wait_for_provider(
            &ctx,
            "Tool",
            ServerProgram::Unknown,
            "tool",
            &serde_json::json!({}),
            "http://localhost:1",
            &tool_info(),
        )
        .await
        .unwrap();

        assert!(!healthy);
        assert!(env.health_checks().is_empty());
    }
}
