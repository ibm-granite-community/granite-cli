//! The `host` commands: the machines that run model servers.

use anyhow::{Result, anyhow, bail};

use crate::AppContext;
use crate::commands::shared::remediation::DependentsPolicy;
use crate::config::host::LOCAL_HOST_ID;

pub(crate) struct HostCommands;

/*-- public --*/

impl HostCommands {
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
