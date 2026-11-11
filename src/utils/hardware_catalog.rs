//! The built-in hardware catalog: known devices and the memory sizes they are
//! sold in. `build.rs` generates `HARDWARE_CATALOG` from
//! `resources/hardware.yaml`, so a malformed file fails the build.

use crate::utils::hardware::{HardwareProfile, Memory, MemoryKind};

/*-- public --*/

/// One device in the catalog.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CatalogEntry {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    /// The group the catalog is ordered and filtered by, such as "Apple
    /// Silicon".
    pub(crate) family: &'static str,
    pub(crate) vendor: Option<&'static str>,
    pub(crate) memory_kind: MemoryKind,
    pub(crate) sizes: &'static [CatalogSize],
}

/// One memory size a device is sold in, in GiB.
#[derive(Debug, Clone)]
pub(crate) struct CatalogSize {
    /// VRAM for discrete memory, the whole pool for unified memory, system
    /// RAM for system memory.
    pub(crate) memory_gb: f64,
    /// For unified memory, the part of the pool the GPU can use.
    pub(crate) addressable_gb: Option<f64>,
}

impl CatalogEntry {
    /// The size this entry is sold in with `memory_gb` of memory, if any.
    pub(crate) fn size(&self, memory_gb: f64) -> Option<&CatalogSize> {
        self.sizes.iter().find(|s| s.memory_gb == memory_gb)
    }

    /// This device's memory figures at `size`. For unified memory the GPU
    /// figure is the addressable part of the pool, or the whole pool when the
    /// entry states no addressable figure.
    pub(crate) fn memory(&self, size: &CatalogSize) -> Memory {
        match self.memory_kind {
            MemoryKind::Discrete => Memory::Discrete {
                vram_gb: size.memory_gb,
                ram_gb: None,
            },
            MemoryKind::Unified => Memory::Unified {
                vram_gb: size.addressable_gb.unwrap_or(size.memory_gb),
                ram_gb: Some(size.memory_gb),
            },
            MemoryKind::System => Memory::System {
                ram_gb: size.memory_gb,
            },
        }
    }

    /// The hardware profile of this device at `size`: its memory figures and
    /// vendor. The OS and CPU are not known for a catalog device.
    pub(crate) fn profile(&self, size: &CatalogSize) -> HardwareProfile {
        let mut profile = HardwareProfile::with_memory(self.memory(size));
        profile.gpu_vendor = self.vendor.map(String::from);
        profile
    }

    /// Memory available for a model on this device at `size`, with the
    /// default headroom for its memory kind.
    #[allow(dead_code)]
    pub(crate) fn usable_memory_gb(&self, size: &CatalogSize) -> f64 {
        self.profile(size).usable_memory_gb()
    }
}

/// The catalog entry with this id.
pub(crate) fn find(id: &str) -> Option<&'static CatalogEntry> {
    HARDWARE_CATALOG.iter().find(|e| e.id == id)
}

// Defines `pub(crate) static HARDWARE_CATALOG: &[CatalogEntry]`, every entry of
// `resources/hardware.yaml` in file order.
include!(concat!(env!("OUT_DIR"), "/generated_hardware_catalog.rs"));

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn entry(id: &str) -> &'static CatalogEntry {
        find(id).unwrap_or_else(|| panic!("catalog has no entry '{id}'"))
    }

    #[test]
    fn every_entry_declares_at_least_one_size_above_zero() {
        for e in HARDWARE_CATALOG.iter() {
            assert!(!e.sizes.is_empty(), "'{}' has no sizes", e.id);
            for s in e.sizes {
                assert!(
                    s.memory_gb > 0.0,
                    "'{}' has a size of {} GiB",
                    e.id,
                    s.memory_gb
                );
            }
        }
    }

    #[test]
    fn every_entry_names_a_family() {
        for e in HARDWARE_CATALOG.iter() {
            assert!(!e.family.trim().is_empty(), "'{}' names no family", e.id);
        }
    }

    #[test]
    fn the_families_are_the_six_the_spec_names_in_order() {
        let mut families: Vec<&str> = vec![];
        for e in HARDWARE_CATALOG.iter() {
            if families.last() != Some(&e.family) {
                families.push(e.family);
            }
        }
        assert_eq!(
            families,
            [
                "Apple Silicon",
                "NVIDIA GeForce",
                "NVIDIA Workstation",
                "NVIDIA Data Center",
                "AMD",
                "CPU only",
            ]
        );
    }

    #[test]
    fn entries_of_one_family_are_kept_together() {
        let mut finished: HashSet<&str> = HashSet::new();
        let mut current = "";
        for e in HARDWARE_CATALOG.iter() {
            if e.family != current {
                finished.insert(current);
                assert!(
                    !finished.contains(e.family),
                    "'{}' is apart from the rest of family '{}'",
                    e.id,
                    e.family
                );
                current = e.family;
            }
        }
    }

    #[test]
    fn no_id_is_a_reserved_hardware_type() {
        // A configured profile's type is a catalog id, `custom` or `local`, and
        // a provider's link can be `unspecified`, so an entry with one of
        // those ids would be read as something else.
        for e in HARDWARE_CATALOG.iter() {
            assert!(
                e.id != crate::config::hardware::CUSTOM_HARDWARE_TYPE
                    && e.id != crate::config::hardware::LOCAL_HARDWARE_ID
                    && e.id != crate::config::hardware::UNSPECIFIED_HARDWARE,
                "catalog id '{}' is reserved",
                e.id
            );
        }
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = HashSet::new();
        for e in HARDWARE_CATALOG.iter() {
            assert!(seen.insert(&e.id), "duplicate id '{}'", e.id);
        }
    }

    #[test]
    fn only_unified_sizes_state_an_addressable_figure() {
        for e in HARDWARE_CATALOG.iter() {
            for s in e.sizes {
                let unified = e.memory_kind == MemoryKind::Unified;
                assert_eq!(
                    s.addressable_gb.is_some(),
                    unified,
                    "'{}' {} GiB: addressable_gb must be set for unified memory only",
                    e.id,
                    s.memory_gb
                );
                if let Some(addressable) = s.addressable_gb {
                    assert!(
                        addressable <= s.memory_gb,
                        "'{}' {} GiB: addressable_gb exceeds the pool",
                        e.id,
                        s.memory_gb
                    );
                }
            }
        }
    }

    #[test]
    fn discrete_entry_uses_vram_with_gpu_headroom() {
        let e = entry("nvidia-rtx-4090");
        assert_eq!(e.memory_kind, MemoryKind::Discrete);
        assert_eq!(e.profile(e.size(24.0).unwrap()).usable_memory_gb(), 16.0);
    }

    #[test]
    fn unified_entry_uses_addressable_figure_with_gpu_headroom() {
        // The addressable figure, 51.84, not the 64 GiB pool.
        let e = entry("apple-m4-pro");
        assert_eq!(e.memory_kind, MemoryKind::Unified);
        assert_eq!(
            e.profile(e.size(64.0).unwrap()).usable_memory_gb(),
            51.84 / 1.5
        );
    }

    #[test]
    fn system_entry_uses_ram_with_system_headroom() {
        let e = entry("cpu-only");
        assert_eq!(e.memory_kind, MemoryKind::System);
        assert_eq!(e.profile(e.size(64.0).unwrap()).usable_memory_gb(), 32.0);
    }

    #[test]
    fn apple_64_gib_entry_matches_a_detected_64_gib_mac() {
        // The measured working set on a 64 GiB Mac.
        let detected = HardwareProfile::with_memory(Memory::Unified {
            vram_gb: 51.84,
            ram_gb: Some(64.0),
        });
        let e = entry("apple-m4-max");
        assert_eq!(
            e.profile(e.size(64.0).unwrap()).usable_memory_gb(),
            detected.usable_memory_gb()
        );
    }

    #[test]
    fn a_size_the_entry_is_not_sold_in_is_not_found() {
        assert!(entry("nvidia-rtx-4090").size(48.0).is_none());
    }

    #[test]
    fn unknown_id_is_not_found() {
        assert!(find("no-such-device").is_none());
    }
}
