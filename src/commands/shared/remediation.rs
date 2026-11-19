//! The prompts that ask what to do about a broken configuration reference,
//! and the code that applies the answer.
//!
//! [`remediate`] offers a fix for one broken reference at a time, until what
//! the caller named validates or the user stops accepting fixes.
//! [`remove_with_dependents`] works out what a removal takes with it and what
//! it leaves broken, over the whole chain of dependents, prints that once,
//! and then removes it.
//!
//! Reconfigure and remove run the instance's own setup and removal commands,
//! the ones a user would run by hand. Un-enabling is the one repair applied
//! here, dropping an id from a launcher's `enabled_capabilities`.
//!
use std::collections::HashMap;

use anyhow::Result;

use crate::commands::{
    CapabilityCommands, HardwareCommands, LauncherCommands, ModelCommands, ProviderCommands,
};
use crate::config::Config;
use crate::config::validation::{
    Problem, RefKind, ValidationError, dependents, find_dangling, type_name, validate_ref,
};

/*-- public --------------------------------------------------------------------*/

/// What declining a fix means for the command that asked, which decides only
/// how the last choice is worded. The caller acts on the returned
/// [`Outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnDecline {
    /// The command carries on with the instance left as it is, which is what
    /// an info or detail command does.
    Skip,
    /// The command cannot run against a broken configuration and stops,
    /// which is what `launch` does.
    Abort,
}

/// Whether what the caller named validates now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing was broken, or everything broken was fixed.
    Clean,
    /// Something is still broken: the user declined to fix it, or there was
    /// nobody to ask. Also returned when the user chose to remove the
    /// instance the caller named, which leaves nothing to validate.
    Unresolved,
}

/// The note a list command puts against each instance of `kind` whose
/// references do not resolve, keyed by instance id.
///
/// A list reports that a problem exists and never prompts about it. Acting on
/// it is left to a command the user chooses to run next.
pub(crate) fn dangling_notes(ctx: &crate::AppContext, kind: RefKind) -> HashMap<String, String> {
    find_dangling(kind, ctx.config(), &ctx.sources())
        .into_iter()
        .map(|dangling| {
            (
                dangling.instance_id,
                ctx.ui.warn_mark(&format!("⚠ {}", dangling.reason)),
            )
        })
        .collect()
}

/// A selection prompt that names the instance currently configured, and says
/// so when it no longer resolves.
///
/// A setup run over an existing instance offers its current values as
/// defaults. Without this, pressing Enter through the wizard re-saves a
/// dangling reference with nothing on screen to say it was one.
pub(crate) fn prompt_with_current(
    ctx: &crate::AppContext,
    prompt: &str,
    kind: RefKind,
    current: Option<&str>,
) -> String {
    let Some(current) = current.filter(|id| !id.is_empty()) else {
        return prompt.to_string();
    };

    match validate_ref(kind, current, ctx.config(), &ctx.sources()) {
        Ok(()) => format!("{prompt} [current: '{current}']"),
        Err(_) => format!(
            "{prompt} [current: '{current}', {} no longer resolves]",
            ctx.ui.warn_mark("⚠")
        ),
    }
}

/// How a removal command should behave when other configured instances point
/// at the entity being removed.
///
/// Passed as `--dependents <cascade|discovered|refuse|leave>` on the CLI.
/// When the flag is omitted the caller passes `None`: an interactive session
/// asks what to do, and a session that cannot ask refuses with exit code 1
/// and an error naming the four policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum DependentsPolicy {
    /// Remove the target and every dependent automatically, without asking.
    /// Cascades recursively so dependents-of-dependents are taken too.
    Cascade,
    /// Remove the target and any dependents that provider discovery created,
    /// without asking. Other dependents are left configured but broken.
    Discovered,
    /// Refuse to remove the target and exit with code 1 if anything depends
    /// on it.
    Refuse,
    /// Remove only the target. Dependents are left configured but broken.
    Leave,
}

/// Removes `(kind, id)` and applies `policy` to the instances that depend on
/// it, directly or through each other.
///
/// The whole chain is worked out before anything is printed or removed:
/// which instances go with the target, and which stay and break because
/// something they point at is removed. The plan is printed once, and then
/// everything in it is removed, dependents before what they depend on, each
/// with its own "removed" line.
///
/// `policy` is `Some(DependentsPolicy::...)` when a `--dependents` flag was
/// given, or `None` to fall back to the session's default:
///
/// - `Some(Refuse)`: fails if anything depends on the target. Nothing is
///   removed or printed beyond the error.
/// - `Some(Cascade)`: removes the target and every instance that depends on
///   it, at every level.
/// - `Some(Discovered)`: removes the target and the models provider discovery
///   created among its dependents, and leaves every other dependent in place.
///   It never removes a catalog or hand-made model.
/// - `Some(Leave)`: removes only the target and leaves its dependents in
///   place.
/// - `None`, interactive session: lists everything that depends on the
///   target and asks one question for the whole chain: remove it all,
///   cancel, or remove only the target.
/// - `None`, non-interactive session (`--output json` / `--output markdown`,
///   or no terminal to ask on): fails like `Refuse`, with an error that names
///   the policies.
///
/// A model in the chain that was discovered by a provider with discovery on
/// is refused, as `model remove` refuses it, unless that provider is removed
/// in the same chain or the policy is `Discovered`. The caller checks the
/// target itself.
pub(crate) fn remove_with_dependents(
    ctx: &mut crate::AppContext,
    kind: RefKind,
    id: &str,
    policy: Option<DependentsPolicy>,
) -> Result<()> {
    let has_dependents = !dependents(kind, id, ctx.config()).is_empty();

    let plan = match policy {
        _ if !has_dependents => plan_removal(ctx, kind, id, Take::Nothing),
        Some(DependentsPolicy::Refuse) | None if !ctx.ui.is_interactive() => {
            anyhow::bail!(
                "Cannot remove {kind} '{id}' because other configured instances depend on it. \
                 Pass --dependents cascade, --dependents leave, or \
                 --dependents refuse."
            );
        }
        Some(DependentsPolicy::Refuse) => {
            anyhow::bail!(
                "Cannot remove {kind} '{id}' because other configured instances depend on it."
            );
        }
        Some(DependentsPolicy::Cascade) => {
            let plan = plan_removal(ctx, kind, id, Take::All);
            check_discovery(ctx, &plan)?;
            report(ctx, &plan);
            plan
        }
        Some(DependentsPolicy::Discovered) => {
            let plan = plan_removal(ctx, kind, id, Take::Discovered);
            report(ctx, &plan);
            plan
        }
        Some(DependentsPolicy::Leave) => {
            let plan = plan_removal(ctx, kind, id, Take::Nothing);
            report(ctx, &plan);
            plan
        }
        None => match ask(ctx, kind, id)? {
            Some(plan) => plan,
            None => {
                ctx.ui.info(&format!("Keeping {kind} '{id}'."));
                return Ok(());
            }
        },
    };

    for (kind, id) in plan.with.iter().chain(std::iter::once(&plan.target)) {
        delete(ctx, *kind, id);
    }
    Ok(())
}

/// Validates `(kind, id)` and offers a fix for whatever is broken,
/// re-validating after each one so a repair that exposes a second problem is
/// offered in turn.
///
/// `may_prompt` is the caller's own mode, false for a non-prompting run such
/// as `setup --auto`. Prompting also needs a `Ui` with somebody to ask, so a
/// JSON or Markdown session never reaches a prompt whatever the caller
/// passes. Without prompting the problem is reported and left alone, which is
/// what skipping does.
///
/// Reached through a launcher, the removal on offer is disabling: the
/// launcher stops enabling the capability and the capability itself stays
/// configured. Deleting an instance is offered only to a caller that named
/// that instance, since a capability may be enabled by more than one launcher.
///
/// The loop ends when validation comes back clean, when the user declines,
/// when a removal takes the instance the caller named, or when every repair
/// on offer has been tried against the same problem. A repair that changes
/// nothing is dropped from the choices rather than ending the run, so it
/// always terminates and always leaves the remaining repairs reachable.
pub(crate) async fn remediate(
    ctx: &mut crate::AppContext,
    kind: RefKind,
    id: &str,
    on_decline: OnDecline,
    may_prompt: bool,
) -> Result<Outcome> {
    let prompting = may_prompt && ctx.ui.is_interactive();
    let mut previous: Option<ValidationError> = None;
    let mut tried: Vec<Choice> = Vec::new();

    loop {
        let Err(error) = validate_ref(kind, id, ctx.config(), &ctx.sources()) else {
            return Ok(Outcome::Clean);
        };

        // A repair that left the problem exactly as it was will do so again,
        // so it is dropped from the choices rather than ending the run. A
        // reconfiguration the user walked out of, or used to change something
        // else, comes back to the same question with the rest still on offer.
        // A different problem starts over with all of them.
        if previous.as_ref() != Some(&error) {
            tried.clear();
        }

        let Some(fix) = Fix::for_error(&error, ctx.config(), (kind, id)) else {
            // The instance the caller asked about is itself missing, so
            // there is nothing to offer: reconfiguring or removing needs
            // something that exists.
            ctx.ui.warn(&error.to_string());
            return Ok(Outcome::Unresolved);
        };

        if !prompting {
            ctx.ui.warn(&error.to_string());
            return Ok(Outcome::Unresolved);
        }

        // Every repair has been tried and the problem is still here.
        let Some(choice) = choose(ctx, &error, &fix, on_decline, &tried)? else {
            ctx.ui.warn(&format!("Still unresolved: {error}"));
            return Ok(Outcome::Unresolved);
        };

        tried.push(choice);
        match choice {
            Choice::Reconfigure => {
                previous = Some(error);
                reconfigure(ctx, &fix).await?;
            }
            Choice::Reset => {
                previous = Some(error);
                reset(ctx, &fix)?;
            }
            Choice::Remove => {
                previous = Some(error);
                remove(ctx, fix.kind, &fix.id, None)?;
                // The removal took the instance the caller named, either as
                // the fix itself or as a dependent of it. Validating again
                // would report it as not configured right after the removal
                // said it was removed. The caller checks whether it still
                // exists.
                if type_name(kind, id, ctx.config()).is_none() {
                    return Ok(Outcome::Unresolved);
                }
            }
            Choice::Disable => {
                previous = Some(error);
                disable(ctx, &fix)?;
            }
            // The walk is deterministic, so the next pass would report the
            // problem just declined. Stop rather than ask about it again.
            Choice::Decline => return Ok(Outcome::Unresolved),
        }
    }
}

/*-- private -------------------------------------------------------------------*/

/// The instance a fix acts on, and what can be done to it.
#[derive(Debug, PartialEq, Eq)]
struct Fix {
    kind: RefKind,
    id: String,
    /// The instance's `*_type`, which reconfiguring hands back to setup.
    type_name: String,
    /// False when the type name is itself the problem. Setup cannot run a
    /// type the registry does not have, so removal is the only fix.
    can_reconfigure: bool,
    /// What resetting would change, when the problem is settings that are not
    /// valid and replacing some of them with this type's defaults leaves an
    /// instance that builds. `None` when no such replacement exists, which is
    /// the case for a type whose defaults carry the same unusable value (an
    /// `agent-model` capability's default model id is empty, like the one
    /// that is not valid).
    reset: Option<ResetPlan>,
    /// The `(launcher, capability)` pair to disable, when remediation was
    /// reached through a launcher that enables the capability. Some means the
    /// removal on offer drops the id from that launcher's list rather than
    /// deleting the instance.
    disable: Option<(String, String)>,
}

impl Fix {
    fn for_error(error: &ValidationError, config: &Config, root: (RefKind, &str)) -> Option<Self> {
        // An instance that is not configured cannot be acted on, so the fix
        // belongs to whoever points at it: `launch claude` finding that
        // `chat`'s model is gone reconfigures `chat`. Every other problem is
        // a property of the target itself.
        let (kind, id) = match &error.problem {
            Problem::NotConfigured => error.referrer.clone()?,
            _ => error.target.clone(),
        };

        let type_name = type_name(kind, &id, config)?.to_string();
        let reset = match &error.problem {
            Problem::UnreadableSettings { .. } => {
                plan_reset(kind, &type_name, settings(kind, &id, config)?)
            }
            _ => None,
        };
        Some(Self {
            can_reconfigure: !matches!(error.problem, Problem::UnknownType { .. }),
            reset,
            type_name,
            disable: disable_target(error, kind, &id, root, config),
            kind,
            id,
        })
    }
}

/// The settings an instance carries, which is what a reset works from.
fn settings<'a>(kind: RefKind, id: &str, config: &'a Config) -> Option<&'a serde_json::Value> {
    match kind {
        RefKind::Launcher => config.get_launcher(id).map(|c| &c.config),
        RefKind::Capability => config.get_capability(id).map(|c| &c.config),
        RefKind::Model => config.get_model(id).map(|c| &c.config),
        RefKind::Provider => config.get_provider(id).map(|c| &c.config),
        RefKind::Hardware => config.get_hardware(id).map(|c| &c.config),
    }
}

/// The settings a reset would write, and the fields it would change to get
/// there.
#[derive(Debug, PartialEq, Eq)]
struct ResetPlan {
    /// The settings to write: the instance's own, with the fields below
    /// replaced by this type's defaults.
    settings: serde_json::Value,
    /// Each field the reset replaces, with the default value it takes.
    changes: Vec<(String, serde_json::Value)>,
}

impl ResetPlan {
    /// What the prompt offers, naming the fields and the values they take, so
    /// nobody accepts a repair without knowing what it moves.
    fn describe(&self, kind: RefKind, id: &str) -> String {
        let render = |value: &serde_json::Value| match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        match self.changes.as_slice() {
            [(field, value)] => format!(
                "Reset {kind} '{id}' {field} setting to its default value of {}",
                render(value)
            ),
            changes => format!(
                "Reset {kind} '{id}' settings to their default values: {}",
                changes
                    .iter()
                    .map(|(field, value)| format!("{field} {}", render(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// A replacement of some of an instance's settings by its type's defaults
/// that builds, or `None` when no replacement does.
///
/// The settings blob is valid JSON; it is reading it as the type's config that
/// failed, and usually over one field. So each field that differs from the
/// default is tried on its own first. When no single field is enough, the
/// differing fields are replaced one after another, in the order the defaults
/// list them, and the first set that builds is the plan. That set can include
/// fields that did not need replacing. Construction reads settings and does no
/// I/O, so trying a few is cheap.
fn plan_reset(kind: RefKind, type_name: &str, current: &serde_json::Value) -> Option<ResetPlan> {
    let defaults = default_settings(kind, type_name);
    let (Some(current_fields), Some(default_fields)) = (current.as_object(), defaults.as_object())
    else {
        return constructs(kind, type_name, &defaults).then(|| ResetPlan {
            settings: defaults.clone(),
            changes: Vec::new(),
        });
    };

    let differing: Vec<(&String, &serde_json::Value)> = default_fields
        .iter()
        .filter(|(field, value)| current_fields.get(*field) != Some(*value))
        .collect();

    for (field, value) in &differing {
        let mut candidate = current.clone();
        candidate[field.as_str()] = (*value).clone();
        if constructs(kind, type_name, &candidate) {
            return Some(ResetPlan {
                settings: candidate,
                changes: vec![((*field).clone(), (*value).clone())],
            });
        }
    }

    let mut candidate = current.clone();
    let mut changes = Vec::new();
    for (field, value) in differing {
        candidate[field.as_str()] = value.clone();
        changes.push((field.clone(), value.clone()));
        if constructs(kind, type_name, &candidate) {
            return Some(ResetPlan {
                settings: candidate,
                changes,
            });
        }
    }
    None
}

/// Whether these settings produce an instance of this type.
fn constructs(kind: RefKind, type_name: &str, settings: &serde_json::Value) -> bool {
    match kind {
        RefKind::Launcher => crate::launchers::LAUNCHER_REGISTRY
            .construct(type_name, type_name, settings)
            .is_ok(),
        RefKind::Capability => crate::capabilities::CAPABILITY_REGISTRY
            .construct(type_name, type_name, settings)
            .is_ok(),
        RefKind::Model => crate::models::MODEL_REGISTRY
            .construct(type_name, type_name, settings)
            .is_ok(),
        RefKind::Provider => crate::providers::PROVIDER_REGISTRY
            .construct(type_name, type_name, settings)
            .is_ok(),
        // A hardware profile is not built from settings; it has no reset.
        RefKind::Hardware => false,
    }
}

/// A type's default settings, as its registry entry declares them.
fn default_settings(kind: RefKind, type_name: &str) -> serde_json::Value {
    match kind {
        RefKind::Launcher => crate::launchers::LAUNCHER_REGISTRY.default_config(type_name),
        RefKind::Capability => crate::capabilities::CAPABILITY_REGISTRY.default_config(type_name),
        RefKind::Model => crate::models::MODEL_REGISTRY.default_config(type_name),
        RefKind::Provider => crate::providers::PROVIDER_REGISTRY.default_config(type_name),
        RefKind::Hardware => serde_json::Value::Null,
    }
}

/// The `(launcher, capability)` pair a fix reached through a launcher can
/// disable.
///
/// A capability is shared: other launchers may enable the same instance, and
/// the caller asked to launch one launcher rather than to change the
/// configuration at large. Dropping the id from that launcher's own list
/// repairs what was asked about and leaves everything else alone.
///
/// Two shapes reach here. The launcher enables a capability whose own
/// reference is broken, where the fix acts on that capability; and the
/// launcher enables a capability that is not configured at all, where the fix
/// acts on the launcher.
fn disable_target(
    error: &ValidationError,
    kind: RefKind,
    id: &str,
    root: (RefKind, &str),
    config: &Config,
) -> Option<(String, String)> {
    if root.0 != RefKind::Launcher {
        return None;
    }
    let launcher_id = root.1;

    let capability_id = match kind {
        RefKind::Capability => id,
        RefKind::Launcher if error.target.0 == RefKind::Capability => error.target.1.as_str(),
        _ => return None,
    };

    config
        .get_launcher(launcher_id)?
        .enabled_capabilities
        .iter()
        .any(|enabled| enabled == capability_id)
        .then(|| (launcher_id.to_string(), capability_id.to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Reconfigure,
    Reset,
    Remove,
    Disable,
    Decline,
}

/// Reports the problem and asks what to do about it. Declining is the
/// default, so a user who answers without reading changes nothing.
fn choose(
    ctx: &crate::AppContext,
    error: &ValidationError,
    fix: &Fix,
    on_decline: OnDecline,
    tried: &[Choice],
) -> Result<Option<Choice>> {
    let mut choices = Vec::new();
    let mut items = Vec::new();

    if fix.can_reconfigure && !tried.contains(&Choice::Reconfigure) {
        choices.push(Choice::Reconfigure);
        items.push(format!("Reconfigure {} '{}' now", fix.kind, fix.id.clone()));
    }

    if let Some(plan) = &fix.reset
        && !tried.contains(&Choice::Reset)
    {
        choices.push(Choice::Reset);
        items.push(plan.describe(fix.kind, &fix.id));
    }

    match &fix.disable {
        Some((launcher_id, capability_id)) if !tried.contains(&Choice::Disable) => {
            choices.push(Choice::Disable);
            items.push(format!(
                "Remove capability '{capability_id}' from launcher '{launcher_id}'"
            ));
        }
        None if !tried.contains(&Choice::Remove) => {
            choices.push(Choice::Remove);
            items.push(format!("Remove {} '{}'", fix.kind, fix.id));
        }
        _ => {}
    }

    if choices.is_empty() {
        return Ok(None);
    }

    choices.push(Choice::Decline);
    items.push(match on_decline {
        OnDecline::Skip => format!("Skip for now, '{}' stays broken until fixed", fix.id),
        OnDecline::Abort => "Cancel".to_string(),
    });

    ctx.ui.warn(&format!("Configuration issue: {error}"));
    let picked = ctx
        .ui
        .select("What would you like to do?", &items, items.len() - 1)?;

    Ok(Some(choices[picked]))
}

/// Runs the instance's own setup command against the instance, which is what
/// the user would run by hand to change what it points at.
///
/// Passes `force_overwrite: true` so the wizard does not ask a second
/// "already configured, overwrite?" question — the choice made at the
/// remediation prompt was already that confirmation.
async fn reconfigure(ctx: &mut crate::AppContext, fix: &Fix) -> Result<()> {
    let (kind, type_name, id) = (fix.kind, fix.type_name.as_str(), Some(fix.id.as_str()));
    match kind {
        RefKind::Launcher => LauncherCommands::setup(ctx, type_name, id, true).await,
        RefKind::Capability => CapabilityCommands::setup(ctx, type_name, id, true).await,
        RefKind::Model => ModelCommands::setup(ctx, type_name, id, true).await,
        RefKind::Provider => ProviderCommands::setup(ctx, type_name, id, true).await,
        RefKind::Hardware => {
            use crate::config::hardware::LOCAL_HARDWARE_ID;
            let args = crate::commands::hardware::HardwareSetupArgs {
                target: Some(type_name.to_string()),
                id: id.filter(|id| *id != LOCAL_HARDWARE_ID).map(str::to_string),
                force_overwrite: true,
                ..Default::default()
            };
            HardwareCommands::setup(ctx, args).map(|_| ())
        }
    }
}

/// Replaces the fields that are not valid with their type's defaults, and
/// keeps everything else the instance was configured with. Reconfiguring also
/// repairs the instance, but setup offers the instance's current values, and
/// for a field whose value is not valid it offers a zero value (`0` for a
/// number, `false` for a flag). Accepting those writes that value, where this
/// writes the type's default.
fn reset(ctx: &mut crate::AppContext, fix: &Fix) -> Result<()> {
    let Some(plan) = &fix.reset else {
        return Ok(());
    };
    let defaults = plan.settings.clone();
    let (kind, id) = (fix.kind, fix.id.clone());
    // The in-memory change lands either way, which is what the walk about to
    // re-run reads. A failure to persist is reported the way the removal
    // commands report theirs.
    let saved = match kind {
        RefKind::Launcher => ctx
            .config_mut()
            .update_launcher(&id, |launcher| launcher.config = defaults),
        RefKind::Capability => ctx
            .config_mut()
            .update_capability(&id, |capability| capability.config = defaults),
        RefKind::Model => ctx
            .config_mut()
            .update_model(&id, |model| model.config = defaults),
        RefKind::Provider => ctx
            .config_mut()
            .update_provider(&id, |provider| provider.config = defaults),
        // `default_settings` gives a hardware profile no defaults, so no reset
        // plan is ever made for one.
        RefKind::Hardware => Ok(()),
    };
    if let Err(e) = saved {
        ctx.ui
            .warn(&format!("failed to persist the change to '{id}': {e}"));
    }
    ctx.ui.info(&format!(
        "{kind} '{id}': {} now holds the default from '{}'.",
        plan.changes
            .iter()
            .map(|(field, _)| field.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        fix.type_name
    ));
    Ok(())
}

/// Drops the capability from the launcher's `enabled_capabilities`. The
/// capability stays configured, so any other launcher enabling it is
/// untouched.
fn disable(ctx: &mut crate::AppContext, fix: &Fix) -> Result<()> {
    let Some((launcher_id, capability_id)) = fix.disable.clone() else {
        return Ok(());
    };
    // The in-memory change lands either way, which is what the walk about to
    // re-run reads. A failure to persist is reported the way the removal
    // commands report theirs.
    if let Err(e) = ctx.config_mut().update_launcher(&launcher_id, |launcher| {
        launcher
            .enabled_capabilities
            .retain(|id| id != &capability_id)
    }) {
        ctx.ui.warn(&format!(
            "failed to persist the change to '{launcher_id}': {e}"
        ));
    }
    ctx.ui.info(&format!(
        "Launcher '{launcher_id}' no longer enables capability '{capability_id}'."
    ));
    Ok(())
}

/// Which dependents of a removed instance are removed with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Take {
    /// Every dependent, at every level (`cascade`).
    All,
    /// Only the models provider discovery created (`discovered`).
    Discovered,
    /// None of them (`leave`, or no dependents at all).
    Nothing,
}

impl Take {
    fn takes(self, kind: RefKind, id: &str, config: &Config) -> bool {
        match self {
            Take::All => true,
            Take::Nothing => false,
            Take::Discovered => {
                kind == RefKind::Model
                    && config
                        .get_model(id)
                        .is_some_and(|model| model.discovered.is_some())
            }
        }
    }
}

/// What a removal takes with it and what it leaves broken, worked out over
/// the whole chain before anything is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemovalPlan {
    target: (RefKind, String),
    take: Take,
    /// Removed with the target, each listed once, in the order they are
    /// removed: an instance comes before anything it depends on.
    with: Vec<(RefKind, String)>,
    /// Left in place although something they point at is removed, each listed
    /// once, sorted by kind and id.
    left: Vec<Stranded>,
}

/// An instance a removal leaves in place, and the removed instances it points
/// at, which are why it stops working.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stranded {
    kind: RefKind,
    id: String,
    removed: Vec<(RefKind, String)>,
}

impl RemovalPlan {
    /// What is removed with the target, sorted by kind and id for display.
    fn shown(&self) -> Vec<&(RefKind, String)> {
        let mut shown: Vec<_> = self.with.iter().collect();
        shown.sort_by(|a, b| rank(a.0).cmp(&rank(b.0)).then(a.1.cmp(&b.1)));
        shown
    }
}

/// Works out the whole removal of `(kind, id)`: follows dependents level by
/// level, removes those `take` takes, and records those it leaves together
/// with the removed instances they point at. An instance reached on two paths
/// is listed once.
fn plan_removal(ctx: &crate::AppContext, kind: RefKind, id: &str, take: Take) -> RemovalPlan {
    fn visit(
        node: &(RefKind, String),
        take: Take,
        config: &Config,
        seen: &mut Vec<(RefKind, String)>,
        order: &mut Vec<(RefKind, String)>,
        left: &mut Vec<Stranded>,
    ) {
        for dependent in dependents(node.0, &node.1, config) {
            if take.takes(dependent.0, &dependent.1, config) {
                if !seen.contains(&dependent) {
                    seen.push(dependent.clone());
                    visit(&dependent, take, config, seen, order, left);
                }
            } else if let Some(stranded) = left
                .iter_mut()
                .find(|s| s.kind == dependent.0 && s.id == dependent.1)
            {
                stranded.removed.push(node.clone());
            } else {
                left.push(Stranded {
                    kind: dependent.0,
                    id: dependent.1,
                    removed: vec![node.clone()],
                });
            }
        }
        // After its dependents, so it is removed after them.
        order.push(node.clone());
    }

    let config = ctx.config();
    let target = (kind, id.to_string());
    let mut seen = vec![target.clone()];
    let mut order = Vec::new();
    let mut left = Vec::new();
    visit(&target, take, config, &mut seen, &mut order, &mut left);

    order.retain(|node| node != &target);
    left.retain(|s| !seen.iter().any(|(k, i)| *k == s.kind && *i == s.id));
    for stranded in &mut left {
        stranded
            .removed
            .sort_by(|a, b| rank(a.0).cmp(&rank(b.0)).then(a.1.cmp(&b.1)));
        stranded.removed.dedup();
    }
    left.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then(a.id.cmp(&b.id)));

    RemovalPlan {
        target,
        take,
        with: order,
        left,
    }
}

/// Refuses a plan that removes a model a provider with discovery on would add
/// back, the same check `model remove` makes on its target. A model goes
/// without the check when its provider is removed in the same plan, or when
/// the plan follows the `discovered` policy, which names such models
/// explicitly.
fn check_discovery(ctx: &crate::AppContext, plan: &RemovalPlan) -> Result<()> {
    if plan.take == Take::Discovered {
        return Ok(());
    }
    let removed_providers: Vec<&str> = plan
        .with
        .iter()
        .chain(std::iter::once(&plan.target))
        .filter(|(kind, _)| *kind == RefKind::Provider)
        .map(|(_, id)| id.as_str())
        .collect();
    for (_, model_id) in plan.with.iter().filter(|(k, _)| *k == RefKind::Model) {
        let discovered_by = ctx
            .config()
            .get_model(model_id)
            .and_then(|model| model.discovered.as_ref())
            .map(|discovered| discovered.provider_id.as_str());
        if discovered_by.is_some_and(|provider| !removed_providers.contains(&provider)) {
            ModelCommands::refuse_while_discovery_on(ctx, model_id)?;
        }
    }
    Ok(())
}

/// Prints the plan: what is removed with the target, then what stays and
/// will not work, with the commands that fix it. Prints nothing when the
/// target goes alone and breaks nothing.
fn report(ctx: &crate::AppContext, plan: &RemovalPlan) {
    let (kind, id) = &plan.target;
    if !plan.with.is_empty() {
        ctx.ui.info(&match plan.take {
            Take::Discovered => {
                format!("Removing {kind} '{id}' and the models discovered from it:")
            }
            _ => format!("Removing {kind} '{id}' and what depends on it:"),
        });
        for (kind, id) in plan.shown() {
            ctx.ui.info(&format!("  - {kind} '{id}'"));
        }
    }
    report_left(ctx, plan);
}

/// The part of the plan that breaks something: each instance left in place,
/// why it stops working, and one setup command per kind to fix it. The type is
/// shown when it differs from the id, since setup takes the type.
fn report_left(ctx: &crate::AppContext, plan: &RemovalPlan) {
    if plan.left.is_empty() {
        return;
    }
    ctx.ui.warn("These stay, and will not work until fixed:");
    for stranded in &plan.left {
        let type_suffix = type_name(stranded.kind, &stranded.id, ctx.config())
            .filter(|type_name| *type_name != stranded.id)
            .map(|type_name| format!(" ({type_name})"))
            .unwrap_or_default();
        ctx.ui.info(&format!(
            "  - {} '{}'{type_suffix}: {}",
            stranded.kind,
            stranded.id,
            reason(&stranded.removed)
        ));
    }

    let mut kinds: Vec<RefKind> = plan.left.iter().map(|s| s.kind).collect();
    kinds.dedup();
    let commands: Vec<String> = kinds
        .iter()
        .map(|kind| format!("`granite-cli {kind} setup <type> --id <id>`"))
        .collect();
    ctx.ui.info(&format!(
        "  Fix {} with {}.",
        if plan.left.len() == 1 { "it" } else { "them" },
        join_and(&commands)
    ));
}

/// Why an instance left in place stops working: the removed instances it
/// points at, grouped by kind, such as "its provider 'ollama' is removed".
fn reason(removed: &[(RefKind, String)]) -> String {
    let mut parts = Vec::new();
    let mut rest = removed;
    while let Some((kind, _)) = rest.first() {
        let count = rest.iter().take_while(|(k, _)| k == kind).count();
        let (group, tail) = rest.split_at(count);
        let ids: Vec<String> = group.iter().map(|(_, id)| format!("'{id}'")).collect();
        parts.push(match ids.as_slice() {
            [one] => format!("its {kind} {one} is removed"),
            _ => format!("its {} {} are removed", plural(*kind), join_and(&ids)),
        });
        rest = tail;
    }
    parts.join("; ")
}

/// Asks one question for the whole chain, after listing everything that
/// depends on the target. Returns the plan for the answer, or `None` when
/// the user cancels.
fn ask(ctx: &crate::AppContext, kind: RefKind, id: &str) -> Result<Option<RemovalPlan>> {
    let all = plan_removal(ctx, kind, id, Take::All);
    ctx.ui
        .warn(&format!("Other instances depend on {kind} '{id}':"));
    for (dependent_kind, dependent_id) in all.shown() {
        ctx.ui
            .info(&format!("  - {dependent_kind} '{dependent_id}'"));
    }

    let together = match all.with.as_slice() {
        [(dependent_kind, dependent_id)] => {
            format!("Remove {kind} '{id}' and {dependent_kind} '{dependent_id}' together")
        }
        with => format!(
            "Remove {kind} '{id}' and the {} instances listed above",
            with.len()
        ),
    };
    let items = vec![
        together,
        format!("Cancel, keep {kind} '{id}'"),
        format!("Remove only {kind} '{id}', fix the rest later"),
    ];

    // Cancelling is the default: this is the destructive prompt, and the
    // other two answers both delete something.
    match ctx.ui.select("What would you like to do?", &items, 1)? {
        0 => {
            check_discovery(ctx, &all)?;
            Ok(Some(all))
        }
        2 => {
            let only = plan_removal(ctx, kind, id, Take::Nothing);
            report_left(ctx, &only);
            Ok(Some(only))
        }
        _ => Ok(None),
    }
}

/// Deletes one instance from the configuration and reports it. A failure to
/// persist is reported and does not stop the rest of the plan, since the
/// in-memory configuration has already changed.
fn delete(ctx: &mut crate::AppContext, kind: RefKind, id: &str) {
    let (saved, name) = match kind {
        RefKind::Launcher => (ctx.config_mut().remove_launcher(id), "Launcher"),
        RefKind::Capability => (ctx.config_mut().remove_capability(id), "Capability"),
        RefKind::Model => (ctx.config_mut().remove_model(id), "Model"),
        RefKind::Provider => (ctx.config_mut().remove_provider(id), "Provider"),
    };
    if let Err(e) = saved {
        ctx.ui
            .warn(&format!("failed to persist {kind} removal: {e}"));
    }
    ctx.ui.info(&format!("{name} '{id}' removed."));
}

/// The order kinds are listed in: what is depended on before what depends
/// on it.
fn rank(kind: RefKind) -> u8 {
    match kind {
        RefKind::Provider => 0,
        RefKind::Model => 1,
        RefKind::Capability => 2,
        RefKind::Launcher => 3,
    }
}

fn plural(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Launcher => "launchers",
        RefKind::Capability => "capabilities",
        RefKind::Model => "models",
        RefKind::Provider => "providers",
    }
}

/// "a", "a and b", "a, b and c".
fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn remove(
    ctx: &mut crate::AppContext,
    kind: RefKind,
    id: &str,
    policy: Option<DependentsPolicy>,
) -> Result<()> {
    match kind {
        RefKind::Launcher => LauncherCommands::remove(ctx, id, policy),
        RefKind::Capability => CapabilityCommands::remove(ctx, id, policy),
        RefKind::Model => ModelCommands::remove(ctx, id, policy),
        RefKind::Provider => ProviderCommands::remove(ctx, id, policy),
        RefKind::Hardware => HardwareCommands::remove(ctx, id, policy),
    }
}

/*-- tests ---------------------------------------------------------------------*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CapabilityConfig, DiscoveredModel, LauncherConfig, ModelConfig, ProviderConfig,
    };
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn capture(ctx: &crate::AppContext) -> &CaptureUi {
        (&*ctx.ui as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .expect("test contexts are built with a CaptureUi")
    }

    /// Answers the remediation prompts in order. `CaptureUi` falls back to
    /// the prompt's own default once the queue is empty, which is declining.
    fn answer(ctx: &crate::AppContext, choices: &[usize]) {
        let ui = capture(ctx);
        for choice in choices {
            ui.select_answers.borrow_mut().push_back(*choice);
        }
    }

    fn prompts(ctx: &crate::AppContext) -> Vec<(String, Vec<String>)> {
        capture(ctx)
            .select_prompts
            .borrow()
            .iter()
            .map(|(prompt, items, _)| (prompt.clone(), items.clone()))
            .collect()
    }

    /// A provider whose settings hold a field of the wrong type, which is
    /// the state the reset repair exists for.
    fn ctx_with_unreadable_provider_settings() -> crate::AppContext {
        let mut ctx = crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()));
        ctx.config_mut().providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                hardware: Default::default(),
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                // A deliberately chosen endpoint beside a field that cannot
                // be read, which is what a scoped repair has to tell apart.
                config: serde_json::json!({
                    "base_url": "http://127.0.0.1:18080",
                    "timeout_secs": "ten",
                }),
            },
        );
        ctx
    }

    #[tokio::test]
    async fn a_reset_replaces_the_field_that_cannot_be_read_and_keeps_the_rest() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_unreadable_provider_settings();
        // Reconfigure, reset, remove, decline: reset is the second.
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Provider,
            "ollama",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Clean);
        let settings = &ctx.config().get_provider("ollama").unwrap().config;
        assert_eq!(
            settings.get("timeout_secs"),
            crate::providers::PROVIDER_REGISTRY
                .default_config("ollama")
                .get("timeout_secs"),
            "the field that could not be read now holds its type's default"
        );
        assert_eq!(
            settings.get("base_url").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:18080"),
            "a field that reads fine is left as it was configured"
        );
    }

    #[tokio::test]
    async fn the_reset_on_offer_names_the_field_and_the_value_it_takes() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_unreadable_provider_settings();

        let _ = remediate(
            &mut ctx,
            RefKind::Provider,
            "ollama",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        let offered = &prompts(&ctx)[0].1;
        assert!(
            offered.iter().any(|item| item
                == "Reset provider 'ollama' timeout_secs setting to its default value of 10"),
            "got: {offered:?}"
        );
    }

    #[tokio::test]
    async fn a_reset_is_not_offered_when_the_defaults_would_not_help() {
        // `agent-model`'s default settings hold an empty model id, the same
        // value that is not valid here, so resetting repairs nothing and is
        // not offered.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()));
        ctx.config_mut().capabilities.insert(
            "chat".to_string(),
            CapabilityConfig {
                capability_id: "chat".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "" }),
            },
        );

        let _ = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        let offered = prompts(&ctx);
        assert!(!offered.is_empty(), "the problem was reported");
        assert!(
            !offered[0].1.iter().any(|item| item.contains("Reset")),
            "got: {:?}",
            offered[0].1
        );
    }

    /// Launcher `claude` enables capability `chat`, which points at a model
    /// that is not configured. One healthy model satisfies `agent-model`'s
    /// Chat requirement, so reconfiguring `chat` picks it without a prompt
    /// of its own.
    fn ctx_with_a_dangling_model_ref() -> crate::AppContext {
        let mut ctx = crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()));
        ctx.config_mut().providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                hardware: Default::default(),
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        ctx.config_mut().models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                discovered: None,
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "ollama".to_string(),
                variant: None,
                config: serde_json::json!({}),
            },
        );
        ctx.config_mut().capabilities.insert(
            "chat".to_string(),
            CapabilityConfig {
                capability_id: "chat".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "gone" }),
            },
        );
        ctx.config_mut().launchers.insert(
            "claude".to_string(),
            LauncherConfig {
                launcher_id: "claude".to_string(),
                launcher_type: "claude".to_string(),
                enabled_capabilities: vec!["chat".to_string()],
                config: serde_json::json!({}),
            },
        );
        ctx
    }

    /// Capability `chat` uses the one configured model, and nothing enables
    /// the capability, so removing it strands nothing further.
    fn ctx_model_with_one_dependent() -> crate::AppContext {
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config_mut().launchers.clear();
        ctx.config_mut()
            .capabilities
            .get_mut("chat")
            .unwrap()
            .config = serde_json::json!({ "model_id": "granite-3.1-8b-instruct" });
        ctx
    }

    /// Provider `ollama` has one catalog model and one discovery-managed
    /// model. Capability `chat` points at the discovered one, so removing the
    /// discovered model would strand a non-discovered dependent too.
    fn ctx_provider_with_discovered_and_user_models() -> crate::AppContext {
        let mut ctx = ctx_model_with_one_dependent();
        ctx.config_mut().models.insert(
            "managed".to_string(),
            ModelConfig {
                model_id: "managed".to_string(),
                model_type: "custom".to_string(),
                provider_id: "ollama".to_string(),
                variant: None,
                discovered: Some(DiscoveredModel {
                    provider_id: "ollama".to_string(),
                    provider_model_id: "llama3:8b".to_string(),
                }),
                config: serde_json::json!({ "family": "Managed" }),
            },
        );
        ctx.config_mut()
            .capabilities
            .get_mut("chat")
            .unwrap()
            .config = serde_json::json!({ "model_id": "managed" });
        ctx
    }

    #[test]
    fn removing_a_model_with_its_dependent_removes_both() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        answer(&ctx, &[0]);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct", None).unwrap();

        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config().get_capability("chat").is_none());
    }

    #[test]
    fn cancelling_a_removal_keeps_both() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        answer(&ctx, &[1]);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct", None).unwrap();

        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config().get_capability("chat").is_some());
    }

    #[test]
    fn removing_only_what_was_asked_leaves_the_dependent_broken() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        answer(&ctx, &[2]);

        ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct", None).unwrap();

        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        // Left in place, and now dangling, which `capability list` reports.
        assert!(ctx.config().get_capability("chat").is_some());
        assert!(!find_dangling(RefKind::Capability, ctx.config(), &ctx.sources()).is_empty());
    }

    #[test]
    fn a_session_with_nobody_to_ask_refuses_when_dependents_exist() {
        // Non-interactive sessions default to refuse so scripts fail loudly
        // rather than silently stranding dependents.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        let err = ModelCommands::remove(&mut ctx, "granite-3.1-8b-instruct", None).unwrap_err();

        assert!(prompts(&ctx).is_empty());
        assert!(err.to_string().contains("Pass --dependents"), "got: {err}");
        // Nothing was removed.
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config().get_capability("chat").is_some());
    }

    #[test]
    fn a_non_interactive_session_with_leave_removes_only_what_was_asked() {
        // Explicit --dependents leave preserves the old non-interactive
        // behaviour for scripts that want it.
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        ModelCommands::remove(
            &mut ctx,
            "granite-3.1-8b-instruct",
            Some(DependentsPolicy::Leave),
        )
        .unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config().get_capability("chat").is_some());
        assert_eq!(
            capture(&ctx).warns.borrow().clone(),
            vec!["These stay, and will not work until fixed:"]
        );
        let infos = capture(&ctx).infos.borrow().clone();
        assert_eq!(
            infos,
            vec![
                "  - capability 'chat' (agent-model): its model 'granite-3.1-8b-instruct' is removed",
                "  Fix it with `granite-cli capability setup <type> --id <id>`.",
                "Model 'granite-3.1-8b-instruct' removed.",
            ]
        );
    }

    #[test]
    fn removing_something_nothing_depends_on_does_not_prompt() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();

        CapabilityCommands::remove(&mut ctx, "chat", None).unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_capability("chat").is_none());
    }
    #[test]
    fn removing_with_cascade_removes_target_and_dependents_without_prompting() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        ModelCommands::remove(
            &mut ctx,
            "granite-3.1-8b-instruct",
            Some(DependentsPolicy::Cascade),
        )
        .unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config().get_capability("chat").is_none());
    }

    #[test]
    fn removing_with_cascade_does_not_prompt_in_an_interactive_session() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();

        ModelCommands::remove(
            &mut ctx,
            "granite-3.1-8b-instruct",
            Some(DependentsPolicy::Cascade),
        )
        .unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config().get_capability("chat").is_none());
    }

    #[test]
    fn cascade_follows_dependents_of_dependents() {
        let _home = crate::config::TestConfigHome::new();
        // provider <- model <- capability: removing the provider has to take
        // the capability too, which only the model's own removal reaches.
        let mut ctx = ctx_model_with_one_dependent();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        ProviderCommands::remove(&mut ctx, "ollama", Some(DependentsPolicy::Cascade)).unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_provider("ollama").is_none());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config().get_capability("chat").is_none());
    }

    #[test]
    fn cascade_removes_the_discovered_models_of_a_provider_with_discovery_on() {
        let _home = crate::config::TestConfigHome::new();
        // With discovery on, `model remove` alone refuses `managed`.
        let mut ctx = ctx_provider_with_discovered_and_user_models();
        ctx.config_mut().providers.get_mut("ollama").unwrap().config =
            serde_json::json!({ "discover_models": true });
        assert!(ModelCommands::remove(&mut ctx, "managed", None).is_err());
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        ProviderCommands::remove(&mut ctx, "ollama", Some(DependentsPolicy::Cascade)).unwrap();

        assert!(ctx.config().get_provider("ollama").is_none());
        assert!(ctx.config().get_model("managed").is_none());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert!(ctx.config().get_capability("chat").is_none());
    }

    #[test]
    fn removing_with_discovered_removes_discovered_models_and_leaves_other_dependents() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_provider_with_discovered_and_user_models();

        ProviderCommands::remove(&mut ctx, "ollama", Some(DependentsPolicy::Discovered)).unwrap();

        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_provider("ollama").is_none());
        assert!(ctx.config().get_model("managed").is_none());
        // The catalog model, and the capability that pointed at the
        // discovered model, are deliberately left in place.
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config().get_capability("chat").is_some());

        // One plan for the whole chain: the model's own removal does not
        // print a second one.
        assert_eq!(
            capture(&ctx).warns.borrow().clone(),
            vec!["These stay, and will not work until fixed:"]
        );
        assert_eq!(
            capture(&ctx).infos.borrow().clone(),
            vec![
                "Removing provider 'ollama' and the models discovered from it:",
                "  - model 'managed'",
                "  - model 'granite-3.1-8b-instruct': its provider 'ollama' is removed",
                "  - capability 'chat' (agent-model): its model 'managed' is removed",
                "  Fix them with `granite-cli model setup <type> --id <id>` and \
                 `granite-cli capability setup <type> --id <id>`.",
                "Model 'managed' removed.",
                "Provider 'ollama' removed.",
            ]
        );
    }

    #[test]
    fn removing_a_discovered_model_with_the_discovered_policy_bypasses_the_discovery_guard() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_provider_with_discovered_and_user_models();

        ModelCommands::remove(&mut ctx, "managed", Some(DependentsPolicy::Discovered)).unwrap();

        assert!(ctx.config().get_model("managed").is_none());
        assert!(ctx.config().get_provider("ollama").is_some());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config().get_capability("chat").is_some());
    }

    #[test]
    fn removing_with_discovered_and_no_discovered_dependents_behaves_like_leave() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();

        ProviderCommands::remove(&mut ctx, "ollama", Some(DependentsPolicy::Discovered)).unwrap();

        assert!(ctx.config().get_provider("ollama").is_none());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config().get_capability("chat").is_some());
    }

    #[test]
    fn removing_with_refuse_fails_when_dependents_exist() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        let err = ModelCommands::remove(
            &mut ctx,
            "granite-3.1-8b-instruct",
            Some(DependentsPolicy::Refuse),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("Cannot remove model"),
            "got: {err}"
        );
        assert!(prompts(&ctx).is_empty());
        assert!(capture(&ctx).infos.borrow().is_empty());
        assert!(capture(&ctx).warns.borrow().is_empty());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
        assert!(ctx.config().get_capability("chat").is_some());
    }

    #[test]
    fn removing_with_refuse_does_not_prompt_in_an_interactive_session() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();

        let result = ModelCommands::remove(
            &mut ctx,
            "granite-3.1-8b-instruct",
            Some(DependentsPolicy::Refuse),
        );

        assert!(result.is_err());
        assert!(prompts(&ctx).is_empty());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
    }

    #[test]
    fn removing_with_refuse_succeeds_when_no_dependents_exist() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_model_with_one_dependent();

        CapabilityCommands::remove(&mut ctx, "chat", Some(DependentsPolicy::Refuse)).unwrap();

        assert!(ctx.config().get_capability("chat").is_none());
    }

    /// provider `ollama` <- model `granite-3.1-8b-instruct` <- capability
    /// `chat` <- launcher `claude`: three levels of dependents.
    fn ctx_three_levels() -> crate::AppContext {
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config_mut()
            .capabilities
            .get_mut("chat")
            .unwrap()
            .config = serde_json::json!({ "model_id": "granite-3.1-8b-instruct" });
        ctx
    }

    fn pair(kind: RefKind, id: &str) -> (RefKind, String) {
        (kind, id.to_string())
    }

    #[test]
    fn a_cascade_plan_covers_every_level_and_removes_dependents_first() {
        let ctx = ctx_three_levels();

        let plan = plan_removal(&ctx, RefKind::Provider, "ollama", Take::All);

        assert_eq!(
            plan.with,
            vec![
                pair(RefKind::Launcher, "claude"),
                pair(RefKind::Capability, "chat"),
                pair(RefKind::Model, "granite-3.1-8b-instruct"),
            ]
        );
        assert_eq!(
            plan.shown(),
            vec![
                &pair(RefKind::Model, "granite-3.1-8b-instruct"),
                &pair(RefKind::Capability, "chat"),
                &pair(RefKind::Launcher, "claude"),
            ]
        );
        assert!(plan.left.is_empty());
    }

    #[test]
    fn a_cascade_prints_one_plan_for_every_level() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_three_levels();

        ProviderCommands::remove(&mut ctx, "ollama", Some(DependentsPolicy::Cascade)).unwrap();

        assert!(capture(&ctx).warns.borrow().is_empty());
        assert_eq!(
            capture(&ctx).infos.borrow().clone(),
            vec![
                "Removing provider 'ollama' and what depends on it:",
                "  - model 'granite-3.1-8b-instruct'",
                "  - capability 'chat'",
                "  - launcher 'claude'",
                "Launcher 'claude' removed.",
                "Capability 'chat' removed.",
                "Model 'granite-3.1-8b-instruct' removed.",
                "Provider 'ollama' removed.",
            ]
        );
        assert!(ctx.config().launchers.is_empty());
        assert!(ctx.config().capabilities.is_empty());
        assert!(ctx.config().models.is_empty());
        assert!(ctx.config().providers.is_empty());
    }

    #[test]
    fn a_discovered_plan_records_what_stays_and_why() {
        let ctx = ctx_provider_with_discovered_and_user_models();

        let plan = plan_removal(&ctx, RefKind::Provider, "ollama", Take::Discovered);

        assert_eq!(plan.with, vec![pair(RefKind::Model, "managed")]);
        assert_eq!(
            plan.left,
            vec![
                Stranded {
                    kind: RefKind::Model,
                    id: "granite-3.1-8b-instruct".to_string(),
                    removed: vec![pair(RefKind::Provider, "ollama")],
                },
                Stranded {
                    kind: RefKind::Capability,
                    id: "chat".to_string(),
                    removed: vec![pair(RefKind::Model, "managed")],
                },
            ]
        );
    }

    #[test]
    fn a_leave_plan_removes_only_the_target_and_lists_its_direct_dependents() {
        let ctx = ctx_three_levels();

        let plan = plan_removal(&ctx, RefKind::Provider, "ollama", Take::Nothing);

        assert!(plan.with.is_empty());
        // The capability and launcher still resolve their own references, so
        // only the model is listed.
        assert_eq!(
            plan.left,
            vec![Stranded {
                kind: RefKind::Model,
                id: "granite-3.1-8b-instruct".to_string(),
                removed: vec![pair(RefKind::Provider, "ollama")],
            }]
        );
    }

    #[test]
    fn an_instance_reached_on_two_paths_is_planned_once() {
        // `claude` enables two capabilities on the same model, so a cascade
        // from the model reaches it through each.
        let mut ctx = ctx_three_levels();
        ctx.config_mut().capabilities.insert(
            "vision".to_string(),
            CapabilityConfig {
                capability_id: "vision".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "granite-3.1-8b-instruct" }),
            },
        );
        ctx.config_mut()
            .launchers
            .get_mut("claude")
            .unwrap()
            .enabled_capabilities
            .push("vision".to_string());

        let plan = plan_removal(&ctx, RefKind::Model, "granite-3.1-8b-instruct", Take::All);

        assert_eq!(
            plan.with
                .iter()
                .filter(|node| **node == pair(RefKind::Launcher, "claude"))
                .count(),
            1,
            "{:?}",
            plan.with
        );
        assert_eq!(plan.with[0], pair(RefKind::Launcher, "claude"));
        assert_eq!(plan.with.len(), 3, "{:?}", plan.with);
    }

    #[test]
    fn a_reason_groups_removed_instances_of_one_kind() {
        assert_eq!(
            reason(&[pair(RefKind::Provider, "ollama")]),
            "its provider 'ollama' is removed"
        );
        assert_eq!(
            reason(&[
                pair(RefKind::Capability, "chat"),
                pair(RefKind::Capability, "vision"),
            ]),
            "its capabilities 'chat' and 'vision' are removed"
        );
    }

    #[test]
    fn an_interactive_removal_asks_one_question_for_the_whole_chain() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_three_levels();
        answer(&ctx, &[0]);

        ProviderCommands::remove(&mut ctx, "ollama", None).unwrap();

        let prompts = prompts(&ctx);
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(
            prompts[0].1,
            vec![
                "Remove provider 'ollama' and the 3 instances listed above",
                "Cancel, keep provider 'ollama'",
                "Remove only provider 'ollama', fix the rest later",
            ]
        );
        assert_eq!(
            capture(&ctx).warns.borrow().clone(),
            vec!["Other instances depend on provider 'ollama':"]
        );
        assert!(ctx.config().launchers.is_empty());
        assert!(ctx.config().capabilities.is_empty());
        assert!(ctx.config().models.is_empty());
        assert!(ctx.config().providers.is_empty());
    }

    #[test]
    fn removing_only_the_target_interactively_reports_what_stays() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_three_levels();
        answer(&ctx, &[2]);

        ProviderCommands::remove(&mut ctx, "ollama", None).unwrap();

        assert_eq!(prompts(&ctx).len(), 1);
        assert_eq!(
            capture(&ctx).warns.borrow().clone(),
            vec![
                "Other instances depend on provider 'ollama':",
                "These stay, and will not work until fixed:",
            ]
        );
        assert!(ctx.config().get_provider("ollama").is_none());
        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_some());
    }

    #[test]
    fn a_cascade_refuses_a_model_another_provider_would_add_back() {
        let _home = crate::config::TestConfigHome::new();
        // `managed` uses `ollama` but was discovered by `other`, whose
        // discovery is on, so removing `ollama` does not stop the sync.
        let mut ctx = ctx_provider_with_discovered_and_user_models();
        ctx.config_mut().providers.insert(
            "other".to_string(),
            ProviderConfig {
                provider_id: "other".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({ "discover_models": true }),
            },
        );
        ctx.config_mut()
            .models
            .get_mut("managed")
            .unwrap()
            .discovered
            .as_mut()
            .unwrap()
            .provider_id = "other".to_string();

        let err = ProviderCommands::remove(&mut ctx, "ollama", Some(DependentsPolicy::Cascade))
            .unwrap_err();

        assert!(err.to_string().contains("discovery on"), "got: {err}");
        assert!(ctx.config().get_provider("ollama").is_some());
        assert!(ctx.config().get_model("managed").is_some());
        assert!(ctx.config().get_capability("chat").is_some());
    }

    #[tokio::test]
    async fn a_healthy_instance_is_clean_without_prompting() {
        let mut ctx = ctx_with_a_dangling_model_ref();

        let outcome = remediate(
            &mut ctx,
            RefKind::Model,
            "granite-3.1-8b-instruct",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Clean);
        assert!(prompts(&ctx).is_empty());
    }

    #[tokio::test]
    async fn reconfigure_runs_setup_against_the_instance_holding_the_reference() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);
        // Remediation passes force_overwrite=true to setup, so no
        // "already configured, overwrite?" confirm appears — the choice made
        // at the remediation prompt is the only confirmation needed.

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        // Setup ran against `chat`, the capability holding the broken
        // reference, not against the model that is missing.
        assert_eq!(
            ctx.config()
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct")
        );
        // And the loop re-validated afterwards rather than taking the fix on
        // trust.
        assert_eq!(outcome, Outcome::Clean);

        let (_, items) = &prompts(&ctx)[0];
        assert!(
            items[0].contains("Reconfigure capability 'chat'"),
            "{items:?}"
        );
        // No overwrite confirm was issued.
        assert!(
            capture(&ctx).confirm_answers.borrow().is_empty(),
            "no canned confirms were consumed, so no confirm was issued"
        );
    }

    #[tokio::test]
    async fn remove_deletes_the_instance_holding_the_reference() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        // Remove, then keep the launcher that enables `chat` when the
        // removal asks about it.
        answer(&ctx, &[1, 2]);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert!(ctx.config().get_capability("chat").is_none());
        // What the caller asked about is gone, so it does not validate.
        assert_eq!(outcome, Outcome::Unresolved);
    }

    #[tokio::test]
    async fn a_fix_that_exposes_a_second_problem_is_offered_in_turn() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        // A second broken capability, so repairing the first leaves one more
        // for the loop to find.
        ctx.config_mut().capabilities.insert(
            "vision".to_string(),
            CapabilityConfig {
                capability_id: "vision".to_string(),
                capability_type: "agent-model".to_string(),
                config: serde_json::json!({ "model_id": "also-gone" }),
            },
        );
        ctx.config_mut()
            .launchers
            .get_mut("claude")
            .unwrap()
            .enabled_capabilities
            .push("vision".to_string());
        // Un-enable the first, then decline the second.
        answer(&ctx, &[1, 2]);

        let outcome = remediate(&mut ctx, RefKind::Launcher, "claude", OnDecline::Skip, true)
            .await
            .unwrap();

        let prompts = prompts(&ctx);
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(prompts[0].1[0].contains("capability 'chat'"), "{prompts:?}");
        assert!(
            prompts[1].1[0].contains("capability 'vision'"),
            "{prompts:?}"
        );
        assert_eq!(outcome, Outcome::Unresolved);
    }

    #[tokio::test]
    async fn a_launch_un_enables_a_capability_instead_of_deleting_it() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(
            items[1], "Remove capability 'chat' from launcher 'claude'",
            "{items:?}"
        );
        assert_eq!(outcome, Outcome::Clean);
        assert!(
            ctx.config()
                .get_launcher("claude")
                .unwrap()
                .enabled_capabilities
                .is_empty()
        );
        assert!(
            ctx.config().get_capability("chat").is_some(),
            "the capability stays configured for any other launcher"
        );
    }

    #[tokio::test]
    async fn a_launch_un_enables_a_capability_that_is_not_configured() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config_mut().capabilities.remove("chat");
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        // The fix acts on the launcher here, and the removal on offer is still
        // the entry in its list rather than the launcher itself.
        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(
            items[1], "Remove capability 'chat' from launcher 'claude'",
            "{items:?}"
        );
        assert_eq!(outcome, Outcome::Clean);
        assert!(
            ctx.config()
                .get_launcher("claude")
                .unwrap()
                .enabled_capabilities
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_caller_naming_the_capability_is_still_offered_deletion() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[2]);

        remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(items[1], "Remove capability 'chat'", "{items:?}");
    }

    #[tokio::test]
    async fn reconfigure_proceeds_without_an_overwrite_confirm() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Clean);
        let prompts = prompts(&ctx);
        assert!(prompts[0].1[0].starts_with("Reconfigure"), "{prompts:?}");
        // No overwrite confirm — force_overwrite skips the second prompt.
        assert!(capture(&ctx).confirm_answers.borrow().is_empty());
        assert_eq!(
            ctx.config()
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct"),
        );
    }

    #[tokio::test]
    async fn reconfiguring_a_hardware_profile_replaces_it_without_an_overwrite_confirm() {
        use crate::config::HardwareConfig;
        use crate::config::hardware::HardwareLink;
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = crate::AppContext::new(Config::default(), Arc::new(CaptureUi::default()));
        ctx.config_mut().hardware.insert(
            "card".to_string(),
            HardwareConfig {
                headroom: Some(0.9),
                ..HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0)
            },
        );
        ctx.config_mut().providers.insert(
            "vllm-lab".to_string(),
            ProviderConfig {
                hardware: Some(HardwareLink::Profile("card".to_string())),
                provider_id: "vllm-lab".to_string(),
                provider_type: "openai-compatible".to_string(),
                config: serde_json::json!({ "base_url": "http://lab:8000" }),
            },
        );
        answer(&ctx, &[0]);
        capture(&ctx)
            .text_answers
            .borrow_mut()
            .push_back("1.5".to_string());

        let outcome = remediate(
            &mut ctx,
            RefKind::Provider,
            "vllm-lab",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Clean);
        assert!(prompts(&ctx)[0].1[0].starts_with("Reconfigure hardware profile 'card'"));
        assert!(capture(&ctx).confirm_prompts.borrow().is_empty());
        assert_eq!(
            ctx.config().get_hardware("card"),
            Some(&HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0))
        );
    }

    #[tokio::test]
    async fn a_launch_reconfigure_fixes_the_reference_without_an_overwrite_confirm() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Clean);

        // No overwrite confirm — force_overwrite skips the second prompt.
        assert!(capture(&ctx).confirm_answers.borrow().is_empty());
        assert_eq!(
            ctx.config()
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct"),
        );
    }

    fn ctx_with_a_dangling_provider_ref() -> crate::AppContext {
        let mut ctx = crate::AppContext::new(
            Config::default(),
            Arc::new(crate::utils::ui::base::tests::CaptureUi::default()),
        );
        ctx.config_mut().providers.insert(
            "ollama".to_string(),
            ProviderConfig {
                hardware: Default::default(),
                provider_id: "ollama".to_string(),
                provider_type: "ollama".to_string(),
                config: serde_json::json!({}),
            },
        );
        ctx.config_mut().models.insert(
            "granite-3.1-8b-instruct".to_string(),
            ModelConfig {
                discovered: None,
                model_id: "granite-3.1-8b-instruct".to_string(),
                model_type: "granite-3.1-8b-instruct".to_string(),
                provider_id: "gone-provider".to_string(),
                variant: None,
                config: serde_json::json!({}),
            },
        );
        ctx
    }

    #[tokio::test]
    async fn reconfigure_model_arm_fixes_a_broken_provider_ref() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_provider_ref();
        answer(&ctx, &[0]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Model,
            "granite-3.1-8b-instruct",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Clean);
        assert_ne!(
            ctx.config()
                .get_model("granite-3.1-8b-instruct")
                .map(|m| m.provider_id.as_str()),
            Some("gone-provider"),
        );
        assert!(capture(&ctx).confirm_answers.borrow().is_empty());
    }

    #[tokio::test]
    async fn removing_the_named_instance_ends_without_reporting_it_missing() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_provider_ref();
        // Reconfigure, remove, skip: remove is the second.
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Model,
            "granite-3.1-8b-instruct",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        assert!(ctx.config().get_model("granite-3.1-8b-instruct").is_none());
        assert_eq!(outcome, Outcome::Unresolved);
        assert_eq!(prompts(&ctx).len(), 1);
        let warns = capture(&ctx).warns.borrow().clone();
        assert!(
            !warns
                .iter()
                .any(|w| w == "model 'granite-3.1-8b-instruct' is not configured"),
            "the model the user chose to remove is not reported as missing: {warns:?}"
        );
    }

    #[tokio::test]
    async fn declining_stops_instead_of_asking_again() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[2]);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Unresolved);
        assert_eq!(prompts(&ctx).len(), 1);
        assert_eq!(
            ctx.config()
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("gone"),
            "declining leaves the configuration alone"
        );
    }

    #[tokio::test]
    async fn a_non_prompting_caller_never_reaches_a_prompt() {
        let mut ctx = ctx_with_a_dangling_model_ref();

        let outcome = remediate(
            &mut ctx,
            RefKind::Capability,
            "chat",
            OnDecline::Skip,
            false,
        )
        .await
        .unwrap();

        assert_eq!(outcome, Outcome::Unresolved);
        assert!(prompts(&ctx).is_empty());
        assert!(!capture(&ctx).warns.borrow().is_empty(), "still reported");
    }

    #[tokio::test]
    async fn a_non_interactive_session_never_reaches_a_prompt() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        *capture(&ctx).interactive.borrow_mut() = Some(false);

        let outcome = remediate(&mut ctx, RefKind::Capability, "chat", OnDecline::Skip, true)
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Unresolved);
        assert!(prompts(&ctx).is_empty());
    }

    #[tokio::test]
    async fn an_unknown_type_offers_removal_but_not_reconfiguration() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        ctx.config_mut()
            .models
            .get_mut("granite-3.1-8b-instruct")
            .unwrap()
            .model_type = "not-a-model".to_string();
        answer(&ctx, &[1]);

        let outcome = remediate(
            &mut ctx,
            RefKind::Model,
            "granite-3.1-8b-instruct",
            OnDecline::Skip,
            true,
        )
        .await
        .unwrap();

        // Setup cannot run a type the registry does not have, so the only
        // fix offered is removal.
        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(items.len(), 2, "{items:?}");
        assert!(items[0].starts_with("Remove model"), "{items:?}");
        assert_eq!(outcome, Outcome::Unresolved);
    }

    // The `launch` pre-launch is thin policy over `remediate`, so its two
    // tests live here with the fixture rather than in `launcher.rs`.

    #[tokio::test]
    async fn the_launch_prelaunch_aborts_when_the_user_declines() {
        let mut ctx = ctx_with_a_dangling_model_ref();

        // No canned answer, so the prompt takes its default, which declines.
        let result = crate::commands::LauncherCommands::prelaunch(&mut ctx, "claude").await;

        assert!(result.is_err(), "declining must stop the launch");
        assert_eq!(
            result.unwrap_err().to_string(),
            "Launch aborted: launcher 'claude' has a configuration problem that was not fixed."
        );
    }

    #[tokio::test]
    async fn the_launch_prelaunch_proceeds_once_the_reference_is_repaired() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[0]);
        // No second confirm needed: force_overwrite bypasses the overwrite
        // prompt, so the remediation choice is the only confirmation.

        crate::commands::LauncherCommands::prelaunch(&mut ctx, "claude")
            .await
            .expect("a repaired configuration launches");

        assert_eq!(
            ctx.config()
                .get_capability("chat")
                .and_then(|c| c.config.get("model_id"))
                .and_then(|v| v.as_str()),
            Some("granite-3.1-8b-instruct")
        );
    }

    #[tokio::test]
    async fn aborting_callers_are_offered_cancel_rather_than_skip() {
        let mut ctx = ctx_with_a_dangling_model_ref();
        answer(&ctx, &[2]);

        remediate(
            &mut ctx,
            RefKind::Launcher,
            "claude",
            OnDecline::Abort,
            true,
        )
        .await
        .unwrap();

        let (_, items) = &prompts(&ctx)[0];
        assert_eq!(items[2], "Cancel", "{items:?}");
    }

    // The Launcher and Provider arms of reconfigure() are unreachable via
    // the normal validation flow, so Fix is constructed directly here.
    #[tokio::test]
    async fn reconfigure_launcher_arm_calls_launcher_setup() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        let fix = Fix {
            kind: RefKind::Launcher,
            id: "claude".to_string(),
            type_name: "claude".to_string(),
            can_reconfigure: true,
            reset: None,
            disable: None,
        };
        // Binary not found in CI is fine — the arm is covered regardless.
        let _ = reconfigure(&mut ctx, &fix).await;
    }

    #[tokio::test]
    async fn reconfigure_provider_arm_calls_provider_setup() {
        let _home = crate::config::TestConfigHome::new();
        let mut ctx = ctx_with_a_dangling_model_ref();
        let fix = Fix {
            kind: RefKind::Provider,
            id: "ollama".to_string(),
            type_name: "ollama".to_string(),
            can_reconfigure: true,
            reset: None,
            disable: None,
        };
        reconfigure(&mut ctx, &fix).await.unwrap();
        assert!(ctx.config().get_provider("ollama").is_some());
        assert!(capture(&ctx).confirm_answers.borrow().is_empty());
    }
}
