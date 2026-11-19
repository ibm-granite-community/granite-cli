//! The `host` commands: the machines that run model servers.

use anyhow::{Result, anyhow, bail};

use crate::AppContext;
use crate::commands::shared::remediation::DependentsPolicy;
use crate::config::HostConfig;
use crate::config::hardware::LOCAL_HARDWARE_ID;
use crate::config::host::{LOCAL_HOST_ID, UNSPECIFIED_HOST};

pub(crate) struct HostCommands;

/*-- public --*/

/// What `host setup` was given on the command line. Anything left out is
/// asked for when the run is interactive.
#[derive(Debug, Default, Clone)]
pub(crate) struct HostSetupArgs {
    /// The host to add, or to change when it is configured.
    pub(crate) id: Option<String>,
    /// A host name or an IP address.
    pub(crate) address: Option<String>,
    /// One hardware profile id per device. Empty: asked for, or, in a run
    /// that cannot ask, unknown hardware for a new host and the devices it
    /// has for a configured one.
    pub(crate) hardware: Vec<String>,
}

impl HostCommands {
    /// `host list`: every host, `local` first, with its address, devices and
    /// the providers on it.
    pub(crate) fn list(ctx: &AppContext) -> Result<()> {
        use crate::config::validation::RefKind;
        let notes = crate::commands::shared::remediation::dangling_notes(ctx, RefKind::Host);
        let rows: Vec<Vec<String>> = host_ids(ctx)
            .into_iter()
            .map(|id| {
                vec![
                    id.clone(),
                    address(ctx, &id),
                    crate::commands::HardwareCommands::devices_summary(ctx, &id),
                    providers_on(ctx, &id).join(", "),
                    notes.get(&id).cloned().unwrap_or_default(),
                ]
            })
            .collect();
        ctx.ui.table(
            &format!(
                "Configured Hosts ({} {})",
                rows.len(),
                if rows.len() == 1 { "host" } else { "hosts" }
            ),
            &["ID", "ADDRESS", "DEVICES", "PROVIDERS", "NOTES"],
            &rows,
        );
        Ok(())
    }

    /// `host info <id>`: the host's address, devices and providers.
    pub(crate) fn info(ctx: &AppContext, id: &str) -> Result<()> {
        if !host_ids(ctx).iter().any(|h| h == id) {
            return Err(not_configured(ctx, id));
        }
        let fields: Vec<(&str, String)> = vec![
            ("Address", address(ctx, id)),
            (
                "Devices",
                crate::commands::HardwareCommands::devices_summary(ctx, id),
            ),
            ("Providers", providers_on(ctx, id).join(", ")),
        ];
        ctx.ui.detail(&format!("Host '{id}'"), &fields);
        Ok(())
    }

    /// `host setup [<id>]`: adds a host, or changes a configured one. Asks
    /// for what `args` leaves out when the run can ask. Returns the id of
    /// the host saved, or `None` when the user kept a configured host as it
    /// was.
    pub(crate) fn setup(ctx: &mut AppContext, args: HostSetupArgs) -> Result<Option<String>> {
        let interactive = crate::commands::HardwareCommands::can_prompt(ctx);
        setup_with(ctx, args, interactive)
    }

    /// Runs the `host setup` steps in a run that can ask: the address, the
    /// devices and the id, then saves the host. Returns its id. `provider
    /// setup` runs it for "create new host…"; the host stays saved when the
    /// rest of that setup is cancelled.
    pub(crate) fn create(ctx: &mut AppContext) -> Result<String> {
        setup_with(ctx, HostSetupArgs::default(), true)?
            .ok_or_else(|| anyhow!("no host was configured"))
    }

    /// Asks again for the devices of host `id` and saves them, keeping its
    /// id and address. The repair the remediation prompt offers for a host
    /// that lists a hardware profile that is missing or cannot be used.
    pub(crate) fn reconfigure_devices(ctx: &mut AppContext, id: &str) -> Result<()> {
        let Some(mut host) = ctx.config().get_host(id).cloned() else {
            return Err(not_configured(ctx, id));
        };
        let local = host.host_id == LOCAL_HOST_ID;
        host.hardware = ask_devices(ctx, &host.address, local)?;
        save(ctx, host)
    }

    /// `host remove <id>`. `local` cannot be removed. When providers are
    /// linked to the host, they are listed and the user chooses to remove
    /// them too, keep the host, or remove it alone.
    pub(crate) fn remove(
        ctx: &mut AppContext,
        id: &str,
        policy: Option<DependentsPolicy>,
    ) -> Result<()> {
        use crate::commands::shared::remediation::{Removal, confirm_removal, remove_all};
        use crate::config::validation::RefKind;

        if id == LOCAL_HOST_ID {
            bail!("host '{id}' is this machine and cannot be removed");
        }
        if ctx.config().get_host(id).is_none() {
            return Err(not_configured(ctx, id));
        }
        match confirm_removal(ctx, RefKind::Host, id, policy)? {
            Removal::Cancel => {
                ctx.ui.info(&format!("Keeping host '{id}'."));
                return Ok(());
            }
            Removal::Proceed { with } => remove_all(ctx, &with, policy)?,
        }
        ctx.config_mut().remove_host(id)?;
        ctx.ui.info(&format!("Host '{id}' removed."));
        Ok(())
    }
}

/*-- private --*/

fn setup_with(
    ctx: &mut AppContext,
    args: HostSetupArgs,
    interactive: bool,
) -> Result<Option<String>> {
    if args.id.as_deref() == Some(LOCAL_HOST_ID) {
        bail!(
            "host '{LOCAL_HOST_ID}' is this machine: its address is {} and its devices follow \
             detection. Change a device with `granite-cli hardware setup local`.",
            crate::config::host::LOCAL_ADDRESS
        );
    }
    let existing = args
        .id
        .as_deref()
        .and_then(|id| ctx.config().get_host(id).cloned());
    if let Some(id) = &args.id
        && existing.is_none()
    {
        check_new_id(ctx, id)?;
    }
    for device in &args.hardware {
        check_device(ctx, device)?;
    }
    if let Some(host) = &existing
        && interactive
        && !ctx.ui.confirm(
            &format!(
                "Host '{}' is already configured ({}, {}). Change it?",
                host.host_id,
                host.address,
                crate::commands::HardwareCommands::devices_summary(ctx, &host.host_id)
            ),
            true,
        )?
    {
        ctx.ui.info("Host setup skipped.");
        return Ok(None);
    }

    let own_id = existing.as_ref().map(|h| h.host_id.as_str());
    let address = match (&args.address, &existing) {
        (Some(address), _) => check_address(ctx, address.trim(), own_id)?,
        (None, Some(host)) if interactive => ask_address_from(ctx, &host.address, own_id)?,
        (None, Some(host)) => host.address.clone(),
        (None, None) if interactive => ask_address(ctx)?,
        (None, None) => bail!("pass --address with the host name or IP address"),
    };
    let hardware = match &existing {
        _ if !args.hardware.is_empty() => args.hardware.clone(),
        Some(host) if interactive => ask_devices_of(ctx, host, &address)?,
        Some(host) => host.hardware.clone(),
        None if interactive => ask_devices(ctx, &address, false)?,
        None => Vec::new(),
    };
    check_memory_kinds(ctx, &hardware)?;
    let id = match (&args.id, &existing) {
        (Some(id), _) => id.clone(),
        (None, _) => {
            let default =
                crate::config::host::default_id(&address, hardware.first().map(String::as_str));
            if interactive {
                let default = crate::config::host::free_id(&default, ctx.config());
                loop {
                    let answer = ctx.ui.text("Host id", &default)?;
                    let answer = answer.trim().to_string();
                    match check_new_id(ctx, &answer) {
                        Ok(()) => break answer,
                        Err(e) => ctx.ui.warn(&e.to_string()),
                    }
                }
            } else {
                check_new_id(ctx, &default).map_err(|e| {
                    anyhow!("{e}; name the host to add with `granite-cli host setup <id> ...`")
                })?;
                default
            }
        }
    };
    let old = existing.as_ref().map(|h| h.address.clone());
    save(
        ctx,
        HostConfig {
            host_id: id.clone(),
            address: address.clone(),
            hardware,
        },
    )?;
    // After the host, so a provider never points at an address its host
    // does not have yet.
    if let Some(old) = old
        && old != address
    {
        rewrite_base_urls(ctx, &id, &old, &address, interactive)?;
    }
    Ok(Some(id))
}

/// Every host id, `local` first, then the others sorted. `local` is listed
/// before it is saved, since every command that uses it writes it.
fn host_ids(ctx: &AppContext) -> Vec<String> {
    let mut ids: Vec<String> = ctx
        .config()
        .hosts
        .keys()
        .filter(|id| id.as_str() != LOCAL_HOST_ID)
        .cloned()
        .collect();
    ids.sort();
    ids.insert(0, LOCAL_HOST_ID.to_string());
    ids
}

/// The address of host `id`: `localhost` for `local` before it is saved.
fn address(ctx: &AppContext, id: &str) -> String {
    ctx.config().get_host(id).map_or_else(
        || crate::config::host::LOCAL_ADDRESS.to_string(),
        |h| h.address.clone(),
    )
}

/// The providers linked to host `id`, sorted. A provider with no saved
/// link runs on `local`.
fn providers_on(ctx: &AppContext, id: &str) -> Vec<String> {
    let mut ids: Vec<String> = ctx
        .config()
        .providers
        .values()
        .filter(|p| p.host_link().host_id() == Some(id))
        .map(|p| p.provider_id.clone())
        .collect();
    ids.sort();
    ids
}

/// Asks for the new address of a configured host, starting on its current
/// one.
fn ask_address_from(ctx: &AppContext, current: &str, own_id: Option<&str>) -> Result<String> {
    loop {
        let answer = ctx.ui.text("Host name or IP address", current)?;
        match check_address(ctx, answer.trim(), own_id) {
            Ok(address) => return Ok(address),
            Err(e) => ctx.ui.warn(&e.to_string()),
        }
    }
}

/// Asks which devices configured host `host` has, starting on keeping the
/// ones it lists.
fn ask_devices_of(ctx: &mut AppContext, host: &HostConfig, address: &str) -> Result<Vec<String>> {
    let keep = format!(
        "keep {}",
        crate::commands::HardwareCommands::devices_summary(ctx, &host.host_id)
    );
    let mut distinct: Vec<String> = Vec::new();
    for device in &host.hardware {
        if !distinct.contains(device) {
            distinct.push(device.clone());
        }
    }
    let mut items = vec![keep];
    items.extend(distinct.iter().map(|d| format!("edit {d}…")));
    items.push("choose other devices…".to_string());
    let picked = ctx
        .ui
        .select(&format!("Hardware on {address}"), &items, 0)?;
    if picked == 0 {
        return Ok(host.hardware.clone());
    }
    if let Some(profile) = distinct.get(picked - 1) {
        return crate::commands::HardwareCommands::edit_for_host(ctx, profile, &host.host_id);
    }
    ask_devices(ctx, address, false)
}

/// Refuses a device that is not a configured hardware profile, or is one of
/// this machine's, which only `local` lists.
fn check_device(ctx: &AppContext, id: &str) -> Result<()> {
    match ctx.config().get_hardware(id) {
        None => {
            let mut ids: Vec<&str> = ctx
                .config()
                .hardware
                .iter()
                .filter(|(_, hw)| hw.hardware_type != LOCAL_HARDWARE_ID)
                .map(|(id, _)| id.as_str())
                .collect();
            ids.sort();
            bail!(
                "hardware profile '{id}' is not configured; configured: {}. Create it with \
                 `granite-cli hardware setup`",
                if ids.is_empty() {
                    "none".to_string()
                } else {
                    ids.join(", ")
                }
            )
        }
        Some(hw) if hw.hardware_type == LOCAL_HARDWARE_ID => bail!(
            "hardware profile '{id}' describes this machine; only host '{LOCAL_HOST_ID}' lists it"
        ),
        Some(_) => Ok(()),
    }
}

/// Refuses devices with different memory kinds, which a host cannot list.
fn check_memory_kinds(ctx: &AppContext, hardware: &[String]) -> Result<()> {
    let mut kinds: Vec<crate::utils::hardware::MemoryKind> = hardware
        .iter()
        .filter_map(|id| ctx.hardware_profile(id).ok())
        .map(|p| p.memory_kind())
        .collect();
    kinds.sort();
    kinds.dedup();
    if kinds.len() > 1 {
        let names: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
        bail!(
            "a host lists devices of one memory kind; these are {}",
            names.join(" and ")
        );
    }
    Ok(())
}

/// Offers to move the base URLs of the providers on host `id` that use
/// `old` to `new`. A run that cannot ask leaves them, and `provider list`
/// notes each one.
fn rewrite_base_urls(
    ctx: &mut AppContext,
    id: &str,
    old: &str,
    new: &str,
    interactive: bool,
) -> Result<()> {
    use crate::config::host::{same_address, url_at, url_host};
    let mut moves: Vec<(String, String, String)> = ctx
        .config()
        .providers
        .values()
        .filter(|p| p.host_link().host_id() == Some(id))
        .filter_map(|p| {
            let url = p.config.get("base_url")?.as_str()?;
            if !same_address(&url_host(url)?, old) {
                return None;
            }
            Some((p.provider_id.clone(), url.to_string(), url_at(url, new)?))
        })
        .collect();
    if moves.is_empty() {
        return Ok(());
    }
    moves.sort();
    if !interactive {
        ctx.ui.warn(&format!(
            "Kept the base URLs of {} on '{id}' at {old}; `granite-cli provider list` notes them.",
            moves
                .iter()
                .map(|(p, _, _)| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return Ok(());
    }
    for (provider, from, to) in &moves {
        ctx.ui.info(&format!("    {provider}  {from}  →  {to}"));
    }
    let question = format!(
        "Update the address in {} {} on '{id}'?",
        moves.len(),
        if moves.len() == 1 {
            "provider"
        } else {
            "providers"
        }
    );
    if !ctx.ui.confirm(&question, true)? {
        return Ok(());
    }
    for (provider, _, to) in moves {
        ctx.config_mut().update_provider(&provider, |p| {
            if let Some(fields) = p.config.as_object_mut() {
                fields.insert("base_url".to_string(), to.into());
            }
        })?;
    }
    Ok(())
}

/// Asks for the address of a new host until the answer is a host name or
/// an IP address that no configured host has. Returns it as URLs write it.
fn ask_address(ctx: &AppContext) -> Result<String> {
    loop {
        let answer = ctx.ui.text("Host name or IP address", "")?;
        match check_address(ctx, answer.trim(), None) {
            Ok(address) => return Ok(address),
            Err(e) => ctx.ui.warn(&e.to_string()),
        }
    }
}

/// The address as URLs write it (a lowercase name, a bracketed IPv6
/// address), or an error for one that is not a host name or an IP address,
/// or that a configured host other than `except` has, naming that host.
/// `localhost` belongs to `local`.
fn check_address(ctx: &AppContext, address: &str, except: Option<&str>) -> Result<String> {
    if address.parse::<std::net::Ipv6Addr>().is_ok() {
        bail!("write an IPv6 address in brackets, such as [{address}]");
    }
    if address.contains(':') && !address.starts_with('[') {
        bail!("'{address}' is not a host name or IP address; an address has no port");
    }
    let address = match url::Host::parse(address) {
        Ok(host) if !address.is_empty() => host.to_string(),
        _ => bail!("'{address}' is not a host name or IP address"),
    };
    let address = address.as_str();
    let mut owner = ctx
        .config()
        .hosts
        .values()
        .find(|h| h.address.eq_ignore_ascii_case(address))
        .map(|h| h.host_id.clone());
    if owner.is_none() && address.eq_ignore_ascii_case(crate::config::host::LOCAL_ADDRESS) {
        owner = Some(LOCAL_HOST_ID.to_string());
    }
    match owner {
        Some(owner) if Some(owner.as_str()) != except => {
            bail!("{address} is the address of host '{owner}'")
        }
        _ => Ok(address.to_string()),
    }
}

/// Asks which devices the host at `address` has: a configured hardware
/// profile, a new profile made with the `hardware setup` steps, or
/// "unknown" for none. After a profile, asks how many of that device the
/// host has. The `local` profiles describe this machine, so only the `local`
/// host, `local`, is offered them.
fn ask_devices(ctx: &mut AppContext, address: &str, local: bool) -> Result<Vec<String>> {
    let mut ids: Vec<String> = ctx
        .config()
        .hardware
        .iter()
        .filter(|(_, hw)| local || hw.hardware_type != LOCAL_HARDWARE_ID)
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    let mut items: Vec<String> = ids
        .iter()
        .map(|id| {
            let memory = ctx
                .hardware_profile(id)
                .map(|p| p.memory_summary())
                .unwrap_or_else(|e| e);
            format!("{id} ({memory})")
        })
        .collect();
    items.push("create new profile…".to_string());
    items.push("unknown (fit not available)".to_string());
    let picked = ctx
        .ui
        .select(&format!("Hardware on {address}"), &items, 0)?;
    let profile = if picked < ids.len() {
        ids[picked].clone()
    } else if picked == ids.len() {
        crate::commands::HardwareCommands::setup(
            ctx,
            crate::commands::hardware::HardwareSetupArgs::default(),
        )?
        .ok_or_else(|| anyhow!("no hardware profile to use; hardware setup was skipped"))?
    } else {
        return Ok(Vec::new());
    };
    let count = loop {
        let answer = ctx
            .ui
            .text(&format!("How many of these on {address}?"), "1")?;
        match answer.trim().parse::<usize>() {
            Ok(n) if n >= 1 => break n,
            _ => ctx
                .ui
                .warn(&format!("'{}' is not a number of devices", answer.trim())),
        }
    };
    Ok(vec![profile; count])
}

/// Refuses an id a new host cannot be saved under: one taken by a
/// configured host, `local`, `unspecified`, or one that is not a plain name
/// (a letter or digit first, then letters, digits, `-`, `_` and `.`).
fn check_new_id(ctx: &AppContext, id: &str) -> Result<()> {
    if id == LOCAL_HOST_ID || id == UNSPECIFIED_HOST {
        bail!("'{id}' is reserved; choose another id");
    }
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
    if !id.starts_with(|c: char| c.is_ascii_alphanumeric()) || !id.chars().all(allowed) {
        bail!(
            "'{id}' cannot be a host id; use letters, digits, '-', '_' and '.', starting with \
             a letter or digit"
        );
    }
    if ctx.config().get_host(id).is_some() {
        bail!("host '{id}' is already configured");
    }
    Ok(())
}

/// Saves `host` and says so in one line.
fn save(ctx: &mut AppContext, host: HostConfig) -> Result<()> {
    let id = host.host_id.clone();
    let address = host.address.clone();
    ctx.config_mut().insert_host(&id, host)?;
    let devices = crate::commands::HardwareCommands::devices_summary(ctx, &id);
    ctx.ui.info(
        &ctx.ui
            .ok(&format!("✓ Host '{id}' configured ({address}, {devices})")),
    );
    Ok(())
}

fn not_configured(ctx: &AppContext, id: &str) -> anyhow::Error {
    let mut ids: Vec<&str> = ctx.config().hosts.keys().map(String::as_str).collect();
    ids.sort();
    anyhow!(
        "host '{id}' is not configured; configured: {}",
        if ids.is_empty() {
            "none".to_string()
        } else {
            ids.join(", ")
        }
    )
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::host::HostLink;
    use crate::config::{Config, HostConfig, ProviderConfig, TestConfigHome};
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn ctx_with_box(ui: &Arc<CaptureUi>) -> AppContext {
        let mut ctx = AppContext::new(Config::new().unwrap(), ui.clone());
        ctx.config_mut()
            .insert_host(
                "box",
                HostConfig {
                    host_id: "box".to_string(),
                    address: "box.lan".to_string(),
                    hardware: Vec::new(),
                },
            )
            .unwrap();
        ctx.config_mut()
            .insert_provider(
                "llama-cpp-box",
                ProviderConfig {
                    provider_id: "llama-cpp-box".to_string(),
                    provider_type: "llama-cpp".to_string(),
                    config: serde_json::json!({ "base_url": "http://box.lan:8080" }),
                    host: Some(HostLink::Host("box".to_string())),
                },
            )
            .unwrap();
        ctx
    }

    #[test]
    fn removing_a_host_lists_its_providers_and_can_remove_them_too() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_box(&ui);
        ui.select_answers.borrow_mut().push_back(0); // Remove together

        HostCommands::remove(&mut ctx, "box", None).unwrap();

        assert_eq!(ui.warns.borrow()[0], "Removing host 'box' will break:");
        assert_eq!(
            ui.infos.borrow()[0],
            "  - provider 'llama-cpp-box' (llama-cpp)"
        );
        assert!(ctx.config().get_host("box").is_none());
        assert!(ctx.config().get_provider("llama-cpp-box").is_none());
    }

    #[test]
    fn local_and_unknown_hosts_cannot_be_removed() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_box(&ui);

        let local = HostCommands::remove(&mut ctx, LOCAL_HOST_ID, None).unwrap_err();
        let gone = HostCommands::remove(&mut ctx, "gone", None).unwrap_err();

        assert_eq!(
            local.to_string(),
            "host 'local' is this machine and cannot be removed"
        );
        assert_eq!(
            gone.to_string(),
            "host 'gone' is not configured; configured: box"
        );
    }

    // -- list, info and setup ----------------------------------------------------

    fn card(id: &str, gb: f64) -> crate::config::HardwareConfig {
        crate::config::HardwareConfig::custom(
            id,
            crate::utils::hardware::HardwareProfile::with_memory(
                crate::utils::hardware::Memory::Discrete {
                    vram_gb: gb,
                    ram_gb: None,
                },
            ),
        )
    }

    /// A context with `local` saved, the `rtx` and `spark` profiles, hosts
    /// `pgx` and `pgx2` sharing `spark`, `box` listing `rtx` twice, and a
    /// provider on `pgx` at its address.
    fn ctx_with_hosts(ui: &Arc<CaptureUi>) -> AppContext {
        let mut ctx = AppContext::new(Config::new().unwrap(), ui.clone());
        ctx.ensure_local_host(|| {
            crate::utils::hardware::HardwareProfile::with_memory(
                crate::utils::hardware::Memory::System { ram_gb: 32.0 },
            )
        })
        .unwrap();
        ctx.config_mut()
            .insert_hardware("rtx", card("rtx", 24.0))
            .unwrap();
        ctx.config_mut()
            .insert_hardware("spark", card("spark", 128.0))
            .unwrap();
        for (id, address, hardware) in [
            ("pgx", "pgx.lan", vec!["spark"]),
            ("pgx2", "pgx2.lan", vec!["spark"]),
            ("box", "box.lan", vec!["rtx", "rtx"]),
        ] {
            ctx.config_mut()
                .insert_host(
                    id,
                    HostConfig {
                        host_id: id.to_string(),
                        address: address.to_string(),
                        hardware: hardware.iter().map(|h| h.to_string()).collect(),
                    },
                )
                .unwrap();
        }
        for (id, url) in [
            ("ollama-pgx", "http://pgx.lan:11434"),
            ("llama-cpp-pgx", "http://pgx.lan:9797"),
            ("elsewhere", "http://10.9.9.9:8080"),
        ] {
            ctx.config_mut()
                .insert_provider(
                    id,
                    ProviderConfig {
                        provider_id: id.to_string(),
                        provider_type: "llama-cpp".to_string(),
                        config: serde_json::json!({ "base_url": url }),
                        host: Some(HostLink::Host("pgx".to_string())),
                    },
                )
                .unwrap();
        }
        ui.infos.borrow_mut().clear();
        ui.stderr_infos.borrow_mut().clear();
        ctx
    }

    fn args(id: Option<&str>, address: Option<&str>, hardware: &[&str]) -> HostSetupArgs {
        HostSetupArgs {
            id: id.map(str::to_string),
            address: address.map(str::to_string),
            hardware: hardware.iter().map(|h| h.to_string()).collect(),
        }
    }

    #[test]
    fn list_and_info_show_address_devices_and_providers() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let ctx = ctx_with_hosts(&ui);

        HostCommands::list(&ctx).unwrap();
        HostCommands::info(&ctx, "box").unwrap();

        let tables = ui.tables.borrow();
        let (title, headers, rows) = &tables[0];
        assert_eq!(title, "Configured Hosts (4 hosts)");
        assert_eq!(
            *headers,
            vec!["ID", "ADDRESS", "DEVICES", "PROVIDERS", "NOTES"]
        );
        let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(ids, vec!["local", "box", "pgx", "pgx2"]);
        assert_eq!(rows[1][2], "2 × rtx (discrete 24.0 GB)");
        assert_eq!(rows[2][3], "elsewhere, llama-cpp-pgx, ollama-pgx");
        let details = ui.details.borrow();
        assert_eq!(details[0].0, "Host 'box'");
        assert_eq!(
            details[0].1[0],
            ("Address".to_string(), "box.lan".to_string())
        );
    }

    #[test]
    fn setup_from_flags_adds_a_host_with_two_devices_or_unknown_hardware() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);

        let two = setup_with(
            &mut ctx,
            args(None, Some("gpu.lan"), &["rtx", "rtx"]),
            false,
        );
        let ip = setup_with(&mut ctx, args(None, Some("10.0.0.30"), &["spark"]), false);
        let unknown = setup_with(&mut ctx, args(None, Some("10.0.0.31"), &[]), false);

        assert_eq!(two.unwrap().as_deref(), Some("gpu"));
        assert_eq!(ip.unwrap().as_deref(), Some("spark"));
        assert_eq!(unknown.unwrap().as_deref(), Some("host"));
        assert_eq!(ctx.config().host_hardware("gpu"), vec!["rtx", "rtx"]);
        assert!(ctx.config().host_hardware("host").is_empty());
    }

    #[test]
    fn setup_asks_for_the_address_devices_how_many_and_the_id() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // rtx is first of: rtx, spark, create, unknown.
        ui.select_answers.borrow_mut().push_back(0);
        ui.text_answers
            .borrow_mut()
            .extend(["Box2.LAN".to_string(), "2".to_string()]);

        let id = setup_with(&mut ctx, HostSetupArgs::default(), true).unwrap();

        assert_eq!(id.as_deref(), Some("box2"));
        let host = ctx.config().get_host("box2").unwrap();
        assert_eq!(host.address, "box2.lan", "saved as URLs write it");
        assert_eq!(host.hardware, vec!["rtx", "rtx"]);
        let texts = ui.text_prompts.borrow();
        assert_eq!(texts[1].0, "How many of these on box2.lan?");
        assert_eq!(texts[2], ("Host id".to_string(), "box2".to_string()));
    }

    #[test]
    fn setup_can_create_a_profile_for_the_host() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        let families = {
            let mut f: Vec<&str> = Vec::new();
            for e in crate::utils::hardware_catalog::HARDWARE_CATALOG.iter() {
                if !f.contains(&e.family) {
                    f.push(e.family);
                }
            }
            f.len()
        };
        // create new profile…; the family list's last item, custom; discrete.
        ui.select_answers.borrow_mut().extend([2, families, 0]);
        // address, profile id, memory, headroom (default), how many, host id
        // (default).
        ui.text_answers.borrow_mut().extend([
            "lab.lan".to_string(),
            "a100".to_string(),
            "80".to_string(),
        ]);

        let id = setup_with(&mut ctx, HostSetupArgs::default(), true).unwrap();

        assert_eq!(id.as_deref(), Some("lab"));
        assert_eq!(ctx.config().host_hardware("lab"), vec!["a100"]);
        assert_eq!(
            ctx.hardware_profile("a100").unwrap().memory.vram_gb(),
            Some(80.0)
        );
    }

    #[test]
    fn setup_refuses_what_it_cannot_save() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        let refused = |ctx: &mut AppContext, a: HostSetupArgs| {
            setup_with(ctx, a, false).unwrap_err().to_string()
        };

        let local = refused(&mut ctx, args(Some("local"), Some("x.lan"), &[]));
        let bad_id = refused(&mut ctx, args(Some("bad/id"), Some("x.lan"), &[]));
        let local_profile = refused(&mut ctx, args(None, Some("x.lan"), &["local"]));
        let missing = refused(&mut ctx, args(None, Some("x.lan"), &["nope"]));
        let taken = refused(&mut ctx, args(Some("other"), Some("BOX.lan"), &[]));
        let localhost = refused(&mut ctx, args(Some("other"), Some("localhost"), &[]));
        let default_taken = refused(&mut ctx, args(None, Some("box.example"), &["rtx"]));
        let ipv6 = refused(&mut ctx, args(Some("v6"), Some("fe80::1"), &[]));
        let mixed = {
            ctx.config_mut()
                .insert_hardware(
                    "mac",
                    crate::config::HardwareConfig::catalog("mac", "apple-m4-max", 64.0),
                )
                .unwrap();
            refused(&mut ctx, args(None, Some("x.lan"), &["rtx", "mac"]))
        };
        let no_address = refused(&mut ctx, args(Some("new"), None, &[]));

        assert!(local.starts_with("host 'local' is this machine"), "{local}");
        assert!(bad_id.contains("cannot be a host id"), "{bad_id}");
        assert!(
            local_profile.contains("describes this machine"),
            "{local_profile}"
        );
        assert!(missing.contains("'nope' is not configured"), "{missing}");
        assert_eq!(taken, "box.lan is the address of host 'box'");
        assert_eq!(localhost, "localhost is the address of host 'local'");
        assert!(
            default_taken.starts_with("host 'box' is already configured"),
            "{default_taken}"
        );
        assert!(ipv6.contains("in brackets"), "{ipv6}");
        assert!(mixed.contains("one memory kind"), "{mixed}");
        assert_eq!(
            no_address,
            "pass --address with the host name or IP address"
        );
        assert!(ctx.config().get_host("other").is_none());
        assert!(ctx.config().get_host("x").is_none());
    }

    #[test]
    fn host_setup_address_change_rewrites_when_accepted() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // Change it? yes; keep the devices; update the providers? yes.
        ui.confirm_answers.borrow_mut().extend([true, true]);
        ui.select_answers.borrow_mut().push_back(0);

        setup_with(&mut ctx, args(Some("pgx"), Some("10.0.0.12"), &[]), true).unwrap();

        let url = |id: &str| ctx.config().get_provider(id).unwrap().config["base_url"].clone();
        assert_eq!(url("ollama-pgx"), "http://10.0.0.12:11434");
        assert_eq!(url("llama-cpp-pgx"), "http://10.0.0.12:9797");
        assert_eq!(
            url("elsewhere"),
            "http://10.9.9.9:8080",
            "not at the old address"
        );
        assert_eq!(
            ui.confirm_prompts.borrow()[1].0,
            "Update the address in 2 providers on 'pgx'?"
        );
        assert_eq!(ctx.config().get_host("pgx").unwrap().address, "10.0.0.12");
    }

    #[test]
    fn an_address_change_that_is_declined_or_cannot_ask_keeps_the_base_urls() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        ui.confirm_answers.borrow_mut().extend([true, false]);
        ui.select_answers.borrow_mut().push_back(0);

        setup_with(&mut ctx, args(Some("pgx"), Some("10.0.0.12"), &[]), true).unwrap();
        setup_with(&mut ctx, args(Some("pgx"), Some("10.0.0.13"), &[]), false).unwrap();

        assert_eq!(
            ctx.config().get_provider("ollama-pgx").unwrap().config["base_url"],
            "http://pgx.lan:11434"
        );
        assert_eq!(ctx.config().get_host("pgx").unwrap().address, "10.0.0.13");
        assert_eq!(ctx.config().host_hardware("pgx"), vec!["spark"]);
    }

    #[test]
    fn edit_shared_profile_for_every_use() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // Change pgx? yes; "edit spark…"; edit the shared profile; kind
        // discrete.
        ui.confirm_answers.borrow_mut().push_back(true);
        ui.select_answers.borrow_mut().extend([1, 0, 0]);
        ui.text_answers
            .borrow_mut()
            .extend(["pgx.lan".to_string(), "96".to_string()]);

        setup_with(&mut ctx, args(Some("pgx"), None, &[]), true).unwrap();

        assert_eq!(ui.warns.borrow()[0], "'spark' is used by pgx and pgx2.");
        assert_eq!(ctx.config().host_hardware("pgx"), vec!["spark"]);
        assert_eq!(ctx.config().host_hardware("pgx2"), vec!["spark"]);
        assert_eq!(
            ctx.hardware_profile("spark").unwrap().memory.vram_gb(),
            Some(96.0)
        );
    }

    #[test]
    fn edit_shared_profile_new_profile_for_one_host() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // Change box? yes; "edit rtx…"; create a new profile for box; kind
        // discrete.
        ui.confirm_answers.borrow_mut().push_back(true);
        ui.select_answers.borrow_mut().extend([1, 1, 0]);
        // Device 1 only.
        ui.multi_select_answers.borrow_mut().push_back(vec![0]);
        // address (kept), profile id (default rtx-box), memory.
        ui.text_answers.borrow_mut().extend([
            "box.lan".to_string(),
            "rtx-box".to_string(),
            "48".to_string(),
        ]);

        setup_with(&mut ctx, args(Some("box"), None, &[]), true).unwrap();

        assert_eq!(
            ui.warns.borrow()[0],
            "'rtx' is used by box (device 1) and box (device 2)."
        );
        let multi = ui.multi_select_prompts.borrow();
        assert_eq!(multi[0].0, "Which devices on box use the new profile?");
        assert_eq!(multi[0].1, vec!["device 1", "device 2"]);
        assert_eq!(ctx.config().host_hardware("box"), vec!["rtx-box", "rtx"]);
        assert_eq!(
            ctx.hardware_profile("rtx").unwrap().memory.vram_gb(),
            Some(24.0),
            "the shared profile is unchanged"
        );
        assert_eq!(
            ctx.hardware_profile("rtx-box").unwrap().memory.vram_gb(),
            Some(48.0)
        );
    }

    #[test]
    fn edit_shared_profile_pick_uses() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // `hardware setup spark`: create a new profile; kind discrete.
        ui.select_answers.borrow_mut().extend([1, 0]);
        // pgx2 only.
        ui.multi_select_answers.borrow_mut().push_back(vec![1]);
        ui.text_answers
            .borrow_mut()
            .extend(["spark-pgx2".to_string(), "64".to_string()]);

        crate::commands::HardwareCommands::setup(
            &mut ctx,
            crate::commands::hardware::HardwareSetupArgs {
                target: Some("spark".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        let multi = ui.multi_select_prompts.borrow();
        assert_eq!(multi[0].0, "Which uses move to the new profile?");
        assert_eq!(multi[0].1, vec!["pgx", "pgx2"]);
        let texts = ui.text_prompts.borrow();
        assert!(
            texts
                .iter()
                .any(|(p, d)| p == "Profile id" && d == "spark-pgx2"),
            "{texts:?}"
        );
        assert_eq!(ctx.config().host_hardware("pgx"), vec!["spark"]);
        assert_eq!(ctx.config().host_hardware("pgx2"), vec!["spark-pgx2"]);
    }

    #[test]
    fn a_new_profile_for_a_host_that_lists_it_once_takes_its_one_device() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // Change pgx? yes; "edit spark…"; create a new profile for pgx; kind
        // discrete.
        ui.confirm_answers.borrow_mut().push_back(true);
        ui.select_answers.borrow_mut().extend([1, 1, 0]);
        ui.text_answers.borrow_mut().extend([
            "pgx.lan".to_string(),
            "spark-pgx".to_string(),
            "120".to_string(),
        ]);

        setup_with(&mut ctx, args(Some("pgx"), None, &[]), true).unwrap();

        assert!(
            ui.multi_select_prompts.borrow().is_empty(),
            "no device to pick"
        );
        assert_eq!(ctx.config().host_hardware("pgx"), vec!["spark-pgx"]);
        assert_eq!(ctx.config().host_hardware("pgx2"), vec!["spark"]);
    }

    #[test]
    fn choosing_no_use_for_the_new_profile_changes_nothing() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        ui.select_answers.borrow_mut().push_back(1);
        ui.multi_select_answers.borrow_mut().push_back(vec![]);

        let saved = crate::commands::HardwareCommands::setup(
            &mut ctx,
            crate::commands::hardware::HardwareSetupArgs {
                target: Some("spark".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(saved, None);
        assert_eq!(
            ctx.hardware_profile("spark").unwrap().memory.vram_gb(),
            Some(128.0)
        );
    }

    #[test]
    fn a_run_that_cannot_ask_refuses_to_replace_a_shared_profile() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        *ui.interactive.borrow_mut() = Some(false);

        let err = crate::commands::HardwareCommands::setup(
            &mut ctx,
            crate::commands::hardware::HardwareSetupArgs {
                target: Some("custom".to_string()),
                id: Some("spark".to_string()),
                kind: Some(crate::utils::hardware::MemoryKind::Discrete),
                memory: Some(8.0),
                ..Default::default()
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("already configured"), "{err}");
        assert_eq!(
            ctx.hardware_profile("spark").unwrap().memory.vram_gb(),
            Some(128.0)
        );
    }

    #[test]
    fn unknown_hardware_can_be_chosen_and_a_port_is_refused() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx_with_hosts(&ui);
        // rtx, spark, create, unknown.
        ui.select_answers.borrow_mut().push_back(3);
        ui.text_answers
            .borrow_mut()
            .extend(["lab.lan:8080".to_string(), "lab.lan".to_string()]);

        let id = setup_with(&mut ctx, HostSetupArgs::default(), true).unwrap();

        assert_eq!(id.as_deref(), Some("lab"));
        assert!(ctx.config().host_hardware("lab").is_empty());
        assert_eq!(
            ui.warns.borrow()[0],
            "'lab.lan:8080' is not a host name or IP address; an address has no port"
        );
    }
}
