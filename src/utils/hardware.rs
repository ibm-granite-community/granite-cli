use serde::{Deserialize, Serialize};

/*-- public --*/

/// Which of a profile's memory figures holds the model. Discrete and unified
/// memory are read from `vram_gb`, the memory the accelerator can address;
/// system memory from `ram_gb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MemoryKind {
    /// A card with its own VRAM, such as an RTX 4090.
    Discrete,
    /// An accelerator that shares one pool with the CPU, such as Apple Silicon
    /// or a DGX Spark. `vram_gb` is the part of the pool it can address.
    Unified,
    /// No accelerator: the model runs from system RAM.
    System,
}

impl MemoryKind {
    /// The factor the estimator divides this kind's figure by when a profile
    /// sets none.
    pub(crate) fn default_headroom(self) -> f64 {
        match self {
            MemoryKind::Discrete | MemoryKind::Unified => 1.5,
            MemoryKind::System => 2.0,
        }
    }
}

/// A profile's memory figures, in GiB. Which figures there are depends on the
/// kind, so a profile without the figure its kind names does not load.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "memory_kind", rename_all = "lowercase")]
pub(crate) enum Memory {
    /// `vram_gb` is the card's VRAM.
    Discrete {
        vram_gb: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ram_gb: Option<f64>,
    },
    /// `vram_gb` is the part of the shared pool the GPU can address.
    Unified {
        vram_gb: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ram_gb: Option<f64>,
    },
    System {
        ram_gb: f64,
    },
}

impl Memory {
    pub(crate) fn kind(&self) -> MemoryKind {
        match self {
            Memory::Discrete { .. } => MemoryKind::Discrete,
            Memory::Unified { .. } => MemoryKind::Unified,
            Memory::System { .. } => MemoryKind::System,
        }
    }

    /// The figure the kind names: accelerator memory for discrete and
    /// unified, system RAM for system.
    pub(crate) fn model_memory_gb(&self) -> f64 {
        match *self {
            Memory::Discrete { vram_gb, .. } | Memory::Unified { vram_gb, .. } => vram_gb,
            Memory::System { ram_gb } => ram_gb,
        }
    }

    /// Accelerator memory, for discrete and unified profiles.
    pub(crate) fn vram_gb(&self) -> Option<f64> {
        match *self {
            Memory::Discrete { vram_gb, .. } | Memory::Unified { vram_gb, .. } => Some(vram_gb),
            Memory::System { .. } => None,
        }
    }

    /// System RAM, when the profile states it.
    pub(crate) fn ram_gb(&self) -> Option<f64> {
        match *self {
            Memory::Discrete { ram_gb, .. } | Memory::Unified { ram_gb, .. } => ram_gb,
            Memory::System { ram_gb } => Some(ram_gb),
        }
    }
}

/// Descriptive fields about the machine. The estimator reads none of them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct MachineInfo {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cpu_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) cpu_arch: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "StoredProfile")]
pub struct HardwareProfile {
    #[serde(flatten)]
    pub(crate) memory: Memory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) gpu_vendor: Option<String>,
    #[serde(default, skip_serializing_if = "MachineInfo::is_empty")]
    pub(crate) machine: MachineInfo,
    /// A factor that replaces the kind's default headroom. It is stored on
    /// the configured hardware profile, not with the figures, and set here
    /// when the figures are looked up.
    #[serde(skip)]
    pub(crate) headroom: Option<f64>,
}

impl HardwareProfile {
    /// A profile with these figures and nothing else.
    pub(crate) fn with_memory(memory: Memory) -> Self {
        Self {
            memory,
            gpu_vendor: None,
            machine: MachineInfo::default(),
            headroom: None,
        }
    }

    pub fn detect() -> Self {
        Self::from_findings(Findings {
            os: std::env::consts::OS.to_string(),
            cpu_cores: num_cpus::get() as u32,
            cpu_arch: std::env::consts::ARCH.to_string(),
            total_ram_bytes: total_memory_bytes(),
            gpu: probe_gpu(),
        })
    }

    pub(crate) fn memory_kind(&self) -> MemoryKind {
        self.memory.kind()
    }

    /// The headroom factor in use: the profile's own, or the kind's default.
    pub(crate) fn headroom(&self) -> f64 {
        self.headroom
            .unwrap_or_else(|| self.memory_kind().default_headroom())
    }

    /// Memory budget available for model weights and KV cache: the figure
    /// the kind names, divided by the headroom factor.
    pub fn usable_memory_gb(&self) -> f64 {
        self.memory.model_memory_gb() / self.headroom()
    }

    /// Why this profile cannot give an estimate, if it cannot: a memory
    /// figure that is not a positive, finite number.
    pub(crate) fn check_figures(&self) -> Result<(), String> {
        let figures = [
            ("vram_gb", self.memory.vram_gb()),
            ("ram_gb", self.memory.ram_gb()),
        ];
        for (name, figure) in figures {
            if let Some(gb) = figure
                && (!gb.is_finite() || gb <= 0.0)
            {
                return Err(format!("{name} must be positive, got {gb}"));
            }
        }
        Ok(())
    }
}

pub fn detect_hardware() -> HardwareProfile {
    HardwareProfile::detect()
}

/*-- private --*/

impl MachineInfo {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// A hardware profile as written in a file. Loading goes through this so
/// that a figure the kind does not have, such as `vram_gb` on a system
/// profile, is an error instead of being dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProfile {
    memory_kind: MemoryKind,
    #[serde(default)]
    vram_gb: Option<f64>,
    #[serde(default)]
    ram_gb: Option<f64>,
    #[serde(default)]
    gpu_vendor: Option<String>,
    #[serde(default)]
    machine: MachineInfo,
}

impl TryFrom<StoredProfile> for HardwareProfile {
    type Error = String;

    fn try_from(stored: StoredProfile) -> Result<Self, Self::Error> {
        let kind = stored.memory_kind;
        let memory = match (kind, stored.vram_gb, stored.ram_gb) {
            (MemoryKind::Discrete, Some(vram_gb), ram_gb) => Memory::Discrete { vram_gb, ram_gb },
            (MemoryKind::Unified, Some(vram_gb), ram_gb) => Memory::Unified { vram_gb, ram_gb },
            (MemoryKind::System, None, Some(ram_gb)) => Memory::System { ram_gb },
            (MemoryKind::Discrete | MemoryKind::Unified, None, _) => {
                return Err(format!(
                    "a {} hardware profile must state vram_gb",
                    kind_name(kind)
                ));
            }
            (MemoryKind::System, Some(_), _) => {
                return Err("a system hardware profile has no vram_gb".to_string());
            }
            (MemoryKind::System, None, None) => {
                return Err("a system hardware profile must state ram_gb".to_string());
            }
        };
        Ok(Self {
            memory,
            gpu_vendor: stored.gpu_vendor,
            machine: stored.machine,
            headroom: None,
        })
    }
}

fn kind_name(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Discrete => "discrete",
        MemoryKind::Unified => "unified",
        MemoryKind::System => "system",
    }
}

/// What the probes read from the operating system, before any decision about
/// memory kind.
struct Findings {
    os: String,
    cpu_cores: u32,
    cpu_arch: String,
    total_ram_bytes: u64,
    gpu: Option<GpuFinding>,
}

struct GpuFinding {
    vendor: String,
    memory: GpuMemory,
}

/// How a GPU reported its memory.
enum GpuMemory {
    /// Memory the card owns (NVML, AMD sysfs, DXGI), in bytes.
    Dedicated(u64),
    /// No framebuffer of its own: the GPU uses system RAM over a coherent link
    /// (NVML `NotSupported`, as on the Grace Blackwell GB10 in a DGX Spark).
    SharedWithSystem,
    /// The part of a shared pool the OS lets the GPU use (Metal's
    /// `recommendedMaxWorkingSetSize`), in bytes. Only the macOS probe
    /// reports it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    WorkingSet(u64),
    /// The GPU was found but its memory could not be read.
    Unknown,
}

impl HardwareProfile {
    fn from_findings(findings: Findings) -> Self {
        let ram_gb = bytes_to_gb(findings.total_ram_bytes);
        let (gpu_vendor, memory) = match findings.gpu {
            None => (None, Memory::System { ram_gb }),
            Some(GpuFinding { vendor, memory }) => {
                let memory = match memory {
                    // An adapter with no dedicated memory, which DXGI can
                    // report, holds no model: the model runs from RAM.
                    GpuMemory::Dedicated(0) => Memory::System { ram_gb },
                    GpuMemory::Dedicated(bytes) => Memory::Discrete {
                        vram_gb: bytes_to_gb(bytes),
                        ram_gb: Some(ram_gb),
                    },
                    GpuMemory::SharedWithSystem => Memory::Unified {
                        vram_gb: ram_gb,
                        ram_gb: Some(ram_gb),
                    },
                    GpuMemory::WorkingSet(bytes) => Memory::Unified {
                        vram_gb: bytes_to_gb(bytes),
                        ram_gb: Some(ram_gb),
                    },
                    GpuMemory::Unknown => Memory::System { ram_gb },
                };
                (Some(vendor), memory)
            }
        };
        let mut profile = Self::with_memory(memory);
        profile.gpu_vendor = gpu_vendor;
        profile.machine = MachineInfo {
            os: findings.os,
            cpu_cores: Some(findings.cpu_cores),
            cpu_arch: findings.cpu_arch,
        };
        profile
    }
}

fn probe_gpu() -> Option<GpuFinding> {
    if let Some(finding) = gpu::detect_nvidia() {
        return Some(finding);
    }

    #[cfg(target_os = "macos")]
    if let Some(finding) = gpu::detect_apple() {
        return Some(finding);
    }

    #[cfg(target_os = "linux")]
    if let Some(finding) = gpu::detect_amd_linux() {
        return Some(finding);
    }

    #[cfg(target_os = "windows")]
    if let Some(finding) = gpu::detect_dxgi() {
        return Some(finding);
    }

    None
}

fn total_memory_bytes() -> u64 {
    sys_info::mem_info()
        .map(|info| info.total * 1024)
        .unwrap_or(0)
}

fn bytes_to_gb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/*-- gpu detection --*/
///
/// Each vendor is probed with a mechanism that avoids build-time native
/// linking: NVML and (on Linux) sysfs are read at runtime, and the Metal /
/// DXGI paths only link OS frameworks that are always present on their
/// respective platforms. Probes report what they read; `from_findings`
/// decides what it means.
mod gpu {
    use super::{GpuFinding, GpuMemory};

    pub fn detect_nvidia() -> Option<GpuFinding> {
        use nvml_wrapper::error::NvmlError;

        let nvml = nvml_wrapper::Nvml::init().ok()?;
        let device = nvml.device_by_index(0).ok()?;
        let memory = match device.memory_info() {
            Ok(info) => GpuMemory::Dedicated(info.total),
            // Coherent-memory superchips (e.g. Grace Blackwell GB10) have no
            // discrete framebuffer -- NVML reports NotSupported because the GPU
            // shares system RAM over NVLink-C2C instead of owning dedicated VRAM.
            Err(NvmlError::NotSupported) => GpuMemory::SharedWithSystem,
            Err(_) => GpuMemory::Unknown,
        };
        Some(GpuFinding {
            vendor: "NVIDIA".to_string(),
            memory,
        })
    }

    #[cfg(target_os = "macos")]
    pub fn detect_apple() -> Option<GpuFinding> {
        // MTLCreateSystemDefaultDevice requires CoreGraphics to be linked.
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {}

        use objc2_metal::MTLDevice;

        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        Some(GpuFinding {
            vendor: "Apple".to_string(),
            memory: GpuMemory::WorkingSet(device.recommendedMaxWorkingSetSize()),
        })
    }

    #[cfg(target_os = "linux")]
    pub fn detect_amd_linux() -> Option<GpuFinding> {
        const AMD_VENDOR_ID: &str = "0x1002";

        let drm_dir = std::fs::read_dir("/sys/class/drm").ok()?;
        for entry in drm_dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("card")
                || !name["card".len()..].bytes().all(|b| b.is_ascii_digit())
            {
                continue;
            }

            let device_dir = entry.path().join("device");
            let vendor = std::fs::read_to_string(device_dir.join("vendor")).unwrap_or_default();
            if vendor.trim() != AMD_VENDOR_ID {
                continue;
            }

            if let Ok(raw) = std::fs::read_to_string(device_dir.join("mem_info_vram_total")) {
                if let Ok(bytes) = raw.trim().parse::<u64>() {
                    return Some(GpuFinding {
                        vendor: "AMD".to_string(),
                        memory: GpuMemory::Dedicated(bytes),
                    });
                }
            }
        }
        None
    }

    #[cfg(target_os = "windows")]
    pub fn detect_dxgi() -> Option<GpuFinding> {
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
        };

        // SAFETY: DXGI enumeration APIs are called per their documented contract;
        // failures are surfaced as `None` rather than unwound.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;

            let mut best: Option<(String, u64)> = None;
            let mut index = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                index += 1;
                let Ok(desc) = adapter.GetDesc1() else {
                    continue;
                };
                if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                    continue;
                }

                let vram = desc.DedicatedVideoMemory as u64;
                if best.as_ref().is_none_or(|(_, best_vram)| vram > *best_vram) {
                    let name_len = desc.Description.iter().take_while(|&&c| c != 0).count();
                    let name = String::from_utf16_lossy(&desc.Description[..name_len]);
                    let vendor = match desc.VendorId {
                        0x10DE => "NVIDIA".to_string(),
                        0x1002 | 0x1022 => "AMD".to_string(),
                        0x8086 => "Intel".to_string(),
                        _ => name,
                    };
                    best = Some((vendor, vram));
                }
            }

            best.map(|(vendor, vram)| GpuFinding {
                vendor,
                memory: GpuMemory::Dedicated(vram),
            })
        }
    }
}

/*-- tests --*/

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn findings(total_ram_gb: u64, gpu: Option<GpuFinding>) -> Findings {
        Findings {
            os: "test-os".to_string(),
            cpu_cores: 8,
            cpu_arch: "test-arch".to_string(),
            total_ram_bytes: total_ram_gb * GIB,
            gpu,
        }
    }

    fn gpu(vendor: &str, memory: GpuMemory) -> Option<GpuFinding> {
        Some(GpuFinding {
            vendor: vendor.to_string(),
            memory,
        })
    }

    fn discrete(vram_gb: f64) -> HardwareProfile {
        HardwareProfile::with_memory(Memory::Discrete {
            vram_gb,
            ram_gb: Some(128.0),
        })
    }

    // -- usable memory, one profile per memory kind ---------------------------

    #[test]
    fn discrete_profile_uses_vram_with_gpu_headroom() {
        assert_eq!(discrete(24.0).usable_memory_gb(), 16.0);
    }

    #[test]
    fn unified_profile_uses_addressable_pool_with_gpu_headroom() {
        let p = HardwareProfile::with_memory(Memory::Unified {
            vram_gb: 48.0,
            ram_gb: Some(64.0),
        });
        assert_eq!(p.usable_memory_gb(), 32.0);
    }

    #[test]
    fn system_profile_uses_ram_with_system_headroom() {
        let p = HardwareProfile::with_memory(Memory::System { ram_gb: 64.0 });
        assert_eq!(p.usable_memory_gb(), 32.0);
    }

    #[test]
    fn a_set_headroom_factor_replaces_the_default() {
        let mut p = discrete(24.0);
        p.headroom = Some(1.2);
        assert_eq!(p.headroom(), 1.2);
        assert_eq!(p.usable_memory_gb(), 20.0);

        let mut p = HardwareProfile::with_memory(Memory::System { ram_gb: 64.0 });
        p.headroom = Some(1.0);
        assert_eq!(p.usable_memory_gb(), 64.0);
    }

    // -- stored shape, one profile per memory kind -----------------------------

    #[test]
    fn each_memory_kind_round_trips_through_yaml() {
        let mut detected = HardwareProfile::with_memory(Memory::Unified {
            vram_gb: 51.84,
            ram_gb: Some(64.0),
        });
        detected.gpu_vendor = Some("Apple".to_string());
        detected.machine = MachineInfo {
            os: "macos".to_string(),
            cpu_cores: Some(14),
            cpu_arch: "aarch64".to_string(),
        };
        for profile in [
            detected,
            discrete(24.0),
            HardwareProfile::with_memory(Memory::Discrete {
                vram_gb: 24.0,
                ram_gb: None,
            }),
            HardwareProfile::with_memory(Memory::System { ram_gb: 64.0 }),
        ] {
            let yaml = serde_yaml::to_string(&profile).unwrap();
            let back: HardwareProfile = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(back, profile, "round trip of:\n{yaml}");
        }
    }

    #[test]
    fn stored_shape_names_the_kind_and_groups_the_machine_fields() {
        let mut p = HardwareProfile::with_memory(Memory::Unified {
            vram_gb: 51.84,
            ram_gb: Some(64.0),
        });
        p.gpu_vendor = Some("Apple".to_string());
        p.machine.os = "macos".to_string();
        let value = serde_json::to_value(&p).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "memory_kind": "unified",
                "vram_gb": 51.84,
                "ram_gb": 64.0,
                "gpu_vendor": "Apple",
                "machine": { "os": "macos" },
            })
        );
    }

    #[test]
    fn a_figure_the_kind_does_not_have_does_not_load() {
        for yaml in [
            "memory_kind: system\nram_gb: 64.0\nvram_gb: 24.0\n",
            "memory_kind: discrete\nvram_gb: 24.0\nmemory_gb: 24.0\n",
        ] {
            assert!(
                serde_yaml::from_str::<HardwareProfile>(yaml).is_err(),
                "loaded: {yaml}"
            );
        }
    }

    #[test]
    fn a_profile_without_the_figure_its_kind_names_does_not_load() {
        for yaml in [
            "memory_kind: discrete\nram_gb: 64.0\n",
            "memory_kind: unified\nram_gb: 64.0\n",
            "memory_kind: system\nvram_gb: 24.0\n",
        ] {
            assert!(
                serde_yaml::from_str::<HardwareProfile>(yaml).is_err(),
                "loaded: {yaml}"
            );
        }
    }

    // -- checking that the figures are positive ------------------------------

    #[test]
    fn check_figures_accepts_positive_figures() {
        assert!(discrete(24.0).check_figures().is_ok());
        let system = HardwareProfile::with_memory(Memory::System { ram_gb: 64.0 });
        assert!(system.check_figures().is_ok());
    }

    #[test]
    fn check_figures_rejects_a_figure_that_is_not_positive() {
        let zero = HardwareProfile::with_memory(Memory::System { ram_gb: 0.0 });
        assert_eq!(
            zero.check_figures().unwrap_err(),
            "ram_gb must be positive, got 0"
        );
        let negative = HardwareProfile::with_memory(Memory::Discrete {
            vram_gb: 24.0,
            ram_gb: Some(-1.0),
        });
        assert_eq!(
            negative.check_figures().unwrap_err(),
            "ram_gb must be positive, got -1"
        );
    }

    // -- mapping findings to a profile, one case per platform -----------------

    #[test]
    fn no_gpu_maps_to_system_memory() {
        let p = HardwareProfile::from_findings(findings(64, None));
        assert_eq!(p.memory, Memory::System { ram_gb: 64.0 });
        assert_eq!(p.gpu_vendor, None);
        assert_eq!(p.usable_memory_gb(), 32.0);
    }

    #[test]
    fn nvidia_card_maps_to_discrete_vram() {
        let p = HardwareProfile::from_findings(findings(
            128,
            gpu("NVIDIA", GpuMemory::Dedicated(24 * GIB)),
        ));
        assert_eq!(
            p.memory,
            Memory::Discrete {
                vram_gb: 24.0,
                ram_gb: Some(128.0)
            }
        );
        assert_eq!(p.gpu_vendor.as_deref(), Some("NVIDIA"));
        assert_eq!(p.usable_memory_gb(), 16.0);
    }

    #[test]
    fn dgx_spark_maps_to_unified_whole_pool() {
        // GB10: NVML has no framebuffer to report, the GPU addresses all of RAM.
        let p = HardwareProfile::from_findings(findings(
            128,
            gpu("NVIDIA", GpuMemory::SharedWithSystem),
        ));
        assert_eq!(
            p.memory,
            Memory::Unified {
                vram_gb: 128.0,
                ram_gb: Some(128.0)
            }
        );
        assert_eq!(p.usable_memory_gb(), 128.0 / 1.5);
    }

    #[test]
    fn nvidia_with_unreadable_memory_maps_to_system_memory() {
        let p = HardwareProfile::from_findings(findings(64, gpu("NVIDIA", GpuMemory::Unknown)));
        assert_eq!(p.memory, Memory::System { ram_gb: 64.0 });
        assert_eq!(p.gpu_vendor.as_deref(), Some("NVIDIA"));
        assert_eq!(p.usable_memory_gb(), 32.0);
    }

    #[test]
    fn apple_silicon_maps_to_unified_working_set() {
        // The working set is below total RAM: 48 of 64 GiB here.
        let p = HardwareProfile::from_findings(findings(
            64,
            gpu("Apple", GpuMemory::WorkingSet(48 * GIB)),
        ));
        assert_eq!(
            p.memory,
            Memory::Unified {
                vram_gb: 48.0,
                ram_gb: Some(64.0)
            }
        );
        assert_eq!(p.usable_memory_gb(), 32.0);
    }

    #[test]
    fn an_adapter_with_no_dedicated_memory_maps_to_system_memory() {
        let p = HardwareProfile::from_findings(findings(32, gpu("Intel", GpuMemory::Dedicated(0))));
        assert_eq!(p.memory, Memory::System { ram_gb: 32.0 });
        assert_eq!(p.gpu_vendor.as_deref(), Some("Intel"));
    }

    #[test]
    fn a_detected_64_gib_mac_matches_its_catalog_entry() {
        // The measured working set on a 64 GiB Mac, 51.84 GiB, as Metal
        // reports it in bytes.
        let working_set = (51.84 * GIB as f64) as u64;
        let detected = HardwareProfile::from_findings(findings(
            64,
            gpu("Apple", GpuMemory::WorkingSet(working_set)),
        ));
        let entry = crate::utils::hardware_catalog::find("apple-m4-max").unwrap();
        let catalog = entry.usable_memory_gb(entry.size(64.0).unwrap());
        assert!(
            (detected.usable_memory_gb() - catalog).abs() < 1e-6,
            "detected {} vs catalog {catalog}",
            detected.usable_memory_gb()
        );
    }

    #[test]
    fn amd_card_on_linux_maps_to_discrete_vram() {
        let p = HardwareProfile::from_findings(findings(
            64,
            gpu("AMD", GpuMemory::Dedicated(16 * GIB)),
        ));
        assert_eq!(p.memory_kind(), MemoryKind::Discrete);
        assert_eq!(p.gpu_vendor.as_deref(), Some("AMD"));
        assert_eq!(p.memory.vram_gb(), Some(16.0));
    }

    #[test]
    fn windows_dxgi_adapter_maps_to_discrete_dedicated_memory() {
        let p = HardwareProfile::from_findings(findings(
            32,
            gpu("Intel", GpuMemory::Dedicated(GIB / 2)),
        ));
        assert_eq!(p.memory_kind(), MemoryKind::Discrete);
        assert_eq!(p.memory.vram_gb(), Some(0.5));
    }

    #[test]
    fn findings_carry_descriptive_fields_through() {
        let p = HardwareProfile::from_findings(findings(64, None));
        assert_eq!(p.machine.os, "test-os");
        assert_eq!(p.machine.cpu_cores, Some(8));
        assert_eq!(p.machine.cpu_arch, "test-arch");
    }
}
