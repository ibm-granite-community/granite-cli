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
use crate::config::host::LOCAL_HOST_ID;
use crate::utils::hardware::HardwareProfile;

/*-- public --*/

pub(crate) struct AppContext {
    config: crate::config::Config,
    pub(crate) ui: std::sync::Arc<dyn crate::utils::ui::Ui>,
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
    /// The providers the setup wizard found in this run and may add, by the
    /// id they would be saved under, with their address and host link.
    /// Empty outside the wizard.
    found_providers: HashMap<String, crate::config::ProviderConfig>,
    /// Default base URLs a test has set for a provider type, in place of
    /// the registry's.
    #[cfg(test)]
    default_base_urls: HashMap<String, Vec<String>>,
    /// The devices of this machine after the first that a test says
    /// detection finds. Empty unless a test sets them.
    #[cfg(test)]
    other_gpus: Vec<HardwareProfile>,
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
            found_providers: HashMap::new(),
            #[cfg(test)]
            default_base_urls: HashMap::new(),
            #[cfg(test)]
            other_gpus: Vec::new(),
        }
    }

    /// The URLs a server of `provider_type` answers on by default, which the
    /// setup wizard probes: the registry's, unless a test has set others.
    /// Empty for a type the registry does not know.
    ///
    /// The environment variable `GRANITE_CLI_TEST_DEFAULT_BASE_URL` replaces
    /// the default URLs of every type with its value, so an end-to-end test
    /// can run `setup` without reaching a server on the machine or a hosted
    /// service.
    pub(crate) fn default_base_urls(&self, provider_type: &str) -> Vec<String> {
        #[cfg(test)]
        if let Some(urls) = self.default_base_urls.get(provider_type) {
            return urls.clone();
        }
        default_base_urls_with(provider_type, std::env::var(TEST_DEFAULT_BASE_URL_VAR).ok())
    }

    /// Makes `default_base_urls` return `urls` for `provider_type`, so a
    /// test can put a type's default port on a fake server.
    #[cfg(test)]
    pub(crate) fn set_default_base_urls(&mut self, provider_type: &str, urls: Vec<String>) {
        self.default_base_urls
            .insert(provider_type.to_string(), urls);
    }

    /// A provider the setup wizard found in this run under `id`, not
    /// configured yet.
    pub(crate) fn found_provider(&self, id: &str) -> Option<&crate::config::ProviderConfig> {
        self.found_providers.get(id)
    }

    /// Records a provider the setup wizard found, under its `provider_id`.
    pub(crate) fn add_found_provider(&mut self, provider: crate::config::ProviderConfig) {
        self.found_providers
            .insert(provider.provider_id.clone(), provider);
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
    pub(crate) fn hardware_profile(&self, id: &str) -> Result<HardwareProfile, String> {
        let mut held = self.hardware.lock().unwrap();
        held.entry(id.to_string())
            .or_insert_with(|| match self.config.get_hardware(id) {
                Some(hardware) => hardware.profile(),
                None => Err(format!("hardware profile '{id}' is not configured")),
            })
            .clone()
    }

    /// The figures fit is estimated on for a provider on host `id` that
    /// does not split a model: those of its device with the most usable
    /// memory. See [`Self::host_fit_profile`].
    pub(crate) fn host_profile(&self, id: &str) -> Result<HardwareProfile, String> {
        self.host_fit_profile(
            id,
            crate::providers::DeviceUse {
                splits: false,
                overhead_gb: 0.0,
            },
        )
    }

    /// The figures fit is estimated on for a provider on host `id` that uses
    /// its devices as `using` says. A provider that splits a model, on a
    /// host with several devices, gets their combined usable memory less
    /// `using.overhead_gb` for each device after the first; any other gets
    /// the device with the most usable memory. Fails with the reason when
    /// the host is not configured, has no devices (its hardware is not
    /// known), or a device's figures cannot be looked up.
    pub(crate) fn host_fit_profile(
        &self,
        id: &str,
        using: crate::providers::DeviceUse,
    ) -> Result<HardwareProfile, String> {
        let devices = self.host_devices(id)?;
        if using.splits && devices.len() > 1 {
            let profile = combined(&devices, using.overhead_gb);
            if profile.usable_memory_gb() <= 0.0 {
                return Err(format!(
                    "the devices of host '{id}' leave no memory after {} GB of overhead each",
                    using.overhead_gb
                ));
            }
            return Ok(profile);
        }
        Ok(devices
            .into_iter()
            .reduce(|best, d| {
                if d.usable_memory_gb() > best.usable_memory_gb() {
                    d
                } else {
                    best
                }
            })
            .expect("a host with devices has a largest one"))
    }

    /// The figures of each device of host `id`, in the order it lists them.
    /// Fails as [`Self::host_fit_profile`] does.
    pub(crate) fn host_devices(&self, id: &str) -> Result<Vec<HardwareProfile>, String> {
        if id != LOCAL_HOST_ID && self.config.get_host(id).is_none() {
            return Err(format!("host '{id}' is not configured"));
        }
        let devices = self.config.host_hardware(id);
        if devices.is_empty() {
            return Err(format!("the hardware of host '{id}' is not known"));
        }
        devices
            .iter()
            .map(|device| self.hardware_profile(device))
            .collect()
    }

    /// The devices of this machine after the first that detection finds:
    /// one per extra discrete GPU. In tests, what the test set with
    /// `set_other_gpus`, so no test reads the machine it runs on.
    pub(crate) fn other_gpus(&self) -> Vec<HardwareProfile> {
        #[cfg(test)]
        {
            self.other_gpus.clone()
        }
        #[cfg(not(test))]
        {
            HardwareProfile::detect_devices()
                .into_iter()
                .skip(1)
                .collect()
        }
    }

    /// Makes `other_gpus` return `gpus`.
    #[cfg(test)]
    pub(crate) fn set_other_gpus(&mut self, gpus: Vec<HardwareProfile>) {
        self.other_gpus = gpus;
    }

    /// Writes the `local` hardware profile from `detect` when there is none,
    /// and says so in one line, then the `local` host when there is none,
    /// with one device per GPU detection finds: `local`, then `local-2` and
    /// so on (see [`Self::set_local_devices`]). Does nothing when both exist.
    /// `detect` is a parameter so tests never read the machine they run on.
    pub(crate) fn ensure_local_host(
        &mut self,
        detect: impl FnOnce() -> HardwareProfile,
    ) -> anyhow::Result<()> {
        if self.config.get_hardware(LOCAL_HARDWARE_ID).is_none() {
            self.write_local_hardware(detect)?;
        }
        if self.config.get_host(LOCAL_HOST_ID).is_none() {
            let others = self.other_gpus();
            self.set_local_devices(others)?;
        }
        Ok(())
    }

    /// The profiles of the `local` host that detection wrote for the
    /// devices after the first (see `hardware::is_detected_device`), in the
    /// order the host lists them.
    pub(crate) fn detected_local_devices(&self) -> Vec<String> {
        self.config
            .host_hardware(LOCAL_HOST_ID)
            .into_iter()
            .filter(|id| {
                self.config
                    .get_hardware(id)
                    .is_some_and(crate::config::hardware::is_detected_device)
            })
            .collect()
    }

    /// Saves the `local` host with the `local` profile, one `local-<n>`
    /// profile in auto mode per device in `others`, and any other profile the
    /// user listed on it. Then removes the `local-<n>` profiles detection
    /// wrote before that are no longer listed and that no other host lists.
    /// Says so in one line when there are other devices.
    pub(crate) fn set_local_devices(&mut self, others: Vec<HardwareProfile>) -> anyhow::Result<()> {
        let before = self.detected_local_devices();
        let kept: Vec<String> = self
            .config
            .host_hardware(LOCAL_HOST_ID)
            .into_iter()
            .filter(|id| id != LOCAL_HARDWARE_ID && !before.contains(id))
            .collect();
        let mut hardware = vec![LOCAL_HARDWARE_ID.to_string()];
        let mut summaries = Vec::new();
        // A numbered id the user took for a profile of their own is skipped.
        let mut n = 1;
        for found in others {
            n += 1;
            while self
                .config
                .get_hardware(&crate::config::hardware::local_device_id(n))
                .is_some_and(|hw| !crate::config::hardware::is_detected_device(hw))
            {
                n += 1;
            }
            let device = HardwareConfig::local_device(n, found);
            summaries.push(format!(
                "{} ({})",
                device.hardware_id,
                device
                    .profile()
                    .map_err(anyhow::Error::msg)?
                    .memory_summary()
            ));
            hardware.push(device.hardware_id.clone());
            self.config_mut()
                .insert_hardware(&device.hardware_id.clone(), device)?;
        }
        hardware.extend(kept);
        // The host first, so it never names a profile that is gone.
        self.config_mut().insert_host(
            LOCAL_HOST_ID,
            crate::config::HostConfig {
                hardware: hardware.clone(),
                ..crate::config::HostConfig::local()
            },
        )?;
        for gone in before.iter().filter(|id| !hardware.contains(id)) {
            let elsewhere = self
                .config
                .hosts
                .values()
                .any(|h| h.host_id != LOCAL_HOST_ID && h.hardware.contains(gone));
            if !elsewhere {
                self.config_mut().remove_hardware(gone)?;
            }
        }
        if !summaries.is_empty() {
            self.ui.info_stderr(&format!(
                "Saved the other devices of this machine on host '{LOCAL_HOST_ID}': {}",
                summaries.join(", ")
            ));
        }
        Ok(())
    }

    fn write_local_hardware(
        &mut self,
        detect: impl FnOnce() -> HardwareProfile,
    ) -> anyhow::Result<()> {
        let local = HardwareConfig::local_from_detection(detect());
        let summary = local
            .profile()
            .map_err(anyhow::Error::msg)?
            .memory_summary();
        self.config_mut()
            .insert_hardware(LOCAL_HARDWARE_ID, local)?;
        self.ui.info_stderr(&format!(
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

/// One profile standing for `devices` used together: their usable memory
/// added up, less `overhead_gb` for each device after the first, as the
/// figure of the first device's memory kind with a headroom of 1, so the
/// estimator uses it as it is.
fn combined(devices: &[HardwareProfile], overhead_gb: f64) -> HardwareProfile {
    use crate::utils::hardware::Memory;
    let usable: f64 = devices
        .iter()
        .map(HardwareProfile::usable_memory_gb)
        .sum::<f64>()
        - overhead_gb * (devices.len() as f64 - 1.0);
    let first = devices[0].clone();
    let memory = match first.memory {
        Memory::Discrete { ram_gb, .. } => Memory::Discrete {
            vram_gb: usable,
            ram_gb,
        },
        Memory::Unified { ram_gb, .. } => Memory::Unified {
            vram_gb: usable,
            ram_gb,
        },
        Memory::System { .. } => Memory::System { ram_gb: usable },
    };
    HardwareProfile {
        memory,
        headroom: Some(1.0),
        ..first
    }
}

/// The environment variable that replaces every provider type's default base
/// URLs, for end-to-end tests. See [`AppContext::default_base_urls`].
const TEST_DEFAULT_BASE_URL_VAR: &str = "GRANITE_CLI_TEST_DEFAULT_BASE_URL";

/// The default base URLs of `provider_type`: `[replacement]` when the
/// replacement is set and not empty, the registry's otherwise. Empty for a
/// type the registry does not know.
fn default_base_urls_with(provider_type: &str, replacement: Option<String>) -> Vec<String> {
    let Some(metadata) = crate::providers::PROVIDER_REGISTRY.get(provider_type) else {
        return Vec::new();
    };
    match replacement {
        Some(url) if !url.is_empty() => vec![url],
        _ => metadata.default_base_urls.clone(),
    }
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

        ctx.ensure_local_host(detected).unwrap();

        assert_eq!(ctx.hardware_profile(LOCAL_HARDWARE_ID), Ok(detected()));
        assert_eq!(
            *ui.stderr_infos.borrow(),
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
        ctx.ensure_local_host(detected).unwrap();
        ui.stderr_infos.borrow_mut().clear();

        ctx.ensure_local_host(|| panic!("detection must not run"))
            .unwrap();

        assert!(ui.stderr_infos.borrow().is_empty());
        assert_eq!(ctx.hardware_profile(LOCAL_HARDWARE_ID), Ok(detected()));
    }

    #[test]
    fn default_base_urls_are_the_registry_s_unless_replaced() {
        let ollama = vec!["http://localhost:11434".to_string()];
        assert_eq!(default_base_urls_with("ollama", None), ollama);
        assert_eq!(
            default_base_urls_with("ollama", Some(String::new())),
            ollama
        );
        assert_eq!(
            default_base_urls_with("ollama", Some("http://127.0.0.1:9".to_string())),
            vec!["http://127.0.0.1:9".to_string()]
        );
        assert!(default_base_urls_with("no-such-type", Some("http://x".to_string())).is_empty());
    }

    #[test]
    fn a_test_can_set_one_type_s_default_base_urls() {
        let mut ctx = AppContext::new(Config::default(), Arc::new(CaptureUi::default()));

        ctx.set_default_base_urls("ollama", vec!["http://127.0.0.1:1".to_string()]);

        assert_eq!(
            ctx.default_base_urls("ollama"),
            vec!["http://127.0.0.1:1".to_string()]
        );
    }

    #[test]
    fn first_use_writes_the_local_host_with_the_local_profile() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));

        ctx.ensure_local_host(detected).unwrap();

        let reloaded = Config::new().unwrap();
        assert_eq!(
            reloaded.get_host(LOCAL_HOST_ID),
            Some(&crate::config::HostConfig::local())
        );
    }

    fn with_host(ctx: &mut AppContext, id: &str, hardware: &[&str]) {
        ctx.config_mut()
            .insert_host(
                id,
                crate::config::HostConfig {
                    host_id: id.to_string(),
                    address: format!("{id}.lan"),
                    hardware: hardware.iter().map(|h| h.to_string()).collect(),
                },
            )
            .unwrap();
    }

    #[test]
    fn fit_on_a_host_uses_its_largest_device() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        for (id, catalog_id, gb) in [
            ("spark", "dgx-spark", 128.0),
            ("card", "nvidia-rtx-4090", 24.0),
        ] {
            ctx.config_mut()
                .insert_hardware(id, HardwareConfig::catalog(id, catalog_id, gb))
                .unwrap();
        }
        with_host(&mut ctx, "pgx", &["spark"]);
        with_host(&mut ctx, "box", &["card", "card"]);

        // The same figures as the profile a provider linked to before.
        assert_eq!(ctx.host_profile("pgx"), ctx.hardware_profile("spark"));
        assert_eq!(ctx.host_profile("box"), ctx.hardware_profile("card"));
    }

    fn splitting(overhead_gb: f64) -> crate::providers::DeviceUse {
        crate::providers::DeviceUse {
            splits: true,
            overhead_gb,
        }
    }

    #[test]
    fn fit_on_two_equal_devices_adds_them_up_for_a_provider_that_splits() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        ctx.config_mut()
            .insert_hardware(
                "card",
                HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0),
            )
            .unwrap();
        with_host(&mut ctx, "box", &["card", "card"]);
        with_host(&mut ctx, "one", &["card"]);
        let one_card = ctx.hardware_profile("card").unwrap().usable_memory_gb();

        let split = ctx.host_fit_profile("box", splitting(1.0)).unwrap();
        let single = ctx.host_profile("box").unwrap();

        assert_eq!(one_card, 16.0);
        assert_eq!(split.usable_memory_gb(), 2.0 * 16.0 - 1.0);
        assert_eq!(single.usable_memory_gb(), 16.0);
        // One device: the same whether the provider splits or not.
        assert_eq!(
            ctx.host_fit_profile("one", splitting(1.0)),
            ctx.host_profile("one")
        );
    }

    #[test]
    fn fit_on_two_unequal_devices_adds_them_up_or_uses_the_larger() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        let card = |vram_gb: f64| {
            crate::utils::hardware::HardwareProfile::with_memory(
                crate::utils::hardware::Memory::Discrete {
                    vram_gb,
                    ram_gb: None,
                },
            )
        };
        for (id, gb) in [("big", 24.0), ("small", 12.0)] {
            ctx.config_mut()
                .insert_hardware(id, HardwareConfig::custom(id, card(gb)))
                .unwrap();
        }
        with_host(&mut ctx, "mixed", &["small", "big"]);

        let split = ctx.host_fit_profile("mixed", splitting(0.5)).unwrap();
        let single = ctx.host_profile("mixed").unwrap();

        assert_eq!(split.usable_memory_gb(), 16.0 + 8.0 - 0.5);
        assert_eq!(single.usable_memory_gb(), 16.0);
        assert_eq!(single.memory, card(24.0).memory);
    }

    #[test]
    fn an_overhead_that_leaves_no_memory_is_reported() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        ctx.config_mut()
            .insert_hardware(
                "card",
                HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0),
            )
            .unwrap();
        with_host(&mut ctx, "box", &["card", "card"]);

        assert_eq!(
            ctx.host_fit_profile("box", splitting(40.0)),
            Err(
                "the devices of host 'box' leave no memory after 40 GB of overhead each"
                    .to_string()
            )
        );
    }

    #[test]
    fn updating_the_local_devices_keeps_profiles_the_user_listed_and_shared_ones() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        let card = || {
            HardwareProfile::with_memory(crate::utils::hardware::Memory::Discrete {
                vram_gb: 24.0,
                ram_gb: None,
            })
        };
        ctx.ensure_local_host(card).unwrap();
        ctx.set_local_devices(vec![card(), card()]).unwrap();
        // A profile the user added by hand, and local-3 also listed elsewhere.
        ctx.config_mut()
            .insert_hardware("local-rtx", HardwareConfig::custom("local-rtx", card()))
            .unwrap();
        let mut local = ctx.config().get_host(LOCAL_HOST_ID).cloned().unwrap();
        local.hardware.push("local-rtx".to_string());
        ctx.config_mut().insert_host(LOCAL_HOST_ID, local).unwrap();
        with_host(&mut ctx, "twin", &["local-3"]);

        ctx.set_local_devices(Vec::new()).unwrap();

        assert_eq!(
            ctx.config().host_hardware(LOCAL_HOST_ID),
            vec!["local", "local-rtx"]
        );
        assert!(ctx.config().get_hardware("local-2").is_none());
        assert!(
            ctx.config().get_hardware("local-3").is_some(),
            "listed by host 'twin'"
        );
        assert!(ctx.config().get_hardware("local-rtx").is_some());
    }

    #[test]
    fn a_numbered_profile_the_user_made_is_not_overwritten_or_compared() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(ui.clone());
        let card = |vram_gb: f64| {
            HardwareProfile::with_memory(crate::utils::hardware::Memory::Discrete {
                vram_gb,
                ram_gb: None,
            })
        };
        ctx.ensure_local_host(|| card(24.0)).unwrap();
        ctx.config_mut()
            .insert_hardware("local-2", HardwareConfig::custom("local-2", card(12.0)))
            .unwrap();
        let mut local = ctx.config().get_host(LOCAL_HOST_ID).cloned().unwrap();
        local.hardware.push("local-2".to_string());
        ctx.config_mut().insert_host(LOCAL_HOST_ID, local).unwrap();
        ctx.set_other_gpus(vec![card(24.0)]);

        ctx.set_local_devices(vec![card(24.0)]).unwrap();

        assert_eq!(
            ctx.config().host_hardware(LOCAL_HOST_ID),
            vec!["local", "local-3", "local-2"]
        );
        assert_eq!(
            ctx.hardware_profile("local-2").unwrap().memory,
            card(12.0).memory
        );
        // The check compares only the devices detection wrote.
        crate::commands::HardwareCommands::check_local(&mut ctx, || card(24.0), true).unwrap();
        assert!(ui.confirm_prompts.borrow().is_empty());
        assert!(ui.warns.borrow().is_empty());
    }

    #[test]
    fn fit_on_a_host_with_no_devices_or_none_configured_is_not_available() {
        let _home = TestConfigHome::new();
        let mut ctx = ctx(Arc::new(CaptureUi::default()));
        with_host(&mut ctx, "lab", &[]);

        assert_eq!(
            ctx.host_profile("lab"),
            Err("the hardware of host 'lab' is not known".to_string())
        );
        assert_eq!(
            ctx.host_profile("gone"),
            Err("host 'gone' is not configured".to_string())
        );
    }

    #[test]
    fn first_use_lists_every_gpu_of_this_machine_on_the_local_host() {
        let _home = TestConfigHome::new();
        let ui = Arc::new(CaptureUi::default());
        let mut ctx = ctx(ui.clone());
        let card = || {
            HardwareProfile::with_memory(crate::utils::hardware::Memory::Discrete {
                vram_gb: 24.0,
                ram_gb: Some(64.0),
            })
        };
        ctx.set_other_gpus(vec![card()]);

        ctx.ensure_local_host(card).unwrap();

        assert_eq!(
            ctx.config().host_hardware(LOCAL_HOST_ID),
            vec!["local", "local-2"]
        );
        assert_eq!(
            ctx.hardware_profile("local-2").unwrap().memory,
            card().memory
        );
        assert!(
            ui.stderr_infos.borrow()[1].contains("local-2 (discrete 24.0 GB)"),
            "{:?}",
            ui.stderr_infos.borrow()
        );
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
