//! Keep placed textures from filling the card, and log where the memory goes.
//!
//! A console title has one pool of memory that it sizes itself against. The
//! card here holds what the title put there and everything the host adds, and
//! when it is full the driver faults. A sampled image first used when the card
//! is nearly full gets system memory instead: slower to sample, but the
//! adapter keeps room for the images the GPU writes and for the driver.
use super::{Alias, Images, lock, residency::Place, upload};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{s, w};

/// Card memory left free for everything that is not a sampled placed image.
const RESERVE: u64 = 1536 << 20;
const LEDGER_INTERVAL: Duration = Duration::from_secs(5);

/// `TUXBOX_VRAM_RESERVE_MIB`: how much of the card's budget sampled images
/// leave free. `off` keeps every image on the card.
pub(super) fn parse_reserve(value: Option<&str>) -> Option<u64> {
    match value.map(str::trim) {
        Some("off") => None,
        value => Some(
            value
                .and_then(|text| text.parse::<u64>().ok())
                .and_then(|mib| mib.checked_mul(1 << 20))
                .unwrap_or(RESERVE),
        ),
    }
}

fn reserve() -> Option<u64> {
    static LIMIT: OnceLock<Option<u64>> = OnceLock::new();
    *LIMIT.get_or_init(|| parse_reserve(std::env::var("TUXBOX_VRAM_RESERVE_MIB").ok().as_deref()))
}

/// The card memory still open to sampled images. `None` where it isn't limited
/// or the adapter can't say.
pub(super) struct Room(pub(super) Option<u64>);

impl Room {
    /// Count an image of `bytes` against the room and say whether it has to go
    /// to system memory. Images the GPU writes stay on the card.
    pub(super) fn spill(&mut self, bytes: u64, flags: D3D12_RESOURCE_FLAGS) -> bool {
        let Some(free) = self.0.as_mut() else {
            return false;
        };
        let written = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
            | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL
            | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        if bytes > *free && (flags & written).0 == 0 {
            return true;
        }
        *free = free.saturating_sub(bytes);
        false
    }
}

/// One device's view of its adapter's memory.
pub(super) struct Meter(Option<IDXGIAdapter3>);

impl Meter {
    pub(super) fn new(device: &ID3D12Device) -> Self {
        // SAFETY: the LUID names the adapter behind this live device.
        let adapter = unsafe {
            CreateDXGIFactory1::<IDXGIFactory4>()
                .and_then(|factory| factory.EnumAdapterByLuid(device.GetAdapterLuid()))
        };
        Self(adapter.ok())
    }

    pub(super) fn segment(&self, group: DXGI_MEMORY_SEGMENT_GROUP) -> Option<DXGI_QUERY_VIDEO_MEMORY_INFO> {
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: the output is a live local.
        unsafe { self.0.as_ref()?.QueryVideoMemoryInfo(0, group, &mut info) }.ok()?;
        (info.Budget != 0).then_some(info)
    }

    pub(super) fn room(&self) -> Room {
        let free = reserve().and_then(|reserve| {
            let card = self.segment(DXGI_MEMORY_SEGMENT_GROUP_LOCAL)?;
            Some(card.Budget.saturating_sub(card.CurrentUsage).saturating_sub(reserve))
        });
        Room(free)
    }
}

/// Count images sent to system memory, and say so now and then.
pub(super) fn spilled(bytes: u64) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);
    let total = BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 4 || count.is_power_of_two() {
        crate::diagnostics::log(&format!(
            "video memory is nearly full: placed texture #{count} put in system memory ({} MiB so far)",
            total >> 20
        ));
    }
}

/// The bytes covered by any of `ranges`.
pub(super) fn covered(ranges: &mut [Range<u64>]) -> u64 {
    ranges.sort_unstable_by_key(|range| range.start);
    let (mut total, mut end) = (0, 0);
    for range in ranges.iter() {
        total += range.end.saturating_sub(range.start.max(end));
        end = end.max(range.end);
    }
    total
}

#[derive(Default)]
struct Tally {
    count: u64,
    bytes: u64,
}

impl Tally {
    fn add(&mut self, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
    }

    fn text(&self) -> String {
        format!("{} ({} MiB)", self.count, self.bytes >> 20)
    }
}

fn textures(images: &BTreeMap<u64, Vec<Alias>>) -> String {
    let [mut unbacked, mut card, mut system, mut committed]: [Tally; 4] = Default::default();
    let mut guest = Vec::new();
    for texture in images.values().flatten().filter_map(|alias| alias.texture.upgrade()) {
        match texture.residency.place() {
            Place::Unbacked(bytes) => unbacked.add(bytes),
            Place::Card(bytes) => {
                card.add(bytes);
                guest.push(texture.range());
            }
            Place::System(bytes) => system.add(bytes),
            Place::Committed => committed.add(texture.size),
        }
    }
    let spans: u64 = guest.iter().map(|range| range.end - range.start).sum();
    let distinct = covered(&mut guest);
    format!(
        "unbacked {}, card {} (title ranges {} MiB, {} MiB distinct), system {}, committed {}",
        unbacked.text(), card.text(), spans >> 20, distinct >> 20, system.text(), committed.text()
    )
}

/// XMem's count of the physical pages the title holds.
fn title_pages() -> Option<u64> {
    // SAFETY: the name and signature match XMem's export.
    unsafe {
        let module = GetModuleHandleW(w!("xmem.dll")).ok()?;
        let symbol = GetProcAddress(module, s!("TuxBoxPhysicalPageBytes"))?;
        let bytes: unsafe extern "system" fn() -> u64 = core::mem::transmute(symbol);
        Some(bytes())
    }
}

fn committed_bytes() -> Option<u64> {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: the extended record starts with the base record and `size` selects it.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast(), size) }
        .as_bool()
        .then(|| counters.PrivateUsage.max(counters.PagefileUsage) as u64)
}

/// The periodic memory lines, written only when the provider logs. Wine cuts
/// a debug line near 300 characters, so each stays short.
#[derive(Default)]
pub(super) struct Ledger(Mutex<Option<(Instant, Arc<Meter>)>>);

impl Ledger {
    pub(super) fn report(&self, device: &ID3D12Device, images: &Images) {
        static LOGGING: OnceLock<bool> = OnceLock::new();
        if !*LOGGING.get_or_init(|| std::env::var_os("TUXBOX_LOG").is_some()) {
            return;
        }
        let meter = {
            // Another submission holding this is already reporting.
            let Ok(mut last) = self.0.try_lock() else { return };
            match last.as_mut() {
                Some((at, _)) if at.elapsed() < LEDGER_INTERVAL => return,
                Some((at, meter)) => {
                    *at = Instant::now();
                    meter.clone()
                }
                None => last.insert((Instant::now(), Arc::new(Meter::new(device)))).1.clone(),
            }
        };
        let segment = |group| {
            meter.segment(group).map_or_else(
                || "unknown".into(),
                |info| format!("{}/{} MiB", info.CurrentUsage >> 20, info.Budget >> 20),
            )
        };
        let mib = |bytes: Option<u64>| bytes.map_or_else(|| "unknown".into(), |b| format!("{} MiB", b >> 20));
        let textures = textures(&lock(&images.0));
        crate::diagnostics::log(&format!(
            "memory ledger: card {}, adapter system memory {}, upload staging {} MiB, title pages {}, process commit {}",
            segment(DXGI_MEMORY_SEGMENT_GROUP_LOCAL),
            segment(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL),
            upload::staging_in_flight() >> 20,
            mib(title_pages()),
            mib(committed_bytes()),
        ));
        crate::diagnostics::log(&format!("texture ledger: {textures}"));
    }
}

#[cfg(test)]
#[path = "../../tests/device/placement/budget.rs"]
mod tests;
