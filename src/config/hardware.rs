//! Configured hardware profiles: what is saved for each one, and how its
//! memory figures are looked up.
//!
//! A configured hardware profile has the shape of a configured model: an id,
//! a type and a selector. The type is a catalog id, `custom` or `local`. A
//! catalog profile saves only the catalog id and the size chosen (`memory`),
//! and its figures are read from the catalog each time they are looked up, so
//! a release that corrects a catalog figure changes the estimate and leaves
//! the saved file alone. Custom and local profiles save their figures in
//! `config`; `local` also saves its detection mode there. Any profile may
//! save `headroom`, a factor that replaces the default for its memory kind.

use serde::{Deserialize, Serialize};

use crate::utils::hardware::{HardwareProfile, Memory};
use crate::utils::hardware_catalog;

/*-- public --*/

/// The id, and type, of the hardware profile that describes the machine
/// granite-cli runs on.
pub(crate) const LOCAL_HARDWARE_ID: &str = "local";

/// The type of a hardware profile whose figures the user typed.
pub(crate) const CUSTOM_HARDWARE_TYPE: &str = "custom";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HardwareConfig {
    /// Instance id: the file name this profile is saved under.
    pub(crate) hardware_id: String,
    /// A `resources/hardware.yaml` id, `custom` or `local`.
    #[serde(rename = "type")]
    pub(crate) hardware_type: String,
    /// For a catalog profile, the size chosen, in GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) memory: Option<f64>,
    /// The factor the estimator divides the memory figure by, in place of
    /// the default for the memory kind. At least 1.0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) headroom: Option<f64>,
    /// For custom and local profiles, the figures, as a `HardwareProfile`;
    /// for `local`, also `mode`.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub(crate) config: serde_json::Value,
}

/// Whether `local` follows detection (`auto`) or keeps figures the user chose
/// for this machine (`manual`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LocalMode {
    Auto,
    Manual,
}

/// What `local` saves in `config`: its mode, its figures, and in manual mode
/// the detected figures the saved ones were kept over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LocalHardware {
    pub(crate) mode: LocalMode,
    #[serde(flatten)]
    pub(crate) profile: HardwareProfile,
    /// In manual mode, what detection read when the user kept the saved
    /// figures. The same detection is not asked about again; a different
    /// one, such as on another machine, is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) detected: Option<HardwareProfile>,
}

/// Detected figures as `local` saves them: memory rounded to 0.01 GB.
/// Detection reads some figures as 32-bit floats, which would otherwise be
/// saved as values like `51.84001159667969`.
pub(crate) fn saved_figures(detected: HardwareProfile) -> HardwareProfile {
    let round = |gb: f64| (gb * 100.0).round() / 100.0;
    let memory = match detected.memory {
        Memory::Discrete { vram_gb, ram_gb } => Memory::Discrete {
            vram_gb: round(vram_gb),
            ram_gb: ram_gb.map(round),
        },
        Memory::Unified { vram_gb, ram_gb } => Memory::Unified {
            vram_gb: round(vram_gb),
            ram_gb: ram_gb.map(round),
        },
        Memory::System { ram_gb } => Memory::System {
            ram_gb: round(ram_gb),
        },
    };
    HardwareProfile { memory, ..detected }
}

impl HardwareConfig {
    /// A `local` profile holding detected figures, in auto mode.
    pub(crate) fn local_from_detection(detected: HardwareProfile) -> Self {
        Self::local(LocalHardware {
            mode: LocalMode::Auto,
            profile: saved_figures(detected),
            detected: None,
        })
    }

    /// A `local` profile holding `settings`.
    pub(crate) fn local(settings: LocalHardware) -> Self {
        Self {
            hardware_id: LOCAL_HARDWARE_ID.to_string(),
            hardware_type: LOCAL_HARDWARE_ID.to_string(),
            memory: None,
            headroom: None,
            config: serde_json::to_value(settings).expect("LocalHardware serializes to JSON"),
        }
    }

    /// The saved settings of a `local` profile, or `None` for any other
    /// profile and for one that cannot be read.
    pub(crate) fn local_settings(&self) -> Option<LocalHardware> {
        if self.hardware_type != LOCAL_HARDWARE_ID {
            return None;
        }
        self.read_local().ok()
    }

    /// The saved settings of a `local` profile, or why they cannot be read.
    /// The recorded detection is read on its own, so an error in it names
    /// `detected`.
    pub(crate) fn read_local(&self) -> Result<LocalHardware, String> {
        let mut config = self.config.clone();
        let detected = config
            .as_object_mut()
            .and_then(|fields| fields.remove("detected"));
        let mut local: LocalHardware = serde_json::from_value(config)
            .map_err(|e| format!("the saved figures cannot be read: {e}"))?;
        local.detected =
            match detected {
                None | Some(serde_json::Value::Null) => None,
                Some(detected) => Some(serde_json::from_value(detected).map_err(|e| {
                    format!("the recorded detection (`detected`) cannot be read: {e}")
                })?),
            };
        Ok(local)
    }

    /// A custom profile holding figures the user typed.
    pub(crate) fn custom(hardware_id: &str, profile: HardwareProfile) -> Self {
        Self {
            hardware_id: hardware_id.to_string(),
            hardware_type: CUSTOM_HARDWARE_TYPE.to_string(),
            memory: None,
            headroom: None,
            config: serde_json::to_value(profile).expect("HardwareProfile serializes to JSON"),
        }
    }

    /// A profile for catalog entry `catalog_id` at `memory_gb`.
    pub(crate) fn catalog(hardware_id: &str, catalog_id: &str, memory_gb: f64) -> Self {
        Self {
            hardware_id: hardware_id.to_string(),
            hardware_type: catalog_id.to_string(),
            memory: Some(memory_gb),
            headroom: None,
            config: serde_json::Value::Null,
        }
    }

    /// The figures this profile stands for: read from the catalog for a
    /// catalog profile, from `config` for custom and local ones, with the
    /// profile's headroom factor. Fails with the reason when they cannot be
    /// found or cannot give an estimate.
    pub(crate) fn profile(&self) -> Result<HardwareProfile, String> {
        let mut profile = match self.hardware_type.as_str() {
            LOCAL_HARDWARE_ID => self.read_local()?.profile,
            CUSTOM_HARDWARE_TYPE => serde_json::from_value::<HardwareProfile>(self.config.clone())
                .map_err(|e| format!("the saved figures cannot be read: {e}"))?,
            catalog_id => catalog_profile(catalog_id, self.memory)?,
        };
        profile.check_figures()?;
        if let Some(factor) = self.headroom {
            if !factor.is_finite() || factor < 1.0 {
                return Err(format!("headroom must be at least 1.0, got {factor}"));
            }
            profile.headroom = Some(factor);
        }
        Ok(profile)
    }
}

/*-- private --*/

fn catalog_profile(catalog_id: &str, memory: Option<f64>) -> Result<HardwareProfile, String> {
    let entry = hardware_catalog::find(catalog_id)
        .ok_or_else(|| format!("the hardware catalog has no entry '{catalog_id}'"))?;
    let memory_gb = memory.ok_or_else(|| format!("no memory size is set for '{catalog_id}'"))?;
    let size = entry.size(memory_gb).ok_or_else(|| {
        let offered: Vec<String> = entry
            .sizes
            .iter()
            .map(|s| s.memory_gb.to_string())
            .collect();
        format!(
            "'{catalog_id}' is not offered with {memory_gb} GB; the catalog lists {} GB",
            offered.join(", ")
        )
    })?;
    Ok(entry.profile(size))
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    fn discrete(vram_gb: f64) -> HardwareProfile {
        HardwareProfile::with_memory(Memory::Discrete {
            vram_gb,
            ram_gb: None,
        })
    }

    fn unified(vram_gb: f64, ram_gb: f64) -> HardwareProfile {
        HardwareProfile::with_memory(Memory::Unified {
            vram_gb,
            ram_gb: Some(ram_gb),
        })
    }

    #[test]
    fn catalog_profile_gets_the_catalog_figures_for_its_size() {
        let p = HardwareConfig::catalog("studio", "apple-m4-max", 64.0)
            .profile()
            .unwrap();
        assert_eq!(p.memory, unified(51.84, 64.0).memory);
        assert_eq!(p.gpu_vendor.as_deref(), Some("Apple"));
    }

    #[test]
    fn catalog_profile_with_a_size_no_longer_offered_fails_with_the_sizes_offered() {
        let err = HardwareConfig::catalog("card", "nvidia-rtx-4090", 48.0)
            .profile()
            .unwrap_err();
        assert_eq!(
            err,
            "'nvidia-rtx-4090' is not offered with 48 GB; the catalog lists 24 GB"
        );
    }

    #[test]
    fn catalog_profile_with_an_unknown_entry_fails() {
        let err = HardwareConfig::catalog("old", "no-such-device", 24.0)
            .profile()
            .unwrap_err();
        assert_eq!(err, "the hardware catalog has no entry 'no-such-device'");
    }

    #[test]
    fn custom_profile_gets_its_own_figures() {
        let saved = discrete(48.0);
        let p = HardwareConfig::custom("lab-box", saved.clone())
            .profile()
            .unwrap();
        assert_eq!(p, saved);
    }

    #[test]
    fn custom_profile_without_the_figure_its_kind_names_fails() {
        let mut lab = HardwareConfig::custom("lab-box", discrete(48.0));
        lab.config = serde_json::json!({ "memory_kind": "system", "vram_gb": 24.0 });
        let err = lab.profile().unwrap_err();
        assert_eq!(
            err,
            "the saved figures cannot be read: a system hardware profile has no vram_gb"
        );
    }

    #[test]
    fn local_profile_gets_its_saved_figures_in_auto_mode() {
        let detected = unified(51.84, 64.0);
        let local = HardwareConfig::local_from_detection(detected.clone());
        assert_eq!(local.hardware_id, LOCAL_HARDWARE_ID);
        assert_eq!(local.profile().unwrap(), detected);
        let saved: LocalHardware = serde_json::from_value(local.config).unwrap();
        assert_eq!(saved.mode, LocalMode::Auto);
    }

    #[test]
    fn local_profile_saves_detected_memory_to_two_decimals() {
        let detected = unified(51.840_011_596_679_69, 64.0);
        let yaml = serde_yaml::to_string(&HardwareConfig::local_from_detection(detected)).unwrap();
        assert!(yaml.contains("vram_gb: 51.84\n"), "{yaml}");
        assert!(yaml.contains("ram_gb: 64.0\n"), "{yaml}");
    }

    #[test]
    fn saved_files_round_trip_through_yaml() {
        let configs = [
            HardwareConfig::catalog("spark", "dgx-spark", 128.0),
            HardwareConfig::custom("lab-box", discrete(48.0)),
            HardwareConfig::local_from_detection(unified(51.84, 64.0)),
            HardwareConfig {
                headroom: Some(1.2),
                ..HardwareConfig::catalog("studio", "apple-m4-max", 128.0)
            },
        ];
        for config in configs {
            let yaml = serde_yaml::to_string(&config).unwrap();
            let read: HardwareConfig = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(read, config, "{yaml}");
        }
    }

    #[test]
    fn a_stored_factor_reaches_the_estimator_for_every_kind_of_profile() {
        let profiles = [
            HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0),
            HardwareConfig::custom("lab-box", discrete(24.0)),
            HardwareConfig::local_from_detection(discrete(24.0)),
        ];
        for config in profiles {
            let default = config.profile().unwrap().usable_memory_gb();
            let set = HardwareConfig {
                headroom: Some(1.2),
                ..config.clone()
            }
            .profile()
            .unwrap();
            assert_eq!(default, 16.0, "{}", config.hardware_id);
            assert_eq!(set.headroom, Some(1.2), "{}", config.hardware_id);
            assert_eq!(set.usable_memory_gb(), 20.0, "{}", config.hardware_id);
        }
    }

    #[test]
    fn a_factor_below_one_is_reported() {
        let config = HardwareConfig {
            headroom: Some(0.9),
            ..HardwareConfig::catalog("card", "nvidia-rtx-4090", 24.0)
        };
        assert_eq!(
            config.profile().unwrap_err(),
            "headroom must be at least 1.0, got 0.9"
        );
    }

    #[test]
    fn the_factor_is_saved_next_to_memory() {
        let config = HardwareConfig {
            headroom: Some(1.2),
            ..HardwareConfig::catalog("studio", "apple-m4-max", 128.0)
        };
        assert_eq!(
            serde_yaml::to_string(&config).unwrap(),
            "hardware_id: studio\ntype: apple-m4-max\nmemory: 128.0\nheadroom: 1.2\n"
        );
    }

    #[test]
    fn a_local_profile_with_an_unreadable_detection_names_it() {
        let mut local = HardwareConfig::local_from_detection(discrete(24.0));
        local.config["mode"] = serde_json::json!("manual");
        local.config["detected"] = serde_json::json!({ "memory_kind": "discrete" });
        let err = local.profile().unwrap_err();
        assert!(
            err.starts_with("the recorded detection (`detected`) cannot be read:"),
            "{err}"
        );
    }

    #[test]
    fn catalog_profile_saves_no_figures() {
        let yaml =
            serde_yaml::to_string(&HardwareConfig::catalog("spark", "dgx-spark", 128.0)).unwrap();
        assert_eq!(yaml, "hardware_id: spark\ntype: dgx-spark\nmemory: 128.0\n");
    }
}
