// Local
use crate::AppContext;
use crate::config::HardwareConfig;
use crate::config::hardware::{
    CUSTOM_HARDWARE_TYPE, LOCAL_HARDWARE_ID, LocalHardware, LocalMode, saved_figures,
};
use crate::utils::hardware::{HardwareProfile, MachineInfo, Memory, MemoryKind, detect_hardware};
use crate::utils::hardware_catalog::{self, CatalogEntry, HARDWARE_CATALOG};
use anyhow::{Result, anyhow, bail};

pub struct HardwareCommands;

/*-- public --*/

/// What `hardware setup` was given on the command line. Anything left out is
/// asked for when the run is interactive.
#[derive(Debug, Default, Clone)]
pub(crate) struct HardwareSetupArgs {
    /// A catalog id, `custom` or `local`.
    pub(crate) target: Option<String>,
    /// The id to save the profile under.
    pub(crate) id: Option<String>,
    /// For a catalog profile, the size; for a custom or manual local
    /// profile, the figure the memory kind names.
    pub(crate) memory: Option<f64>,
    /// For a custom or manual local profile.
    pub(crate) kind: Option<MemoryKind>,
    /// `setup local --auto`: take the detected figures and follow detection.
    pub(crate) auto: bool,
    /// `setup local --manual`: keep typed figures on this machine.
    pub(crate) manual: bool,
    /// The headroom factor, in place of the default for the memory kind.
    pub(crate) headroom: Option<f64>,
    /// Replace a profile with the same id without asking, for callers that
    /// have already asked.
    pub(crate) force_overwrite: bool,
}

impl HardwareCommands {
    /// Key-value fields for the hardware detail panel, from detection.
    /// Shared by the CLI command and the TUI.
    pub(crate) fn hardware_fields() -> Vec<(&'static str, String)> {
        let profile = detect_hardware();
        let gb = |figure: Option<f64>| {
            figure
                .map(|v| format!("{v:.2} GB"))
                .unwrap_or_else(|| "None".to_string())
        };
        vec![
            (
                "CPU Cores",
                profile
                    .machine
                    .cpu_cores
                    .map(|cores| cores.to_string())
                    .unwrap_or_else(|| "None".to_string()),
            ),
            ("CPU Architecture", profile.machine.cpu_arch.clone()),
            ("RAM", gb(profile.memory.ram_gb())),
            (
                "GPU Vendor",
                profile
                    .gpu_vendor
                    .clone()
                    .unwrap_or_else(|| "None".to_string()),
            ),
            ("VRAM", gb(profile.memory.vram_gb())),
        ]
    }

    /// `granite-cli hardware` with no subcommand: the local hardware profile
    /// and its mode.
    pub fn show(ctx: &mut AppContext) -> Result<()> {
        let can_prompt = can_prompt(ctx);
        Self::show_with(ctx, detect_hardware, can_prompt)
    }

    /// `hardware catalog`: the built-in devices and their sizes, grouped by
    /// family in catalog order, or one family with `--family`.
    pub fn catalog(ctx: &AppContext, family: Option<&str>) -> Result<()> {
        let family = family.map(find_family).transpose()?;
        let rows: Vec<Vec<String>> = HARDWARE_CATALOG
            .iter()
            .filter(|e| family.is_none_or(|f| e.family == f))
            .map(|e| {
                vec![
                    e.family.clone(),
                    e.id.clone(),
                    e.name.clone(),
                    catalog_memory(e),
                ]
            })
            .collect();
        ctx.ui.table(
            &format!(
                "Hardware Catalog ({})",
                count(rows.len(), "entry", "entries")
            ),
            &["FAMILY", "ID", "NAME", "MEMORY"],
            &rows,
        );
        Ok(())
    }

    /// `hardware list`: the configured hardware profiles, `local` first.
    pub fn list(ctx: &mut AppContext) -> Result<()> {
        let unused = Self::compare_local(ctx, detect_hardware, can_prompt(ctx))?;
        Self::list_table(ctx, unused.as_ref());
        Ok(())
    }

    /// `hardware info <id>`: one configured hardware profile.
    pub fn info(ctx: &mut AppContext, id: &str) -> Result<()> {
        let unused = Self::compare_local(ctx, detect_hardware, can_prompt(ctx))?;
        Self::info_detail(ctx, id, unused.as_ref())
    }

    /// `hardware setup`: create or change a hardware profile from a catalog
    /// entry, from typed figures (`custom`), or set up `local`. Returns the id
    /// of the profile saved. `model recommend` and `provider setup` reuse
    /// these steps to create a profile on the way.
    pub(crate) fn setup(ctx: &mut AppContext, args: HardwareSetupArgs) -> Result<String> {
        let interactive = can_prompt(ctx);
        Self::setup_with(ctx, args, interactive, detect_hardware)
    }

    /// `hardware remove <id>`. `local` cannot be removed.
    pub fn remove(ctx: &mut AppContext, id: &str) -> Result<()> {
        Self::check_local(ctx, detect_hardware, can_prompt(ctx))?;
        if ctx.config().get_hardware(id).is_none() {
            return Err(not_configured(ctx, id));
        }
        ctx.config_mut().remove_hardware(id)?;
        ctx.ui.info(&format!("Hardware profile '{id}' removed."));
        Ok(())
    }

    /// The local hardware profile's figures, for a command that estimates
    /// fit. Writes `local` from detection when there is none, and checks it
    /// against detection first (see [`Self::check_local`]).
    pub(crate) fn local_for_fit(ctx: &mut AppContext, can_prompt: bool) -> Result<HardwareProfile> {
        Self::check_local(ctx, detect_hardware, can_prompt)?;
        ctx.hardware_profile(LOCAL_HARDWARE_ID)
            .map_err(|e| anyhow!("hardware profile '{LOCAL_HARDWARE_ID}': {e}"))
    }

    /// The id and figures of hardware profile `id`, for a command that
    /// estimates fit against it. The id differs from `id` only when the user
    /// creates the profile under another name. `local` is checked against
    /// detection first. An id with no profile is offered for creation when
    /// the run can ask, from the catalog entry with that id or as a custom
    /// profile, and is an error listing the configured ids otherwise.
    pub(crate) fn profile_for_fit(
        ctx: &mut AppContext,
        id: &str,
        can_prompt: bool,
    ) -> Result<(String, HardwareProfile)> {
        Self::profile_for_fit_with(ctx, id, can_prompt, detect_hardware)
    }

    /// The providers that run on hardware profile `id`, sorted. Until
    /// providers are linked to hardware, every provider runs on `local`,
    /// which is how existing configuration is read.
    pub(crate) fn providers_on(ctx: &AppContext, id: &str) -> Vec<String> {
        providers_on(ctx, id)
    }

    /// Whether this run can ask the user something: the output backend is
    /// interactive, which for terminal and plain output means stdin and
    /// stderr are terminals (see [`crate::utils::ui::base::Ui::is_interactive`]).
    pub(crate) fn can_prompt(ctx: &AppContext) -> bool {
        can_prompt(ctx)
    }

    /// Makes sure `local` exists and compares it with `detect`. In manual
    /// mode, a detection the user already kept the saved figures over is not
    /// a difference. A difference is never saved on its own: an interactive
    /// run asks what to do, and any other run keeps the saved figures and
    /// warns on stderr. `detect` runs at most once.
    pub(crate) fn check_local(
        ctx: &mut AppContext,
        detect: impl FnOnce() -> HardwareProfile,
        can_prompt: bool,
    ) -> Result<()> {
        Self::compare_local(ctx, detect, can_prompt).map(|_| ())
    }

    /// [`Self::check_local`] for a command that does not use `local`'s
    /// figures, such as `hardware setup` of another profile: a problem is a warning on
    /// stderr, and the command goes on. Loading the configuration already
    /// logs why a `local` profile cannot be read, so for that case the
    /// warning only names the command that replaces it.
    pub(crate) fn check_local_or_warn(
        ctx: &mut AppContext,
        detect: impl FnOnce() -> HardwareProfile,
        can_prompt: bool,
    ) {
        let unreadable = ctx
            .config()
            .get_hardware(LOCAL_HARDWARE_ID)
            .is_some_and(|h| h.read_local().is_err());
        if unreadable {
            ctx.ui.warn_stderr(&format!(
                "Hardware profile '{LOCAL_HARDWARE_ID}' cannot be read; run `granite-cli hardware \
                 setup local --auto` to replace it with the detected figures."
            ));
            return;
        }
        if let Err(e) = Self::check_local(ctx, detect, can_prompt) {
            ctx.ui.warn_stderr(&e.to_string());
        }
    }
}

/*-- private --*/

impl HardwareCommands {
    /// [`Self::check_local`], returning the detection that differs from the
    /// figures in use when this run keeps the stored figures without
    /// recording it, so `hardware list` and `info` can show it.
    fn compare_local(
        ctx: &mut AppContext,
        detect: impl FnOnce() -> HardwareProfile,
        can_prompt: bool,
    ) -> Result<Option<HardwareProfile>> {
        let Some(saved) = ctx.config().get_hardware(LOCAL_HARDWARE_ID) else {
            ctx.ensure_local_hardware(detect)?;
            return Ok(None);
        };
        let headroom = saved.headroom;
        let mut local = saved.read_local().map_err(|e| {
            anyhow!(
                "hardware profile '{LOCAL_HARDWARE_ID}': {e}; run `granite-cli hardware setup \
                 local --auto` to replace it with the detected figures"
            )
        })?;
        let detected = saved_figures(detect());
        let kept_over = match local.mode {
            LocalMode::Auto => None,
            // A manual profile written without a detection, by hand or by an
            // earlier build, has nothing to compare with.
            LocalMode::Manual => match &local.detected {
                Some(kept_over) => Some(kept_over),
                None => return Ok(None),
            },
        };
        if local.profile.same_memory(&detected)
            || kept_over.is_some_and(|k| k.same_memory(&detected))
        {
            return Ok(None);
        }
        let stored = local.profile.memory_summary();
        let found = detected.memory_summary();

        if !can_prompt {
            ctx.ui.warn_stderr(&format!(
                "Local hardware differs from profile '{LOCAL_HARDWARE_ID}' (stored {stored}, \
                 detected {found}).\nUsing the stored figures. Run `granite-cli hardware \
                 setup local --auto` to update."
            ));
            return Ok(Some(detected));
        }

        ctx.ui.warn(&format!(
            "Local hardware differs from profile '{LOCAL_HARDWARE_ID}':\n    stored    {stored}\n    \
             detected  {found}"
        ));
        let choices = [
            format!("Update '{LOCAL_HARDWARE_ID}' to the detected figures"),
            "Keep the stored figures for this session".to_string(),
            "Keep the stored figures (don't ask again)".to_string(),
        ];
        match ctx.ui.select("What would you like to do?", &choices, 0)? {
            0 => {
                local.mode = LocalMode::Auto;
                local.profile = detected;
                local.detected = None;
            }
            // This run only: nothing is saved, so the next run asks again.
            1 => return Ok(Some(detected)),
            _ => {
                local.mode = LocalMode::Manual;
                local.detected = Some(detected);
            }
        }
        let config = HardwareConfig {
            headroom,
            ..HardwareConfig::local(local)
        };
        ctx.config_mut()
            .insert_hardware(LOCAL_HARDWARE_ID, config)?;
        Ok(None)
    }

    fn profile_for_fit_with(
        ctx: &mut AppContext,
        id: &str,
        can_prompt: bool,
        detect: impl FnOnce() -> HardwareProfile,
    ) -> Result<(String, HardwareProfile)> {
        let mut id = id.to_string();
        if id == LOCAL_HARDWARE_ID {
            Self::check_local(ctx, detect, can_prompt)?;
        } else if ctx.config().get_hardware(&id).is_none() {
            let entry = hardware_catalog::find(&id);
            if entry.is_none() {
                check_profile_id(&id)
                    .map_err(|e| anyhow!("no hardware profile '{id}', and {e}"))?;
            }
            let missing = || {
                anyhow!(
                    "no hardware profile '{id}'. Configured: {}.\n  Create it with `{}`.",
                    configured_ids(ctx).join(", "),
                    setup_command(&id)
                )
            };
            if !can_prompt {
                return Err(missing());
            }
            let question = match entry {
                Some(entry) => format!(
                    "No hardware profile '{id}'. Create one from catalog entry '{id}' ({})?",
                    entry.name
                ),
                None => format!("No hardware profile '{id}'. Create it as a custom profile?"),
            };
            if !ctx.ui.confirm(&question, true)? {
                return Err(missing());
            }
            // A catalog profile asks for its id, defaulting to the catalog id;
            // a custom one is saved under the id asked for.
            let args = match entry {
                Some(_) => HardwareSetupArgs {
                    target: Some(id.clone()),
                    ..Default::default()
                },
                None => HardwareSetupArgs {
                    target: Some(CUSTOM_HARDWARE_TYPE.to_string()),
                    id: Some(id.clone()),
                    ..Default::default()
                },
            };
            id = Self::setup_with(ctx, args, true, || unreachable!("only `local` detects"))?;
        }
        let profile = ctx
            .hardware_profile(&id)
            .map_err(|e| anyhow!("hardware profile '{id}': {e}"))?;
        Ok((id, profile))
    }

    fn show_with(
        ctx: &mut AppContext,
        detect: impl FnOnce() -> HardwareProfile,
        can_prompt: bool,
    ) -> Result<()> {
        let unused = Self::compare_local(ctx, detect, can_prompt)?;
        Self::info_detail(ctx, LOCAL_HARDWARE_ID, unused.as_ref())
    }

    /// `unused` is a detection of this machine that differs from `local`'s
    /// figures and that this run did not take; see [`Self::compare_local`].
    fn info_detail(ctx: &AppContext, id: &str, unused: Option<&HardwareProfile>) -> Result<()> {
        let hardware = ctx
            .config()
            .get_hardware(id)
            .ok_or_else(|| not_configured(ctx, id))?;
        let mut fields: Vec<(&str, String)> = vec![("Type", hardware.hardware_type.clone())];
        if let Some(entry) = hardware_catalog::find(&hardware.hardware_type) {
            fields.push(("Device", entry.name.clone()));
        }
        if let Some(local) = hardware.local_settings() {
            fields.push(("Mode", mode_label(local.mode).to_string()));
            if let Some(detected) = unused.or(local.detected.as_ref()) {
                fields.push((
                    "Detected",
                    format!("{} (not used)", detected.memory_summary()),
                ));
            }
        }
        match ctx.hardware_profile(id) {
            Ok(p) => {
                let gb = |v: Option<f64>| v.map(|v| format!("{v:.2} GB"));
                fields.push(("Memory", p.memory_summary()));
                if let Some(vram) = gb(p.memory.vram_gb()) {
                    fields.push(("Accelerator memory", vram));
                }
                if let Some(ram) = gb(p.memory.ram_gb()) {
                    fields.push(("System memory", ram));
                }
                fields.push(("Headroom", headroom_label(&p)));
                fields.push((
                    "Usable for models",
                    format!("{:.2} GB", p.usable_memory_gb()),
                ));
                let MachineInfo {
                    os,
                    cpu_cores,
                    cpu_arch,
                } = p.machine;
                let text = |s: String| (!s.is_empty()).then_some(s);
                let descriptive = [
                    ("GPU Vendor", p.gpu_vendor),
                    ("OS", text(os)),
                    ("CPU Architecture", text(cpu_arch)),
                    ("CPU Cores", cpu_cores.map(|c| c.to_string())),
                ];
                fields.extend(
                    descriptive
                        .into_iter()
                        .filter_map(|(k, v)| v.map(|v| (k, v))),
                );
            }
            Err(e) => fields.push(("Problem", ctx.ui.warn_mark(&format!("⚠ {e}")))),
        }
        fields.push(("Providers", providers_on(ctx, id).join(", ")));
        ctx.ui.detail(&format!("Hardware Profile '{id}'"), &fields);
        Ok(())
    }

    /// `unused` is as for [`Self::info_detail`].
    fn list_table(ctx: &AppContext, unused: Option<&HardwareProfile>) {
        let config = ctx.config();
        let mut ids: Vec<&String> = config.hardware.keys().collect();
        ids.sort_by_key(|id| (id.as_str() != LOCAL_HARDWARE_ID, id.as_str()));
        let rows: Vec<Vec<String>> = ids
            .into_iter()
            .map(|id| {
                let hardware = &config.hardware[id];
                let (memory, problem) = match ctx.hardware_profile(id) {
                    Ok(p) => (p.memory_summary(), None),
                    Err(e) => ("-".to_string(), Some(ctx.ui.warn_mark(&format!("⚠ {e}")))),
                };
                vec![
                    id.clone(),
                    hardware.hardware_type.clone(),
                    memory,
                    providers_on(ctx, id).join(", "),
                    problem.unwrap_or_else(|| notes(hardware, unused)),
                ]
            })
            .collect();
        ctx.ui.table(
            &format!(
                "Configured Hardware ({})",
                count(rows.len(), "profile", "profiles")
            ),
            &["ID", "TYPE", "MEMORY", "PROVIDERS", "NOTES"],
            &rows,
        );
    }

    fn setup_with(
        ctx: &mut AppContext,
        args: HardwareSetupArgs,
        interactive: bool,
        detect: impl FnOnce() -> HardwareProfile,
    ) -> Result<String> {
        let target = match args.target.clone() {
            Some(target) => target,
            None if interactive => choose_target(ctx)?,
            None => bail!(
                "name what to set up: a catalog id (see `granite-cli hardware catalog`), \
                 `custom` or `local`"
            ),
        };
        check_flags(&target, &args)?;
        let mut config = match target.as_str() {
            LOCAL_HARDWARE_ID => setup_local(ctx, &args, interactive, detect)?,
            CUSTOM_HARDWARE_TYPE => {
                let id = profile_id(ctx, &args, None, interactive)?;
                if !replace_existing(ctx, &id, args.force_overwrite, interactive)? {
                    return Ok(id);
                }
                let profile = typed_figures(ctx, &args, interactive)?;
                HardwareConfig::custom(&id, profile)
            }
            catalog_id => {
                let entry = hardware_catalog::find(catalog_id).ok_or_else(|| {
                    anyhow!(
                        "'{catalog_id}' is not in the hardware catalog; see `granite-cli \
                         hardware catalog`, or use `custom`"
                    )
                })?;
                let memory = catalog_size(ctx, entry, args.memory, interactive)?;
                let id = profile_id(ctx, &args, Some(catalog_id), interactive)?;
                if !replace_existing(ctx, &id, args.force_overwrite, interactive)? {
                    return Ok(id);
                }
                HardwareConfig::catalog(&id, catalog_id, memory)
            }
        };
        let id = config.hardware_id.clone();
        let kind = config.profile().map_err(anyhow::Error::msg)?.memory_kind();
        let kept = ctx.config().get_hardware(&id).and_then(|h| h.headroom);
        config.headroom = headroom(ctx, args.headroom, kept, kind, interactive)?;
        ctx.config_mut().insert_hardware(&id, config)?;
        let summary = ctx
            .hardware_profile(&id)
            .map(|p| p.memory_summary())
            .unwrap_or_default();
        ctx.ui.info(
            &ctx.ui
                .ok(&format!("✓ Hardware profile '{id}' configured ({summary})")),
        );
        Ok(id)
    }
}

/// Refuses flags that do not apply to `target`, so a mistyped command fails
/// instead of saving something other than what was asked for.
fn check_flags(target: &str, args: &HardwareSetupArgs) -> Result<()> {
    let is_local = target == LOCAL_HARDWARE_ID;
    let is_custom = target == CUSTOM_HARDWARE_TYPE;
    if (args.auto || args.manual) && !is_local {
        bail!("--auto and --manual apply to `local` only");
    }
    if args.id.is_some() && is_local {
        bail!("`local` always has the id 'local'; --id does not apply");
    }
    let takes_kind = is_custom || (is_local && !args.auto);
    if args.kind.is_some() && !takes_kind {
        bail!("--kind applies to `custom` and `local --manual` only");
    }
    if args.memory.is_some() && is_local && args.auto {
        bail!("--memory does not apply to `local --auto`, which detects the figures");
    }
    Ok(())
}

fn can_prompt(ctx: &AppContext) -> bool {
    ctx.ui.is_interactive()
}

/// Whether `hardware setup` goes on to save profile `id`: yes when no
/// profile has that id or `force` is set. Otherwise an interactive run asks,
/// as the other `setup` commands do, and any other run is refused. `local`
/// is not checked here: every command that uses it writes it, and
/// `setup local` is how it is changed.
fn replace_existing(ctx: &AppContext, id: &str, force: bool, interactive: bool) -> Result<bool> {
    if force || ctx.config().get_hardware(id).is_none() {
        return Ok(true);
    }
    if !interactive {
        bail!(
            "hardware profile '{id}' is already configured; run this command from a terminal \
             to replace it, or remove it first with `granite-cli hardware remove {id}`"
        );
    }
    let replace = ctx.ui.confirm(
        &format!("Hardware profile '{id}' is already configured. Overwrite?"),
        false,
    )?;
    if !replace {
        ctx.ui.info("Hardware setup skipped.");
    }
    Ok(replace)
}

fn not_configured(ctx: &AppContext, id: &str) -> anyhow::Error {
    let mut ids: Vec<&str> = ctx.config().hardware.keys().map(String::as_str).collect();
    ids.sort();
    anyhow!(
        "hardware profile '{id}' is not configured; configured: {}",
        ids.join(", ")
    )
}

/// The ids of the configured hardware profiles, sorted, with `local` listed
/// even before it is written, since every command that uses it writes it.
fn configured_ids(ctx: &AppContext) -> Vec<String> {
    let mut ids: Vec<String> = ctx.config().hardware.keys().cloned().collect();
    if !ids.iter().any(|id| id == LOCAL_HARDWARE_ID) {
        ids.push(LOCAL_HARDWARE_ID.to_string());
    }
    ids.sort();
    ids
}

/// The `hardware setup` command that creates profile `id`, with the flags a
/// run without a terminal needs: the size for a catalog entry sold in
/// several sizes, and the kind and figure for a custom profile.
fn setup_command(id: &str) -> String {
    match hardware_catalog::find(id) {
        Some(entry) if entry.sizes.len() > 1 => {
            let sizes: Vec<String> = entry.sizes.iter().map(|s| format_gb(s.memory_gb)).collect();
            format!(
                "granite-cli hardware setup {id} --memory <{}>",
                sizes.join("|")
            )
        }
        Some(_) => format!("granite-cli hardware setup {id}"),
        None => format!(
            "granite-cli hardware setup custom --id {id} --kind <discrete|unified|system> \
             --memory <GB>"
        ),
    }
}

/// See [`HardwareCommands::providers_on`].
fn providers_on(ctx: &AppContext, id: &str) -> Vec<String> {
    if id != LOCAL_HARDWARE_ID {
        return Vec::new();
    }
    let mut ids: Vec<String> = ctx.config().providers.keys().cloned().collect();
    ids.sort();
    ids
}

/// NOTES for a profile in `hardware list`. `unused` is as for
/// [`HardwareCommands::info_detail`]; it applies to `local` only.
fn notes(hardware: &HardwareConfig, unused: Option<&HardwareProfile>) -> String {
    let mut notes = vec![];
    if let Some(local) = hardware.local_settings() {
        notes.push(match (local.mode, local.detected) {
            (LocalMode::Auto, _) => match unused {
                Some(detected) => {
                    format!("auto; detected {} (not used)", detected.memory_summary())
                }
                None => "auto-detected".to_string(),
            },
            // `hardware info` shows the same detection: this run's when it
            // differs from the recorded one, otherwise the recorded one.
            (LocalMode::Manual, recorded) => match unused.or(recorded.as_ref()) {
                Some(detected) => format!("manual; detected {}", detected.memory_summary()),
                None => "manual".to_string(),
            },
        });
    }
    if let Some(factor) = hardware.headroom {
        notes.push(format!("headroom {}", format_factor(factor)));
    }
    notes.join("; ")
}

/// `n` followed by the singular or plural noun, such as "1 profile".
fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// The headroom factor in use, and whether it is the default for the kind.
fn headroom_label(profile: &HardwareProfile) -> String {
    match profile.headroom {
        Some(factor) => format!("÷ {} (set)", format_factor(factor)),
        None => format!(
            "÷ {} (default for {} memory)",
            format_factor(profile.memory_kind().default_headroom()),
            profile.memory_kind().name()
        ),
    }
}

/// The headroom factor to save: `flag` when given; otherwise asked for when
/// the run is interactive, starting from the factor the profile already has
/// (`kept`) or the kind's default; otherwise the kind's default. A factor
/// equal to the kind's default is not saved.
fn headroom(
    ctx: &AppContext,
    flag: Option<f64>,
    kept: Option<f64>,
    kind: MemoryKind,
    interactive: bool,
) -> Result<Option<f64>> {
    let default = kind.default_headroom();
    let factor = match (flag, interactive) {
        (Some(factor), _) => factor,
        (None, true) => {
            let start = format_factor(kept.unwrap_or(default));
            let answer = ctx.ui.text("Headroom factor", &start)?;
            answer
                .trim()
                .parse::<f64>()
                .map_err(|_| anyhow!("'{answer}' is not a headroom factor"))?
        }
        (None, false) => return Ok(None),
    };
    if !factor.is_finite() || factor < 1.0 {
        bail!("the headroom factor must be at least 1.0, got {factor}");
    }
    Ok((factor != default).then_some(factor))
}

/// A headroom factor with at least one decimal, such as "2.0" or "1.25".
fn format_factor(factor: f64) -> String {
    if factor.fract() == 0.0 {
        format!("{factor:.1}")
    } else {
        factor.to_string()
    }
}

/// The catalog family `name` refers to, ignoring case.
fn find_family(name: &str) -> Result<&'static str> {
    let families = families();
    families
        .iter()
        .find(|f| f.eq_ignore_ascii_case(name.trim()))
        .copied()
        .ok_or_else(|| {
            anyhow!(
                "no hardware family '{name}'; the families are: {}",
                families.join(", ")
            )
        })
}

/// The catalog's families, in the order the catalog lists them.
fn families() -> Vec<&'static str> {
    let mut families: Vec<&'static str> = vec![];
    for e in HARDWARE_CATALOG.iter() {
        if !families.contains(&e.family.as_str()) {
            families.push(&e.family);
        }
    }
    families
}

fn mode_label(mode: LocalMode) -> &'static str {
    match mode {
        LocalMode::Auto => "auto (detected on the commands that estimate fit)",
        LocalMode::Manual => "manual (stored figures kept on this machine)",
    }
}

/// An entry's sizes, such as "unified 36, 48, 64, 128 GB".
fn catalog_memory(entry: &CatalogEntry) -> String {
    let sizes: Vec<String> = entry.sizes.iter().map(|s| format_gb(s.memory_gb)).collect();
    format!("{} {} GB", entry.memory_kind.name(), sizes.join(", "))
}

fn format_gb(gb: f64) -> String {
    if gb.fract() == 0.0 {
        format!("{gb:.0}")
    } else {
        gb.to_string()
    }
}

/// Asks for the family, then the device in it. A family with one device
/// skips the second question, and "custom" ends the family list.
fn choose_target(ctx: &AppContext) -> Result<String> {
    let families = families();
    let mut items: Vec<String> = families.iter().map(|f| f.to_string()).collect();
    items.push("custom (enter the figures)".to_string());
    let index = ctx.ui.select("Hardware family", &items, 0)?;
    let Some(family) = families.get(index) else {
        return Ok(CUSTOM_HARDWARE_TYPE.to_string());
    };
    let entries: Vec<&CatalogEntry> = HARDWARE_CATALOG
        .iter()
        .filter(|e| e.family == *family)
        .collect();
    if let [entry] = entries.as_slice() {
        return Ok(entry.id.clone());
    }
    let items: Vec<String> = entries
        .iter()
        .map(|e| format!("{} ({})", e.id, e.name))
        .collect();
    let index = ctx.ui.select("Hardware type", &items, 0)?;
    Ok(entries[index].id.clone())
}

fn catalog_size(
    ctx: &AppContext,
    entry: &CatalogEntry,
    memory: Option<f64>,
    interactive: bool,
) -> Result<f64> {
    let offered: Vec<f64> = entry.sizes.iter().map(|s| s.memory_gb).collect();
    let listed = || {
        offered
            .iter()
            .map(|gb| format_gb(*gb))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match memory {
        Some(gb) if entry.size(gb).is_some() => Ok(gb),
        Some(gb) => bail!(
            "'{}' is not offered with {} GB; choose one of {} GB",
            entry.id,
            format_gb(gb),
            listed()
        ),
        None if offered.len() == 1 => Ok(offered[0]),
        None if interactive => {
            let items: Vec<String> = offered
                .iter()
                .map(|gb| format!("{} GB", format_gb(*gb)))
                .collect();
            Ok(offered[ctx.ui.select("Memory", &items, 0)?])
        }
        None => bail!(
            "'{}' is sold in several sizes; pass --memory with one of {} GB",
            entry.id,
            listed()
        ),
    }
}

/// The id to save a new profile under: `--id`, or asked for, defaulting to
/// the catalog id. See [`check_profile_id`] for the ids accepted.
fn profile_id(
    ctx: &AppContext,
    args: &HardwareSetupArgs,
    default: Option<&str>,
    interactive: bool,
) -> Result<String> {
    let id = match (&args.id, default) {
        (Some(id), _) => id.clone(),
        (None, default) if interactive => ctx.ui.text("Profile id", default.unwrap_or(""))?,
        (None, Some(default)) => default.to_string(),
        (None, None) => bail!("pass --id with the name to save the profile under"),
    };
    let id = id.trim().to_string();
    check_profile_id(&id)?;
    Ok(id)
}

/// Refuses an id that `hardware setup` cannot save a new profile under. An id
/// starts with a letter or digit and uses only letters, digits, `-`, `_` and
/// `.`, so it can be typed in a command and used as a file name as it is.
/// `local` and `custom` are reserved.
fn check_profile_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("a hardware profile needs an id");
    }
    if id == LOCAL_HARDWARE_ID || id == CUSTOM_HARDWARE_TYPE {
        bail!("'{id}' is reserved; choose another id");
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if !id.starts_with(|c: char| c.is_ascii_alphanumeric()) || !id.chars().all(allowed) {
        bail!(
            "'{id}' cannot be a hardware profile id; use letters, digits, '-', '_' and '.', \
             starting with a letter or digit"
        );
    }
    Ok(())
}

/// A profile from figures the user gives: a memory kind and the figure it
/// names.
fn typed_figures(
    ctx: &AppContext,
    args: &HardwareSetupArgs,
    interactive: bool,
) -> Result<HardwareProfile> {
    let kind = match args.kind {
        Some(kind) => kind,
        None if interactive => {
            let kinds = [
                MemoryKind::Discrete,
                MemoryKind::Unified,
                MemoryKind::System,
            ];
            let items = [
                "discrete (a card with its own VRAM)".to_string(),
                "unified (one pool shared by CPU and GPU)".to_string(),
                "system (no accelerator, system RAM)".to_string(),
            ];
            kinds[ctx.ui.select("Memory kind", &items, 0)?]
        }
        None => bail!("pass --kind discrete, unified or system"),
    };
    let (question, flag) = match kind {
        MemoryKind::Discrete => ("VRAM (GB)", "the VRAM in GB"),
        MemoryKind::Unified => ("Memory the GPU can use (GB)", "the GB the GPU can use"),
        MemoryKind::System => ("System RAM (GB)", "the system RAM in GB"),
    };
    let gb = match args.memory {
        Some(gb) => gb,
        None if interactive => {
            let answer = ctx.ui.text(question, "")?;
            answer
                .trim()
                .parse::<f64>()
                .map_err(|_| anyhow!("'{answer}' is not a number of GB"))?
        }
        None => bail!("pass --memory with {flag}"),
    };
    let profile = HardwareProfile::with_memory(match kind {
        MemoryKind::Discrete => Memory::Discrete {
            vram_gb: gb,
            ram_gb: None,
        },
        MemoryKind::Unified => Memory::Unified {
            vram_gb: gb,
            ram_gb: None,
        },
        MemoryKind::System => Memory::System { ram_gb: gb },
    });
    profile.check_figures().map_err(anyhow::Error::msg)?;
    Ok(profile)
}

/// `setup local`: take detected figures (`--auto`), or typed figures kept on
/// this machine (`--manual`). Both detect: manual mode records what
/// detection read, so another machine is asked about.
fn setup_local(
    ctx: &AppContext,
    args: &HardwareSetupArgs,
    interactive: bool,
    detect: impl FnOnce() -> HardwareProfile,
) -> Result<HardwareConfig> {
    let auto = match (args.auto, args.manual) {
        (true, true) => bail!("--auto and --manual cannot be used together"),
        (true, false) => true,
        (false, true) => false,
        (false, false) if interactive => {
            let items = [
                "Detect this machine's hardware (auto)".to_string(),
                "Enter the figures for this machine (manual)".to_string(),
            ];
            ctx.ui.select("Local hardware", &items, 0)? == 0
        }
        (false, false) => bail!("pass --auto or --manual"),
    };
    if auto {
        return Ok(HardwareConfig::local_from_detection(detect()));
    }
    let detected = saved_figures(detect());
    let typed = typed_figures(ctx, args, interactive)?;
    // Keep what detection knows about the machine for display.
    let profile = HardwareProfile {
        machine: detected.machine.clone(),
        gpu_vendor: detected.gpu_vendor.clone(),
        ..typed
    };
    Ok(HardwareConfig::local(LocalHardware {
        mode: LocalMode::Manual,
        profile,
        detected: Some(detected),
    }))
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, TestConfigHome};
    use crate::utils::ui::base::tests::CaptureUi;

    fn figures(memory: Memory) -> HardwareProfile {
        let mut profile = HardwareProfile::with_memory(memory);
        profile.gpu_vendor = Some("Apple".to_string());
        profile.machine = MachineInfo {
            os: "macos".to_string(),
            cpu_cores: Some(16),
            cpu_arch: "aarch64".to_string(),
        };
        profile
    }

    fn mac(vram_gb: f64, ram_gb: f64) -> HardwareProfile {
        figures(Memory::Unified {
            vram_gb,
            ram_gb: Some(ram_gb),
        })
    }

    fn mac_36() -> HardwareProfile {
        mac(29.16, 36.0)
    }

    fn mac_48() -> HardwareProfile {
        mac(38.88, 48.0)
    }

    fn mac_64() -> HardwareProfile {
        mac(51.84, 64.0)
    }

    /// The index of `family` in the setup family question.
    fn family_index(family: &str) -> usize {
        families().iter().position(|f| *f == family).unwrap()
    }

    fn ctx(ui: &Arc<CaptureUi>) -> AppContext {
        AppContext::new(Config::new().unwrap(), ui.clone())
    }

    /// A context whose `local` holds `saved`, with the notice cleared.
    fn ctx_with_local(ui: &Arc<CaptureUi>, saved: HardwareProfile) -> AppContext {
        let mut ctx = ctx(ui);
        HardwareCommands::check_local(&mut ctx, || saved, true).unwrap();
        ui.infos.borrow_mut().clear();
        ctx
    }

    fn local(ctx: &AppContext) -> LocalHardware {
        ctx.config()
            .get_hardware(LOCAL_HARDWARE_ID)
            .unwrap()
            .local_settings()
            .unwrap()
    }

    fn setup(ctx: &mut AppContext, args: HardwareSetupArgs, interactive: bool) -> Result<String> {
        HardwareCommands::setup_with(ctx, args, interactive, || panic!("detection must not run"))
    }

    // -- subcommand output -------------------------------------------------------

    #[test]
    fn catalog_lists_every_entry_with_its_sizes() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        HardwareCommands::catalog(&ctx(&ui), None).unwrap();

        let tables = ui.tables.borrow();
        let (title, headers, rows) = &tables[0];
        assert_eq!(
            title,
            &format!("Hardware Catalog ({} entries)", HARDWARE_CATALOG.len())
        );
        assert_eq!(headers, &["FAMILY", "ID", "NAME", "MEMORY"]);
        let m4_max = rows.iter().find(|r| r[1] == "apple-m4-max").unwrap();
        assert_eq!(m4_max[0], "Apple Silicon");
        assert_eq!(m4_max[3], "unified 36, 48, 64, 128 GB");
    }

    #[test]
    fn catalog_is_ordered_by_family() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        HardwareCommands::catalog(&ctx(&ui), None).unwrap();

        let tables = ui.tables.borrow();
        let mut seen: Vec<&str> = vec![];
        for row in &tables[0].2 {
            if seen.last() != Some(&row[0].as_str()) {
                assert!(!seen.contains(&row[0].as_str()), "{} split", row[0]);
                seen.push(&row[0]);
            }
        }
        assert_eq!(seen, families());
    }

    #[test]
    fn catalog_family_matches_regardless_of_case() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        HardwareCommands::catalog(&ctx(&ui), Some("nvidia geforce")).unwrap();

        let tables = ui.tables.borrow();
        let (title, _, rows) = &tables[0];
        assert_eq!(title, "Hardware Catalog (5 entries)");
        assert!(rows.iter().all(|r| r[0] == "NVIDIA GeForce"));
    }

    #[test]
    fn catalog_with_an_unknown_family_lists_the_families() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let err = HardwareCommands::catalog(&ctx(&ui), Some("Intel")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "no hardware family 'Intel'; the families are: Apple Silicon, NVIDIA GeForce, \
             NVIDIA Workstation, NVIDIA Data Center, AMD, CPU only"
        );
    }

    #[test]
    fn list_shows_local_first_with_its_providers_and_mode() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ctx.config_mut()
            .insert_hardware(
                "spark",
                HardwareConfig::catalog("spark", "dgx-spark", 128.0),
            )
            .unwrap();
        ctx.config_mut()
            .insert_provider(
                "ollama",
                crate::config::ProviderConfig {
                    provider_id: "ollama".to_string(),
                    provider_type: "ollama".to_string(),
                    config: serde_json::json!({}),
                },
            )
            .unwrap();

        HardwareCommands::list_table(&ctx, None);

        let tables = ui.tables.borrow();
        let (title, _, rows) = &tables[0];
        assert_eq!(title, "Configured Hardware (2 profiles)");
        assert_eq!(
            rows[0],
            vec![
                "local",
                "local",
                "unified 29.2 GB",
                "ollama",
                "auto-detected"
            ]
        );
        assert_eq!(
            rows[1],
            vec!["spark", "dgx-spark", "unified 128.0 GB", "", ""]
        );
    }

    #[test]
    fn info_shows_a_catalog_profile_with_its_device_and_usable_memory() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        ctx.config_mut()
            .insert_hardware(
                "studio",
                HardwareConfig::catalog("studio", "apple-m4-max", 64.0),
            )
            .unwrap();

        HardwareCommands::info_detail(&ctx, "studio", None).unwrap();

        let details = ui.details.borrow();
        let (title, fields) = &details[0];
        assert_eq!(title, "Hardware Profile 'studio'");
        let field = |k: &str| {
            fields
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(field("Device").as_deref(), Some("Apple M4 Max"));
        assert_eq!(field("Memory").as_deref(), Some("unified 51.8 GB"));
        assert_eq!(field("Usable for models").as_deref(), Some("34.56 GB"));
    }

    #[test]
    fn info_on_an_unknown_id_lists_the_configured_ones() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let ctx = ctx_with_local(&ui, mac_36());

        let err = HardwareCommands::info_detail(&ctx, "spark", None).unwrap_err();

        assert_eq!(
            err.to_string(),
            "hardware profile 'spark' is not configured; configured: local"
        );
    }

    #[test]
    fn bare_hardware_shows_local_and_its_mode() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        HardwareCommands::show_with(&mut ctx, mac_36, false).unwrap();

        let details = ui.details.borrow();
        let (title, fields) = &details[0];
        assert_eq!(title, "Hardware Profile 'local'");
        assert!(
            fields
                .iter()
                .any(|(k, v)| *k == "Mode" && v.starts_with("auto"))
        );
    }

    #[test]
    fn remove_drops_a_profile_and_refuses_local() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ctx.config_mut()
            .insert_hardware(
                "spark",
                HardwareConfig::catalog("spark", "dgx-spark", 128.0),
            )
            .unwrap();

        HardwareCommands::remove(&mut ctx, "spark").unwrap();
        assert!(ctx.config().get_hardware("spark").is_none());
        assert!(HardwareCommands::remove(&mut ctx, LOCAL_HARDWARE_ID).is_err());
    }

    // -- setup -------------------------------------------------------------------

    #[test]
    fn setup_with_scripted_answers_writes_a_catalog_profile() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let m4_max = HARDWARE_CATALOG
            .iter()
            .filter(|e| e.family == "Apple Silicon")
            .position(|e| e.id == "apple-m4-max")
            .unwrap();
        // Family, hardware type, then the 128 GB size (fourth of four). The
        // headroom question keeps its default.
        ui.select_answers
            .borrow_mut()
            .extend([family_index("Apple Silicon"), m4_max, 3]);
        ui.text_answers.borrow_mut().push_back("studio".to_string());

        let id = setup(&mut ctx, HardwareSetupArgs::default(), true).unwrap();

        assert_eq!(id, "studio");
        assert_eq!(
            ctx.config().get_hardware("studio"),
            Some(&HardwareConfig::catalog("studio", "apple-m4-max", 128.0))
        );
        assert!(ui.oks.borrow()[0].starts_with("✓ Hardware profile 'studio' configured"));
        let asked: Vec<String> = ui
            .select_prompts
            .borrow()
            .iter()
            .map(|(prompt, _, _)| prompt.clone())
            .collect();
        assert_eq!(asked, ["Hardware family", "Hardware type", "Memory"]);
        assert_eq!(
            ui.text_prompts.borrow().last().cloned(),
            Some(("Headroom factor".to_string(), "1.5".to_string()))
        );
    }

    #[test]
    fn setup_skips_the_device_question_for_a_family_with_one_entry() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        // CPU only has one entry, sold in several sizes: the 64 GB one.
        let cpu = hardware_catalog::find("cpu-only").unwrap();
        let size = cpu.sizes.iter().position(|s| s.memory_gb == 64.0).unwrap();
        ui.select_answers
            .borrow_mut()
            .extend([family_index("CPU only"), size]);
        ui.text_answers.borrow_mut().push_back("box".to_string());

        setup(&mut ctx, HardwareSetupArgs::default(), true).unwrap();

        assert_eq!(
            ctx.config().get_hardware("box"),
            Some(&HardwareConfig::catalog("box", "cpu-only", 64.0))
        );
        assert!(
            !ui.select_prompts
                .borrow()
                .iter()
                .any(|(prompt, _, _)| prompt == "Hardware type")
        );
    }

    #[test]
    fn setup_headroom_flag_writes_the_factor() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = HardwareSetupArgs {
            target: Some("apple-m4-max".to_string()),
            memory: Some(128.0),
            id: Some("studio".to_string()),
            headroom: Some(1.2),
            ..Default::default()
        };

        setup(&mut ctx, args, false).unwrap();

        assert_eq!(
            ctx.config().get_hardware("studio").unwrap().headroom,
            Some(1.2)
        );
        assert_eq!(ctx.hardware_profile("studio").unwrap().headroom, Some(1.2));
    }

    #[test]
    fn setup_refuses_a_headroom_factor_below_one() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let args = HardwareSetupArgs {
            target: Some("dgx-spark".to_string()),
            headroom: Some(0.8),
            ..Default::default()
        };

        let err = setup(&mut ctx(&ui), args, false).unwrap_err();

        assert_eq!(
            err.to_string(),
            "the headroom factor must be at least 1.0, got 0.8"
        );
    }

    #[test]
    fn interactive_headroom_is_saved_only_when_it_differs_from_the_default() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = |id: &str| HardwareSetupArgs {
            target: Some("dgx-spark".to_string()),
            id: Some(id.to_string()),
            ..Default::default()
        };
        ui.text_answers
            .borrow_mut()
            .extend(["1.5".to_string(), "1.25".to_string()]);

        setup(&mut ctx, args("same"), true).unwrap();
        setup(&mut ctx, args("set"), true).unwrap();

        assert_eq!(ctx.config().get_hardware("same").unwrap().headroom, None);
        assert_eq!(
            ctx.config().get_hardware("set").unwrap().headroom,
            Some(1.25)
        );
    }

    #[test]
    fn setup_in_a_script_takes_memory_and_defaults_the_id() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = HardwareSetupArgs {
            target: Some("nvidia-a100".to_string()),
            memory: Some(80.0),
            ..Default::default()
        };

        let id = setup(&mut ctx, args, false).unwrap();

        assert_eq!(
            ctx.config().get_hardware(&id),
            Some(&HardwareConfig::catalog("nvidia-a100", "nvidia-a100", 80.0))
        );
    }

    #[test]
    fn setup_in_a_script_without_a_size_lists_the_sizes() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let args = HardwareSetupArgs {
            target: Some("nvidia-a100".to_string()),
            ..Default::default()
        };

        let err = setup(&mut ctx(&ui), args, false).unwrap_err();

        assert_eq!(
            err.to_string(),
            "'nvidia-a100' is sold in several sizes; pass --memory with one of 40, 80 GB"
        );
    }

    #[test]
    fn setup_custom_writes_the_typed_figures() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = HardwareSetupArgs {
            target: Some(CUSTOM_HARDWARE_TYPE.to_string()),
            id: Some("lab-box".to_string()),
            kind: Some(MemoryKind::Discrete),
            memory: Some(48.0),
            ..Default::default()
        };

        setup(&mut ctx, args, false).unwrap();

        let p = ctx.hardware_profile("lab-box").unwrap();
        assert_eq!(
            p.memory,
            Memory::Discrete {
                vram_gb: 48.0,
                ram_gb: None
            }
        );
    }

    #[test]
    fn setup_refuses_a_reserved_id() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let args = HardwareSetupArgs {
            target: Some("dgx-spark".to_string()),
            id: Some(LOCAL_HARDWARE_ID.to_string()),
            ..Default::default()
        };

        let err = setup(&mut ctx(&ui), args, false).unwrap_err();

        assert_eq!(err.to_string(), "'local' is reserved; choose another id");
    }

    #[test]
    fn setup_refuses_an_id_that_is_not_a_plain_name() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        for id in ["../evil", "Bad Id", ".hidden", "-x"] {
            let args = HardwareSetupArgs {
                target: Some("dgx-spark".to_string()),
                id: Some(id.to_string()),
                ..Default::default()
            };

            let err = setup(&mut ctx(&ui), args, false).unwrap_err();

            assert_eq!(
                err.to_string(),
                format!(
                    "'{id}' cannot be a hardware profile id; use letters, digits, '-', '_' and \
                     '.', starting with a letter or digit"
                )
            );
        }
        let args = HardwareSetupArgs {
            target: Some("dgx-spark".to_string()),
            id: Some("lab_box-2.a".to_string()),
            ..Default::default()
        };
        assert_eq!(setup(&mut ctx(&ui), args, false).unwrap(), "lab_box-2.a");
    }

    #[test]
    fn setup_on_an_existing_id_asks_before_replacing_it() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = |memory: f64| HardwareSetupArgs {
            target: Some(CUSTOM_HARDWARE_TYPE.to_string()),
            id: Some("lab-box".to_string()),
            kind: Some(MemoryKind::Discrete),
            memory: Some(memory),
            headroom: Some(1.5),
            ..Default::default()
        };
        setup(&mut ctx, args(48.0), true).unwrap();
        ui.confirm_answers.borrow_mut().extend([false, true]);

        setup(&mut ctx, args(24.0), true).unwrap();
        let vram = |ctx: &AppContext| ctx.hardware_profile("lab-box").unwrap().memory.vram_gb();
        assert_eq!(vram(&ctx), Some(48.0));
        assert!(
            ui.infos
                .borrow()
                .contains(&"Hardware setup skipped.".to_string())
        );

        setup(&mut ctx, args(24.0), true).unwrap();
        assert_eq!(vram(&ctx), Some(24.0));
        let asked: Vec<String> = ui
            .confirm_prompts
            .borrow()
            .iter()
            .map(|(prompt, default)| format!("{prompt} {default}"))
            .collect();
        assert_eq!(
            asked,
            ["Hardware profile 'lab-box' is already configured. Overwrite? false"; 2]
        );
    }

    #[test]
    fn setup_on_an_existing_id_is_refused_without_a_terminal_unless_forced() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = |memory: f64, force_overwrite: bool| HardwareSetupArgs {
            target: Some("apple-m4-max".to_string()),
            id: Some("studio".to_string()),
            memory: Some(memory),
            force_overwrite,
            ..Default::default()
        };
        setup(&mut ctx, args(128.0, false), false).unwrap();

        let err = setup(&mut ctx, args(64.0, false), false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "hardware profile 'studio' is already configured; run this command from a terminal \
             to replace it, or remove it first with `granite-cli hardware remove studio`"
        );
        assert_eq!(
            ctx.config().get_hardware("studio"),
            Some(&HardwareConfig::catalog("studio", "apple-m4-max", 128.0))
        );

        setup(&mut ctx, args(64.0, true), false).unwrap();
        assert_eq!(
            ctx.config().get_hardware("studio"),
            Some(&HardwareConfig::catalog("studio", "apple-m4-max", 64.0))
        );
        assert!(ui.confirm_prompts.borrow().is_empty());
    }

    #[test]
    fn a_run_that_cannot_ask_saves_the_default_headroom_without_the_flag() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        let args = |headroom: Option<f64>| HardwareSetupArgs {
            target: Some("dgx-spark".to_string()),
            id: Some("spark".to_string()),
            headroom,
            force_overwrite: true,
            ..Default::default()
        };
        setup(&mut ctx, args(Some(1.2)), false).unwrap();

        setup(&mut ctx, args(None), false).unwrap();

        assert_eq!(ctx.config().get_hardware("spark").unwrap().headroom, None);
        assert!(ui.text_prompts.borrow().is_empty());
    }

    #[test]
    fn setup_local_manual_keeps_typed_figures_and_records_the_detection() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        let args = HardwareSetupArgs {
            target: Some(LOCAL_HARDWARE_ID.to_string()),
            manual: true,
            kind: Some(MemoryKind::Discrete),
            memory: Some(96.0),
            ..Default::default()
        };

        HardwareCommands::setup_with(&mut ctx, args, false, mac_36).unwrap();

        let saved = local(&ctx);
        assert_eq!(saved.mode, LocalMode::Manual);
        assert_eq!(saved.profile.memory.vram_gb(), Some(96.0));
        assert_eq!(saved.profile.machine.os, "macos");
        assert!(saved.detected.unwrap().same_memory(&mac_36()));

        // The machine the figures were typed on is not asked about.
        HardwareCommands::check_local(&mut ctx, mac_36, true).unwrap();
        assert!(ui.select_prompts.borrow().is_empty());
    }

    #[test]
    fn setup_local_auto_takes_the_detected_figures_and_leaves_manual_mode() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.select_answers.borrow_mut().push_back(2);
        HardwareCommands::check_local(&mut ctx, mac_64, true).unwrap();
        assert_eq!(local(&ctx).mode, LocalMode::Manual);
        let args = HardwareSetupArgs {
            target: Some(LOCAL_HARDWARE_ID.to_string()),
            auto: true,
            ..Default::default()
        };

        HardwareCommands::setup_with(&mut ctx, args, false, mac_48).unwrap();

        let saved = local(&ctx);
        assert_eq!(saved.mode, LocalMode::Auto);
        assert!(saved.profile.same_memory(&mac_48()));
        assert_eq!(saved.detected, None);
    }

    // -- checking local against detection ----------------------------------------

    #[test]
    fn same_hardware_asks_nothing() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        HardwareCommands::check_local(&mut ctx, mac_36, true).unwrap();

        assert!(ui.select_prompts.borrow().is_empty());
        assert!(ui.warns.borrow().is_empty());
    }

    #[test]
    fn choosing_update_saves_the_detected_figures() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.select_answers.borrow_mut().push_back(0);

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        assert_eq!(
            ui.warns.borrow()[0],
            "Local hardware differs from profile 'local':\n    stored    unified 29.2 GB\n    \
             detected  unified 38.9 GB"
        );
        assert!(local(&ctx).profile.same_memory(&mac_48()));
        assert_eq!(
            ctx.hardware_profile(LOCAL_HARDWARE_ID)
                .unwrap()
                .memory
                .vram_gb(),
            Some(38.88)
        );
    }

    #[test]
    fn updating_local_to_the_detected_figures_keeps_its_factor() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        let mut saved = ctx
            .config()
            .get_hardware(LOCAL_HARDWARE_ID)
            .cloned()
            .unwrap();
        saved.headroom = Some(1.2);
        ctx.config_mut()
            .insert_hardware(LOCAL_HARDWARE_ID, saved)
            .unwrap();
        ui.select_answers.borrow_mut().push_back(0);

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        assert!(local(&ctx).profile.same_memory(&mac_48()));
        assert_eq!(
            ctx.config()
                .get_hardware(LOCAL_HARDWARE_ID)
                .unwrap()
                .headroom,
            Some(1.2)
        );
    }

    #[test]
    fn info_and_list_show_the_headroom_factor() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ctx.config_mut()
            .insert_hardware(
                "studio",
                HardwareConfig {
                    headroom: Some(1.2),
                    ..HardwareConfig::catalog("studio", "apple-m4-max", 64.0)
                },
            )
            .unwrap();

        HardwareCommands::info_detail(&ctx, "studio", None).unwrap();
        HardwareCommands::info_detail(&ctx, LOCAL_HARDWARE_ID, None).unwrap();
        HardwareCommands::list_table(&ctx, None);

        let details = ui.details.borrow();
        let headroom = |i: usize| {
            details[i]
                .1
                .iter()
                .find(|(k, _)| k == "Headroom")
                .map(|(_, v)| v.clone())
        };
        assert_eq!(headroom(0).as_deref(), Some("÷ 1.2 (set)"));
        assert_eq!(
            headroom(1).as_deref(),
            Some("÷ 1.5 (default for unified memory)")
        );
        let tables = ui.tables.borrow();
        let studio = tables[0].2.iter().find(|r| r[0] == "studio").unwrap();
        assert_eq!(studio.last().unwrap(), "headroom 1.2");
    }

    #[test]
    fn choosing_keep_for_now_saves_nothing_and_asks_again() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        let before = ctx.config().get_hardware(LOCAL_HARDWARE_ID).cloned();
        ui.select_answers.borrow_mut().extend([1, 1]);

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();
        assert_eq!(
            ctx.config().get_hardware(LOCAL_HARDWARE_ID).cloned(),
            before
        );

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();
        assert_eq!(ui.select_prompts.borrow().len(), 2);
    }

    #[test]
    fn list_and_info_show_a_detection_this_run_did_not_take() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        let unused = HardwareCommands::compare_local(&mut ctx, mac_48, false).unwrap();
        HardwareCommands::list_table(&ctx, unused.as_ref());
        HardwareCommands::info_detail(&ctx, LOCAL_HARDWARE_ID, unused.as_ref()).unwrap();

        assert!(unused.is_some_and(|d| d.same_memory(&mac_48())));
        let tables = ui.tables.borrow();
        let (title, _, rows) = &tables[0];
        assert_eq!(title, "Configured Hardware (1 profile)");
        assert_eq!(rows[0][4], "auto; detected unified 38.9 GB (not used)");
        let details = ui.details.borrow();
        let detected = details[0].1.iter().find(|(k, _)| k == "Detected").unwrap();
        assert_eq!(detected.1, "unified 38.9 GB (not used)");
    }

    #[test]
    fn a_detection_that_is_taken_or_recorded_is_not_returned() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        // Update, then: the same detection again.
        ui.select_answers.borrow_mut().push_back(0);

        let updated = HardwareCommands::compare_local(&mut ctx, mac_48, true).unwrap();
        let same = HardwareCommands::compare_local(&mut ctx, mac_48, false).unwrap();

        assert!(updated.is_none());
        assert!(same.is_none());
    }

    #[test]
    fn choosing_manual_keeps_the_figures_for_this_machine_only() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.select_answers.borrow_mut().push_back(2);

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        let saved = local(&ctx);
        assert_eq!(saved.mode, LocalMode::Manual);
        assert!(saved.profile.same_memory(&mac_36()));
        assert!(saved.detected.unwrap().same_memory(&mac_48()));

        // The same machine: no prompt.
        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();
        assert_eq!(ui.select_prompts.borrow().len(), 1);

        // Another machine: asked again.
        ui.select_answers.borrow_mut().push_back(1);
        HardwareCommands::check_local(&mut ctx, mac_64, true).unwrap();
        assert_eq!(ui.select_prompts.borrow().len(), 2);
    }

    #[test]
    fn choosing_update_in_manual_mode_follows_detection_again() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.select_answers.borrow_mut().extend([2, 0]);
        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        HardwareCommands::check_local(&mut ctx, mac_64, true).unwrap();

        let saved = local(&ctx);
        assert_eq!(saved.mode, LocalMode::Auto);
        assert!(saved.profile.same_memory(&mac_64()));
        assert_eq!(saved.detected, None);
    }

    #[test]
    fn the_choices_name_this_session_and_dont_ask_again() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.select_answers.borrow_mut().push_back(1);

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        assert_eq!(
            ui.select_prompts.borrow().last().unwrap().1,
            [
                "Update 'local' to the detected figures",
                "Keep the stored figures for this session",
                "Keep the stored figures (don't ask again)",
            ]
        );
    }

    #[test]
    fn a_malformed_detected_block_is_an_error_that_names_it_or_a_warning() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        let mut local = ctx
            .config()
            .get_hardware(LOCAL_HARDWARE_ID)
            .cloned()
            .unwrap();
        local.config["mode"] = serde_json::json!("manual");
        local.config["detected"] = serde_json::json!({ "memory_kind": "unified" });
        ctx.config_mut()
            .insert_hardware(LOCAL_HARDWARE_ID, local)
            .unwrap();

        let err = HardwareCommands::check_local(&mut ctx, mac_36, true).unwrap_err();

        assert!(err.to_string().contains("`detected`"), "{err}");
        HardwareCommands::check_local_or_warn(&mut ctx, || panic!("detection must not run"), true);
        assert_eq!(
            *ui.stderr_warns.borrow(),
            vec![
                "Hardware profile 'local' cannot be read; run `granite-cli hardware setup local \
                 --auto` to replace it with the detected figures."
                    .to_string()
            ]
        );
    }

    #[test]
    fn manual_profile_without_a_recorded_detection_is_not_asked_about() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);
        ctx.config_mut()
            .insert_hardware(
                LOCAL_HARDWARE_ID,
                HardwareConfig::local(LocalHardware {
                    mode: LocalMode::Manual,
                    profile: mac_36(),
                    detected: None,
                }),
            )
            .unwrap();

        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        assert!(ui.select_prompts.borrow().is_empty());
        assert!(ui.warns.borrow().is_empty());
    }

    #[test]
    fn manual_profile_shows_the_detection_it_keeps_its_figures_over() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.select_answers.borrow_mut().push_back(2);
        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        HardwareCommands::info_detail(&ctx, LOCAL_HARDWARE_ID, None).unwrap();
        HardwareCommands::list_table(&ctx, None);

        let details = ui.details.borrow();
        let fields = &details[0].1;
        assert!(fields.contains(&(
            "Mode".to_string(),
            "manual (stored figures kept on this machine)".to_string()
        )));
        assert!(fields.contains(&(
            "Detected".to_string(),
            "unified 38.9 GB (not used)".to_string()
        )));
        let tables = ui.tables.borrow();
        let local_row = &tables[0].2[0];
        assert_eq!(
            local_row.last().unwrap(),
            "manual; detected unified 38.9 GB"
        );
    }

    #[test]
    fn list_and_info_agree_on_a_detection_that_differs_from_the_recorded_one() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        // Keep the stored figures over mac_48 and don't ask again.
        ui.select_answers.borrow_mut().push_back(2);
        HardwareCommands::check_local(&mut ctx, mac_48, true).unwrap();

        // Another machine: detection differs from the recorded one too.
        let unused = HardwareCommands::compare_local(&mut ctx, mac_64, false).unwrap();
        HardwareCommands::info_detail(&ctx, LOCAL_HARDWARE_ID, unused.as_ref()).unwrap();
        HardwareCommands::list_table(&ctx, unused.as_ref());

        let now = mac_64().memory_summary();
        let details = ui.details.borrow();
        let detected = details[0].1.iter().find(|(k, _)| k == "Detected").unwrap();
        assert_eq!(detected.1, format!("{now} (not used)"));
        let tables = ui.tables.borrow();
        assert_eq!(
            tables[0].2[0].last().unwrap(),
            &format!("manual; detected {now}")
        );
    }

    #[test]
    fn a_run_that_cannot_ask_keeps_the_saved_figures_and_warns_each_time() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        HardwareCommands::check_local(&mut ctx, mac_48, false).unwrap();
        HardwareCommands::check_local(&mut ctx, mac_48, false).unwrap();

        assert!(ui.select_prompts.borrow().is_empty());
        assert!(local(&ctx).profile.same_memory(&mac_36()));
        assert!(ui.warns.borrow().is_empty());
        let warns = ui.stderr_warns.borrow();
        assert_eq!(warns.len(), 2);
        assert_eq!(
            warns[0],
            "Local hardware differs from profile 'local' (stored unified 29.2 GB, detected \
             unified 38.9 GB).\nUsing the stored figures. Run `granite-cli hardware setup \
             local --auto` to update."
        );
    }

    #[test]
    fn a_json_run_sends_the_notice_and_the_warning_to_stderr_and_keeps_its_output_clean() {
        let _home = TestConfigHome::new();
        let json = Arc::new(crate::utils::ui::backends::json::JsonOutput::with_capture());
        let mut ctx = AppContext::new(Config::new().unwrap(), json.clone());
        assert!(!HardwareCommands::can_prompt(&ctx));

        HardwareCommands::check_local(&mut ctx, mac_36, false).unwrap();
        HardwareCommands::check_local(&mut ctx, mac_48, false).unwrap();

        assert!(json.captured().is_empty());
        let stderr = json.captured_stderr();
        assert_eq!(stderr.len(), 2);
        assert_eq!(stderr[0]["type"], "info");
        assert!(
            stderr[0]["message"]
                .as_str()
                .unwrap()
                .starts_with("Saved hardware profile 'local' from this machine")
        );
        assert_eq!(stderr[1]["type"], "warn");
        assert!(
            stderr[1]["message"]
                .as_str()
                .unwrap()
                .starts_with("Local hardware differs from profile 'local'")
        );
        assert!(local(&ctx).profile.same_memory(&mac_36()));
    }

    #[test]
    fn a_ram_change_under_a_discrete_gpu_is_not_a_difference() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let card = |ram| {
            figures(Memory::Discrete {
                vram_gb: 24.0,
                ram_gb: Some(ram),
            })
        };
        let mut ctx = ctx_with_local(&ui, card(64.0));

        HardwareCommands::check_local(&mut ctx, || card(128.0), true).unwrap();

        assert!(ui.select_prompts.borrow().is_empty());
    }

    #[test]
    fn info_on_a_new_machine_writes_local_first() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);

        HardwareCommands::check_local(&mut ctx, mac_36, false).unwrap();
        HardwareCommands::info_detail(&ctx, LOCAL_HARDWARE_ID, None).unwrap();

        assert_eq!(ui.details.borrow().len(), 1);
    }

    #[test]
    fn setup_refuses_flags_that_do_not_apply() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let cases = [
            (
                HardwareSetupArgs {
                    target: Some("dgx-spark".to_string()),
                    auto: true,
                    ..Default::default()
                },
                "--auto and --manual apply to `local` only",
            ),
            (
                HardwareSetupArgs {
                    target: Some(LOCAL_HARDWARE_ID.to_string()),
                    auto: true,
                    id: Some("box".to_string()),
                    ..Default::default()
                },
                "`local` always has the id 'local'; --id does not apply",
            ),
            (
                HardwareSetupArgs {
                    target: Some("dgx-spark".to_string()),
                    kind: Some(MemoryKind::System),
                    ..Default::default()
                },
                "--kind applies to `custom` and `local --manual` only",
            ),
        ];
        for (args, expected) in cases {
            let err = setup(&mut ctx(&ui), args, false).unwrap_err();
            assert_eq!(err.to_string(), expected);
        }
    }

    // -- choosing a hardware profile to estimate fit against ----------------------

    fn no_detection() -> HardwareProfile {
        panic!("detection must not run")
    }

    #[test]
    fn an_unknown_id_in_a_run_that_cannot_ask_lists_the_configured_ids() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        let err = HardwareCommands::profile_for_fit_with(&mut ctx, "spark", false, no_detection)
            .unwrap_err();

        assert_eq!(
            err.to_string(),
            "no hardware profile 'spark'. Configured: local.\n  Create it with `granite-cli \
             hardware setup custom --id spark --kind <discrete|unified|system> --memory <GB>`."
        );
    }

    #[test]
    fn an_unknown_id_that_cannot_be_a_profile_id_is_refused_without_a_suggestion() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        for (id, can_prompt) in [("custom", false), ("../evil", true)] {
            let err =
                HardwareCommands::profile_for_fit_with(&mut ctx, id, can_prompt, no_detection)
                    .unwrap_err();

            assert!(
                err.to_string()
                    .starts_with(&format!("no hardware profile '{id}', and '{id}'")),
                "{err}"
            );
        }
        assert!(ui.confirm_prompts.borrow().is_empty());
    }

    #[test]
    fn a_catalog_id_sold_in_several_sizes_is_suggested_with_its_sizes() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        let entry = HARDWARE_CATALOG.iter().find(|e| e.sizes.len() > 1).unwrap();
        let sizes: Vec<String> = entry.sizes.iter().map(|s| format_gb(s.memory_gb)).collect();

        let err = HardwareCommands::profile_for_fit_with(&mut ctx, &entry.id, false, no_detection)
            .unwrap_err();

        assert!(
            err.to_string().ends_with(&format!(
                "`granite-cli hardware setup {} --memory <{}>`.",
                entry.id,
                sizes.join("|")
            )),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_catalog_id_in_a_run_that_cannot_ask_names_its_setup_command_and_local() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(&ui);

        let err =
            HardwareCommands::profile_for_fit_with(&mut ctx, "dgx-spark", false, no_detection)
                .unwrap_err();

        assert_eq!(
            err.to_string(),
            "no hardware profile 'dgx-spark'. Configured: local.\n  Create it with `granite-cli \
             hardware setup dgx-spark`."
        );
    }

    #[test]
    fn local_gives_the_saved_local_figures() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());

        let (id, p) =
            HardwareCommands::profile_for_fit_with(&mut ctx, LOCAL_HARDWARE_ID, false, mac_36)
                .unwrap();

        assert_eq!(id, LOCAL_HARDWARE_ID);
        assert_eq!(p.memory, mac_36().memory);
    }

    #[test]
    fn an_unknown_catalog_id_is_created_from_the_catalog_with_scripted_answers() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.confirm_answers.borrow_mut().push_back(true);
        // The 80 GB size, second of two, and the default id.
        ui.select_answers.borrow_mut().push_back(1);

        let (id, p) =
            HardwareCommands::profile_for_fit_with(&mut ctx, "nvidia-a100", true, no_detection)
                .unwrap();

        assert_eq!(id, "nvidia-a100");
        assert_eq!(p.memory.vram_gb(), Some(80.0));
        assert_eq!(
            ctx.config().get_hardware("nvidia-a100"),
            Some(&HardwareConfig::catalog("nvidia-a100", "nvidia-a100", 80.0))
        );
        assert!(
            ui.confirm_prompts.borrow()[0]
                .0
                .contains("catalog entry 'nvidia-a100'")
        );
    }

    #[test]
    fn an_unknown_other_id_is_offered_as_a_custom_profile() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.confirm_answers.borrow_mut().push_back(true);
        ui.select_answers.borrow_mut().push_back(0); // discrete
        ui.text_answers.borrow_mut().push_back("48".to_string());

        let (_, p) =
            HardwareCommands::profile_for_fit_with(&mut ctx, "lab-box", true, no_detection)
                .unwrap();

        assert_eq!(p.memory_kind(), MemoryKind::Discrete);
        assert_eq!(p.memory.vram_gb(), Some(48.0));
    }

    #[test]
    fn declining_to_create_is_an_error() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_local(&ui, mac_36());
        ui.confirm_answers.borrow_mut().push_back(false);

        assert!(
            HardwareCommands::profile_for_fit_with(&mut ctx, "dgx-spark", true, no_detection)
                .is_err()
        );
        assert!(ctx.config().get_hardware("dgx-spark").is_none());
    }
}
