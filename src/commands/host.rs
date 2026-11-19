//! The `host` commands: the machines that run model servers.

use anyhow::{Result, anyhow, bail};

use crate::AppContext;
use crate::commands::shared::remediation::DependentsPolicy;
use crate::config::HostConfig;
use crate::config::hardware::LOCAL_HARDWARE_ID;
use crate::config::host::{LOCAL_HOST_ID, UNSPECIFIED_HOST};

pub(crate) struct HostCommands;

/*-- public --*/

impl HostCommands {
    /// Runs the `host setup` steps in a run that can ask: the address, the
    /// devices and the id, then saves the host. Returns its id. `provider
    /// setup` runs it for "create new host…"; the host stays saved when the
    /// rest of that setup is cancelled.
    pub(crate) fn create(ctx: &mut AppContext) -> Result<String> {
        let address = ask_address(ctx)?;
        let hardware = ask_devices(ctx, &address, false)?;
        let default = crate::config::host::free_id(
            &crate::config::host::default_id(&address, hardware.first().map(String::as_str)),
            ctx.config(),
        );
        let id = loop {
            let answer = ctx.ui.text("Host id", &default)?;
            let answer = answer.trim().to_string();
            match check_new_id(ctx, &answer) {
                Ok(()) => break answer,
                Err(e) => ctx.ui.warn(&e.to_string()),
            }
        };
        save(
            ctx,
            HostConfig {
                host_id: id.clone(),
                address,
                hardware,
            },
        )?;
        Ok(id)
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
    if address.contains(':') && !address.starts_with('[') {
        bail!("write an IPv6 address in brackets, such as [{address}]");
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
}
