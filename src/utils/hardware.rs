use serde::{Deserialize, Serialize};

/*-- public --*/

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HardwareProfile {
    pub(crate) os: String,
    pub(crate) cpu_cores: u32,
    pub(crate) cpu_arch: String,
    pub(crate) gpu_vendor: Option<String>,
    pub(crate) vram_gb: Option<f64>,
    pub(crate) ram_gb: f64,
}

impl HardwareProfile {
    fn detect() -> Self {
        let (gpu_vendor, vram_gb) = Self::detect_gpu();
        Self {
            os: std::env::consts::OS.to_string(),
            cpu_cores: num_cpus::get() as u32,
            cpu_arch: std::env::consts::ARCH.to_string(),
            gpu_vendor,
            vram_gb,
            ram_gb: total_memory_bytes() as f64 / (1024.0 * 1024.0 * 1024.0),
        }
    }

    fn detect_gpu() -> (Option<String>, Option<f64>) {
        if let Some(result) = gpu::detect_nvidia() {
            return result;
        }

        #[cfg(target_os = "macos")]
        if let Some(result) = gpu::detect_apple() {
            return result;
        }

        #[cfg(target_os = "linux")]
        if let Some(result) = gpu::detect_amd_linux() {
            return result;
        }

        #[cfg(target_os = "windows")]
        if let Some(result) = gpu::detect_dxgi() {
            return result;
        }

        (None, None)
    }

    /// Memory budget available for model weights + KV cache, after backing
    /// off the safety headroom (1.5x for VRAM, 2.0x for system RAM).
    pub(crate) fn usable_memory_gb(&self) -> f64 {
        match self.vram_gb {
            Some(vram) => vram / 1.5,
            None => self.ram_gb / 2.0,
        }
    }
}

pub(crate) fn detect_hardware() -> HardwareProfile {
    HardwareProfile::detect()
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
/// respective platforms.
mod gpu {
    use super::bytes_to_gb;

    pub(crate) fn detect_nvidia() -> Option<(Option<String>, Option<f64>)> {
        use nvml_wrapper::error::NvmlError;

        let nvml = nvml_wrapper::Nvml::init().ok()?;
        let device = nvml.device_by_index(0).ok()?;
        let vram_gb = match device.memory_info() {
            Ok(info) => Some(bytes_to_gb(info.total)),
            // Coherent-memory superchips (e.g. Grace Blackwell GB10) have no
            // discrete framebuffer -- NVML reports NotSupported because the GPU
            // shares system RAM over NVLink-C2C instead of owning dedicated VRAM.
            Err(NvmlError::NotSupported) => Some(bytes_to_gb(super::total_memory_bytes())),
            Err(_) => None,
        };
        Some((Some("NVIDIA".to_string()), vram_gb))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn detect_apple() -> Option<(Option<String>, Option<f64>)> {
        // MTLCreateSystemDefaultDevice requires CoreGraphics to be linked.
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {}

        use objc2_metal::MTLDevice;

        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        let vram_gb = bytes_to_gb(device.recommendedMaxWorkingSetSize());
        Some((Some("Apple".to_string()), Some(vram_gb)))
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn detect_amd_linux() -> Option<(Option<String>, Option<f64>)> {
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

            if let Ok(raw) = std::fs::read_to_string(device_dir.join("mem_info_vram_total"))
                && let Ok(bytes) = raw.trim().parse::<u64>()
            {
                return Some((Some("AMD".to_string()), Some(bytes_to_gb(bytes))));
            }
        }
        None
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn detect_dxgi() -> Option<(Option<String>, Option<f64>)> {
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

            best.map(|(vendor, vram)| (Some(vendor), Some(bytes_to_gb(vram))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hardware_profile_detects() {
        let profile = HardwareProfile::detect();
        assert!(profile.cpu_cores > 0);
        assert!(!profile.cpu_arch.is_empty());
        assert!(profile.ram_gb > 0.0);
    }
}
