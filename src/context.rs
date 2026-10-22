//! The application context every command runs against: the configuration,
//! the `Ui`, and the sources built from that configuration.
//!
//! It lives in a module of its own so that `config` is private to this file.
//! Every read goes through [`AppContext::config`] and every write through
//! [`AppContext::config_mut`] or [`AppContext::set_config`], which discard the
//! sources, so nothing built from an earlier snapshot is handed out after a
//! write.

use std::collections::HashMap;

use crate::config::HardwareConfig;
use crate::config::hardware::LOCAL_HARDWARE_ID;
use crate::utils::hardware::{HardwareProfile, MemoryKind};

/*-- public --*/

pub struct AppContext {
    config: crate::config::Config,
    pub ui: std::sync::Arc<dyn crate::utils::ui::Ui>,
    /// The sources built from `config`, built on the first ask and dropped
    /// whenever configuration is written. Behind a `Mutex` because commands
    /// ask for them through `&self` and hold what they get across await
    /// points.
    sources: std::sync::Mutex<Option<crate::sources::Sources>>,
    /// The figures of each hardware profile looked up so far, or why they
    /// could not be. Dropped with the sources whenever configuration is
    /// written, so a catalog figure is read again after the profile changes.
    hardware: std::sync::Mutex<HashMap<String, Result<HardwareProfile, String>>>,
    /// The session proxy for this run, when a launch started one. Every
    /// provider the sources hand out points at it, which is why setting it
    /// discards them.
    model_proxy: Option<crate::proxy::ProxyHandle>,
}

impl AppContext {
    pub(crate) fn new(
        config: crate::config::Config,
        ui: std::sync::Arc<dyn crate::utils::ui::Ui>,
    ) -> Self {
        Self {
            config,
            ui,
            sources: std::sync::Mutex::new(None),
            hardware: std::sync::Mutex::new(HashMap::new()),
            model_proxy: None,
        }
    }

    /// The configuration as it now stands.
    pub(crate) fn config(&self) -> &crate::config::Config {
        &self.config
    }

    /// The configuration, to change. Taking it discards the sources, so
    /// nothing built from the old snapshot is handed out after a write.
    pub(crate) fn config_mut(&mut self) -> &mut crate::config::Config {
        self.invalidate();
        &mut self.config
    }

    /// Replace the configuration wholesale, which a launch does to pick up
    /// whatever was saved before it ran.
    pub(crate) fn set_config(&mut self, config: crate::config::Config) {
        self.invalidate();
        self.config = config;
    }

    /// Point every provider the sources hand out at the session proxy a
    /// launch just started.
    pub(crate) fn set_model_proxy(&mut self, model_proxy: crate::proxy::ProxyHandle) {
        self.invalidate();
        self.model_proxy = Some(model_proxy);
    }

    /// The sources for the configuration as it now stands, built on the
    /// first ask. The handles outlive the snapshot they came from, so a
    /// launch keeps the set it resolved against for as long as it runs.
    pub(crate) fn sources(&self) -> crate::sources::Sources {
        let mut held = self.sources.lock().unwrap();
        held.get_or_insert_with(|| {
            crate::sources::Sources::build(&self.config, self.model_proxy.clone())
        })
        .clone()
    }

    /// The figures of hardware profile `id`, looked up on the first ask and
    /// kept until configuration is written. Fails with the reason when `id`
    /// is not configured or its figures cannot be found.
    // Read by the commands that estimate fit, which later sub-tasks of spec
    // 0027 point at configured hardware profiles.
    #[allow(dead_code)]
    pub(crate) fn hardware_profile(&self, id: &str) -> Result<HardwareProfile, String> {
        let mut held = self.hardware.lock().unwrap();
        held.entry(id.to_string())
            .or_insert_with(|| match self.config.get_hardware(id) {
                Some(hardware) => hardware.profile(),
                None => Err(format!("hardware profile '{id}' is not configured")),
            })
            .clone()
    }

    /// Writes the `local` hardware profile from `detect` when there is none,
    /// and says so in one line. Does nothing when `local` exists. `detect` is
    /// a parameter so tests never read the machine they run on.
    // Called by the commands that estimate fit, which a later sub-task of
    // spec 0027 adds.
    #[allow(dead_code)]
    pub(crate) fn ensure_local_hardware(
        &mut self,
        detect: impl FnOnce() -> HardwareProfile,
    ) -> anyhow::Result<()> {
        if self.config.get_hardware(LOCAL_HARDWARE_ID).is_some() {
            return Ok(());
        }
        let local = HardwareConfig::local_from_detection(detect());
        let summary = describe(&local.profile().map_err(anyhow::Error::msg)?);
        self.config_mut()
            .insert_hardware(LOCAL_HARDWARE_ID, local)?;
        self.ui.info(&format!(
            "Saved hardware profile '{LOCAL_HARDWARE_ID}' from this machine: {summary}"
        ));
        Ok(())
    }

    fn invalidate(&mut self) {
        *self.sources.lock().unwrap() = None;
        self.hardware.lock().unwrap().clear();
    }
}

/*-- private --*/

/// A profile's memory in a few words, such as "unified 51.8 GB".
fn describe(profile: &HardwareProfile) -> String {
    let kind = match profile.memory_kind() {
        MemoryKind::Discrete => "discrete",
        MemoryKind::Unified => "unified",
        MemoryKind::System => "system",
    };
    format!("{kind} {:.1} GB", profile.memory.model_memory_gb())
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, TestConfigHome};
    use crate::utils::ui::base::tests::CaptureUi;

    fn detected() -> HardwareProfile {
        let mut profile = HardwareProfile::with_memory(crate::utils::hardware::Memory::Unified {
            vram_gb: 51.84,
            ram_gb: Some(64.0),
        });
        profile.gpu_vendor = Some("Apple".to_string());
        profile.machine = crate::utils::hardware::MachineInfo {
            os: "macos".to_string(),
            cpu_cores: Some(16),
            cpu_arch: "aarch64".to_string(),
        };
        profile
    }

    fn ctx(ui: Arc<CaptureUi>) -> AppContext {
        AppContext::new(Config::new().unwrap(), ui)
    }

    #[test]
    fn local_is_written_from_detection_on_first_use_with_a_notice() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(ui.clone());

        ctx.ensure_local_hardware(detected).unwrap();

        assert_eq!(ctx.hardware_profile(LOCAL_HARDWARE_ID), Ok(detected()));
        assert_eq!(
            *ui.infos.borrow(),
            vec!["Saved hardware profile 'local' from this machine: unified 51.8 GB".to_string()]
        );
        // Saved, so the next run finds it.
        let reloaded = Config::new().unwrap();
        assert!(reloaded.get_hardware(LOCAL_HARDWARE_ID).is_some());
    }

    #[test]
    fn an_existing_local_is_left_alone_and_detection_does_not_run() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(ui.clone());
        ctx.ensure_local_hardware(detected).unwrap();
        ui.infos.borrow_mut().clear();

        ctx.ensure_local_hardware(|| panic!("detection must not run"))
            .unwrap();

        assert!(ui.infos.borrow().is_empty());
        assert_eq!(ctx.hardware_profile(LOCAL_HARDWARE_ID), Ok(detected()));
    }

    #[test]
    fn an_unconfigured_hardware_profile_is_reported() {
        let _home = TestConfigHome::new();
        let ctx = ctx(Arc::new(CaptureUi::default()));

        assert_eq!(
            ctx.hardware_profile("spark"),
            Err("hardware profile 'spark' is not configured".to_string())
        );
    }

    #[test]
    fn a_configuration_write_discards_the_looked_up_figures() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        ctx.config_mut()
            .insert_hardware(
                "studio",
                HardwareConfig::catalog("studio", "apple-m4-max", 36.0),
            )
            .unwrap();
        assert_eq!(
            ctx.hardware_profile("studio").unwrap().memory.vram_gb(),
            Some(29.16)
        );

        ctx.config_mut()
            .insert_hardware(
                "studio",
                HardwareConfig::catalog("studio", "apple-m4-max", 64.0),
            )
            .unwrap();

        assert_eq!(
            ctx.hardware_profile("studio").unwrap().memory.vram_gb(),
            Some(51.84)
        );
    }
}
