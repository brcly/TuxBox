//! What the host graphics driver can do, found once per device and logged.
//!
//! Lowerings and fallbacks choose from these capabilities, not from a vendor
//! name: the same card behaves differently under another driver or system.
//! `TUXBOX_HOST_PROFILE` replaces what the driver reports, so one machine can
//! exercise a path meant for another, for example `wave=32` or
//! `wave=32-64;section-import=substituted;mesh=none`, or `cpu-view-heap=262144`
//! to refuse larger CPU-only view heaps as a driver may.
mod section_probe;

use super::Device;
use std::sync::OnceLock;
use windows::Win32::Graphics::Direct3D12::{
    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER, D3D12_FEATURE,
    D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_D3D12_OPTIONS5, D3D12_FEATURE_D3D12_OPTIONS7,
    D3D12_FEATURE_DATA_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS5,
    D3D12_FEATURE_DATA_D3D12_OPTIONS7, D3D12_MESH_SHADER_TIER_1, D3D12_RAYTRACING_TIER_1_0,
};

const VARIABLE: &str = "TUXBOX_HOST_PROFILE";

/// Whether the GPU reads the guest's own pages after importing them. Xbox memory
/// is made of section views, which some drivers cannot import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SectionImport {
    /// The GPU read what the CPU wrote.
    Aliased,
    /// The import succeeded but the GPU read other memory.
    Substituted,
    /// The driver refused the import.
    Refused,
    /// The probe could not run, with the host's error code.
    Unknown(i32),
}

impl SectionImport {
    fn name(self) -> String {
        match self {
            Self::Aliased => "aliased".into(),
            Self::Substituted => "substituted".into(),
            Self::Refused => "refused".into(),
            Self::Unknown(code) => format!("unknown({code:#010x})"),
        }
    }
}

/// Capabilities given in `TUXBOX_HOST_PROFILE`. Mesh shaders and ray tracing
/// can only be taken away: claiming one the driver lacks would hand it work it
/// cannot run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Overrides {
    pub(crate) wave: Option<(u32, u32)>,
    pub(crate) section_import: Option<SectionImport>,
    pub(crate) mesh: Option<bool>,
    pub(crate) rays: Option<bool>,
    /// The most descriptors one CPU-only view heap may hold. The driver does
    /// not report its own limit, so a larger heap is refused for memory as the
    /// driver would refuse it.
    pub(crate) cpu_view_heap: Option<u32>,
}

fn lane_count(text: &str) -> Option<u32> {
    let count: u32 = text.trim().parse().ok()?;
    (count.is_power_of_two() && (4..=128).contains(&count)).then_some(count)
}

/// Parse `key=value` settings separated by `;`. One bad setting rejects them
/// all, so a typing mistake cannot silently test the wrong path.
pub(crate) fn parse_overrides(text: &str) -> Result<Overrides, String> {
    let mut overrides = Overrides::default();
    for setting in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let bad = || format!("unusable setting {setting:?}");
        let (key, value) = setting.split_once('=').ok_or_else(bad)?;
        match (key.trim(), value.trim()) {
            ("wave", value) => {
                let (low, high) = value.split_once('-').unwrap_or((value, value));
                let range = (lane_count(low).ok_or_else(bad)?, lane_count(high).ok_or_else(bad)?);
                if range.0 > range.1 {
                    return Err(bad());
                }
                overrides.wave = Some(range);
            }
            ("section-import", "aliased") => overrides.section_import = Some(SectionImport::Aliased),
            ("section-import", "substituted") => overrides.section_import = Some(SectionImport::Substituted),
            ("section-import", "refused") => overrides.section_import = Some(SectionImport::Refused),
            ("mesh", "none") => overrides.mesh = Some(false),
            ("rays", "none") => overrides.rays = Some(false),
            ("cpu-view-heap", value) => {
                let most = value.parse::<u32>().ok().filter(|&most| most > 0).ok_or_else(bad)?;
                overrides.cpu_view_heap = Some(most);
            }
            _ => return Err(bad()),
        }
    }
    Ok(overrides)
}

fn overrides() -> Overrides {
    static PARSED: OnceLock<Overrides> = OnceLock::new();
    *PARSED.get_or_init(|| {
        let Ok(text) = std::env::var(VARIABLE) else { return Overrides::default() };
        parse_overrides(&text).unwrap_or_else(|reason| {
            crate::diagnostics::log(&format!("{VARIABLE} ignored: {reason}"));
            Overrides::default()
        })
    })
}

/// The most descriptors one CPU-only view heap may hold when a profile sets it.
pub(super) fn cpu_view_heap() -> Option<u32> {
    overrides().cpu_view_heap
}

/// Per-device answers, each found when first needed.
#[derive(Default)]
pub(crate) struct Probes {
    wave: OnceLock<Option<(u32, u32)>>,
    section: OnceLock<SectionImport>,
    mesh: OnceLock<bool>,
    rays: OnceLock<bool>,
    reported: OnceLock<()>,
}

/// The wave lane range `(min, max)` to choose shader paths by.
pub(super) fn wave_range(device: &Device) -> Option<(u32, u32)> {
    *device.state.host.wave.get_or_init(|| overrides().wave.or_else(|| host_wave_range(device)))
}

fn host_wave_range(device: &Device) -> Option<(u32, u32)> {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    let ok = unsafe {
        device.native.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS1,
            (&raw mut options).cast(),
            size_of_val(&options) as u32,
        )
    }
    .is_ok();
    (ok && options.WaveOps.as_bool()
        && options.WaveLaneCountMin != 0
        && options.WaveLaneCountMax >= options.WaveLaneCountMin)
        .then_some((options.WaveLaneCountMin, options.WaveLaneCountMax))
}

/// The host's answer to a feature query, or `None` when it has no answer.
fn host_feature<T: Default>(device: &Device, feature: D3D12_FEATURE) -> Option<T> {
    let mut record = T::default();
    // SAFETY: the record is the desktop structure for this feature, at its exact size.
    let ok = unsafe {
        device.native.CheckFeatureSupport(feature, (&raw mut record).cast(), size_of::<T>() as u32)
    }
    .is_ok();
    ok.then_some(record)
}

/// Whether the host runs mesh shaders, and with them mesh dispatch records.
/// Older desktop runtimes that do not know the query have none.
pub(super) fn mesh_shaders(device: &Device) -> bool {
    *device.state.host.mesh.get_or_init(|| {
        overrides().mesh.unwrap_or_else(|| {
            host_feature::<D3D12_FEATURE_DATA_D3D12_OPTIONS7>(device, D3D12_FEATURE_D3D12_OPTIONS7)
                .is_some_and(|host| host.MeshShaderTier.0 >= D3D12_MESH_SHADER_TIER_1.0)
        })
    })
}

/// Whether the host traces rays, and with that runs ray dispatch records.
pub(super) fn ray_tracing(device: &Device) -> bool {
    *device.state.host.rays.get_or_init(|| {
        overrides().rays.unwrap_or_else(|| {
            host_feature::<D3D12_FEATURE_DATA_D3D12_OPTIONS5>(device, D3D12_FEATURE_D3D12_OPTIONS5)
                .is_some_and(|host| host.RaytracingTier.0 >= D3D12_RAYTRACING_TIER_1_0.0)
        })
    })
}

/// The PCI vendor and device identifiers of `adapter`, by which the operating
/// system lists its sensors.
pub(crate) fn pci_ids(adapter: &windows::Win32::Graphics::Dxgi::IDXGIAdapter1) -> Option<(u32, u32)> {
    // SAFETY: the description is returned by value from the live adapter.
    let description = unsafe { adapter.GetDesc1() }.ok()?;
    Some((description.VendorId, description.DeviceId))
}

/// Whether the GPU reads imported game memory. The first caller waits for the
/// probe, which runs one small GPU copy.
pub(super) fn section_import(device: &Device) -> SectionImport {
    *device.state.host.section.get_or_init(|| {
        overrides().section_import.unwrap_or_else(|| section_probe::run(&device.native))
    })
}

/// Whether imports of game memory need a copy the GPU can read. A probe that
/// could not run keeps direct imports.
pub(crate) fn needs_mirror(section: SectionImport) -> bool {
    matches!(section, SectionImport::Substituted | SectionImport::Refused)
}

/// The one line that describes the host, with overridden values marked.
pub(crate) fn describe(
    adapter: Option<(u32, u32)>,
    wave: Option<(u32, u32)>,
    section: SectionImport,
    descriptors: (u32, u32),
    shaders: (bool, bool),
    overrides: Overrides,
) -> String {
    let adapter = adapter.map_or("unknown".into(), |(vendor, device)| format!("{vendor:04x}:{device:04x}"));
    let mark = |set: bool| if set { " (set)" } else { "" };
    let wave_text = wave.map_or("none".into(), |(low, high)| format!("{low}-{high}"));
    let has = |present: bool| if present { "yes" } else { "none" };
    format!(
        "host profile: adapter={adapter} wave={wave_text}{} section-import={}{} descriptor-bytes={}/{} mesh={}{} rays={}{}",
        mark(overrides.wave.is_some()),
        section.name(),
        mark(overrides.section_import.is_some()),
        descriptors.0,
        descriptors.1,
        has(shaders.0),
        mark(overrides.mesh.is_some()),
        has(shaders.1),
        mark(overrides.rays.is_some()),
    )
}

/// Log the profile once per device.
pub(super) fn report(device: &Device) {
    device.state.host.reported.get_or_init(|| {
        if !crate::diagnostics::logging() {
            return;
        }
        let overrides = overrides();
        let section = section_import(device);
        let adapter = super::dxgi::adapter_ids(device);
        // SAFETY: both are valid descriptor heap types on a live device.
        let descriptors = unsafe {
            (
                device.native.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV),
                device.native.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER),
            )
        };
        let shaders = (mesh_shaders(device), ray_tracing(device));
        crate::diagnostics::log(&describe(adapter, wave_range(device), section, descriptors, shaders, overrides));
    });
}

#[cfg(test)]
#[path = "../tests/device/host_profile.rs"]
mod tests;
