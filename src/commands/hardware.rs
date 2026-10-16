// Local
use crate::utils::hardware::detect_hardware;
use anyhow::Result;

pub struct HardwareCommands;

/*-- public --*/

impl HardwareCommands {
    /// Key-value fields for the hardware detail panel.
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

    pub fn show(ctx: &crate::AppContext) -> Result<()> {
        ctx.ui.detail("Hardware Profile", &Self::hardware_fields());
        Ok(())
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::ui::base::tests::CaptureUi;
    use std::sync::Arc;

    fn ctx() -> crate::AppContext {
        crate::AppContext::new(
            crate::config::Config::default(),
            Arc::new(CaptureUi::default()),
        )
    }

    #[test]
    fn hardware_show_renders_detail() {
        let ctx = ctx();
        HardwareCommands::show(&ctx).unwrap();
        let details = (&*ctx.ui as &dyn std::any::Any)
            .downcast_ref::<CaptureUi>()
            .unwrap()
            .details
            .borrow();
        assert_eq!(details.len(), 1);
    }

    #[test]
    fn hardware_detail_has_cpu_and_ram_fields() {
        let fields = HardwareCommands::hardware_fields();
        assert!(fields.iter().any(|(k, _)| *k == "CPU Cores"));
        assert!(fields.iter().any(|(k, _)| *k == "RAM"));
    }
}
