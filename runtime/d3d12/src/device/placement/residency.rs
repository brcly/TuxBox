//! Give a placed texture's desktop image memory only when GPU work first uses it.
//!
//! A title creates a view over most guest textures long before it draws with
//! them, and many it never draws with at all. A committed image for each holds
//! graphics memory from the moment the view exists (#292). A reserved image
//! holds none until its tiles are mapped, which happens on the upload queue
//! ahead of the first batch, copy or upload that touches it. Unmapped tiles are
//! never read: every GPU use of a placed texture passes through `submit` or a
//! CPU write flush, and both back the image first.
use super::{Texture, budget, lock};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

const TILE: u64 = D3D12_TILED_RESOURCE_TILE_SIZE_IN_BYTES as u64;

/// An image and the heap mapped under it. A mapping holds no reference to its
/// heap, so queued work that names a reserved image has to hold both.
pub(super) struct Held {
    _image: ID3D12Resource,
    _heap: Option<ID3D12Heap>,
}

pub(super) enum Residency {
    /// Created with its own memory.
    Committed,
    /// Reserved; `backing` is set once its tiles are mapped.
    Reserved { tiles: u32, backing: OnceLock<Backing> },
}

/// The heap under a reserved image, and whether it is in system memory.
pub(super) struct Backing {
    heap: ID3D12Heap,
    system: bool,
}

/// Where an image's memory is, with its size where the image was reserved.
#[derive(Debug, PartialEq)]
pub(super) enum Place {
    Committed,
    Unbacked(u64),
    Card(u64),
    System(u64),
}

/// `TUXBOX_LAZY_TEXTURES=0` creates every placed image committed, as before.
pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| parse(std::env::var("TUXBOX_LAZY_TEXTURES").ok().as_deref()))
}

pub(super) fn parse(value: Option<&str>) -> bool {
    !matches!(value.map(str::trim), Some("0" | "off" | "false" | "no"))
}

static CREATED: AtomicU64 = AtomicU64::new(0);
static CREATED_BYTES: AtomicU64 = AtomicU64::new(0);
static BACKED: AtomicU64 = AtomicU64::new(0);
static BACKED_BYTES: AtomicU64 = AtomicU64::new(0);

fn report(event: &str, count: u64) {
    if count <= 4 || count.is_power_of_two() {
        crate::diagnostics::log(&format!(
            "lazy placed textures after {event} #{count}: {} reserved ({} MiB), {} backed ({} MiB)",
            CREATED.load(Ordering::Relaxed),
            CREATED_BYTES.load(Ordering::Relaxed) >> 20,
            BACKED.load(Ordering::Relaxed),
            BACKED_BYTES.load(Ordering::Relaxed) >> 20,
        ));
    }
}

/// A reserved image for `host`, or `None` where the driver can't reserve one
/// (1D and multi-planar images, 3D below tiled tier 3, some formats). The
/// caller then creates it committed.
pub(super) fn reserve(
    device: &ID3D12Device,
    host: &D3D12_RESOURCE_DESC,
    clear: Option<&D3D12_CLEAR_VALUE>,
) -> Option<(ID3D12Resource, Residency)> {
    if !enabled() || host.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE1D {
        return None;
    }
    let desc = D3D12_RESOURCE_DESC {
        Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
        ..*host
    };
    let mut native: Option<ID3D12Resource> = None;
    // SAFETY: D3D12 only borrows the input structures during creation.
    let created = unsafe {
        device.CreateReservedResource(
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            clear.map(core::ptr::from_ref),
            &mut native,
        )
    };
    let native = match (created, native) {
        (Ok(()), Some(native)) => native,
        (result, _) => {
            static REFUSED: AtomicU64 = AtomicU64::new(0);
            let count = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 8 || count.is_power_of_two() {
                crate::diagnostics::log(&format!(
                    "placed texture created committed, reserved image refused (#{count}): {}x{}x{} format={} dimension={} mips={} samples={} flags={:#x}: {:?}",
                    host.Width, host.Height, host.DepthOrArraySize, host.Format.0, host.Dimension.0,
                    host.MipLevels, host.SampleDesc.Count, host.Flags.0, result.err().map(|e| e.code()),
                ));
            }
            return None;
        }
    };
    let mut tiles = 0u32;
    // SAFETY: only the total tile count is requested.
    unsafe { device.GetResourceTiling(&native, Some(&mut tiles), None, None, None, 0, core::ptr::null_mut()) };
    if tiles == 0 {
        return None;
    }
    CREATED_BYTES.fetch_add(u64::from(tiles) * TILE, Ordering::Relaxed);
    report("reservation", CREATED.fetch_add(1, Ordering::Relaxed) + 1);
    Some((native, Residency::Reserved { tiles, backing: OnceLock::new() }))
}

/// A heap of `bytes` for a reserved image, on the card or in system memory.
fn heap(device: &ID3D12Device, bytes: u64, system: bool) -> Result<ID3D12Heap> {
    let properties = if system {
        // vkd3d-proton puts a system-pool heap on the card all the same when
        // it has no CPU access, so this one asks for write-combined access.
        D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
        }
    } else {
        D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
            ..Default::default()
        }
    };
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: bytes,
        Properties: properties,
        Alignment: TILE,
        Flags: D3D12_HEAP_FLAG_NONE,
    };
    let mut created: Option<ID3D12Heap> = None;
    // SAFETY: D3D12 only borrows the description during creation.
    unsafe { device.CreateHeap(&desc, &mut created)? };
    created.ok_or_else(|| windows::Win32::Foundation::E_POINTER.into())
}

impl Residency {
    pub(super) fn place(&self) -> Place {
        match self {
            Self::Committed => Place::Committed,
            Self::Reserved { tiles, backing } => {
                let bytes = u64::from(*tiles) * TILE;
                match backing.get() {
                    None => Place::Unbacked(bytes),
                    Some(Backing { system: false, .. }) => Place::Card(bytes),
                    Some(Backing { system: true, .. }) => Place::System(bytes),
                }
            }
        }
    }

    /// Map a reserved image's tiles onto a new heap through `queue`, in system
    /// memory if `room` has no space for it. Returns whether it mapped anything.
    pub(super) fn map(
        &self,
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        native: &ID3D12Resource,
        flags: D3D12_RESOURCE_FLAGS,
        room: &mut budget::Room,
    ) -> Result<bool> {
        let Self::Reserved { tiles, backing } = self else {
            return Ok(false);
        };
        if backing.get().is_some() {
            return Ok(false);
        }
        let bytes = u64::from(*tiles) * TILE;
        let mut system = room.spill(bytes, flags);
        let created = match system.then(|| heap(device, bytes, true)) {
            Some(Ok(created)) => created,
            // A driver that refuses the system heap keeps the image on the card.
            _ => {
                system = false;
                heap(device, bytes, false)?
            }
        };
        let start = D3D12_TILED_RESOURCE_COORDINATE::default();
        let size = D3D12_TILE_REGION_SIZE { NumTiles: *tiles, UseBox: false.into(), ..Default::default() };
        let range = D3D12_TILE_RANGE_FLAG_NONE;
        let offset = 0u32;
        // SAFETY: one region and one range, each of `tiles` tiles, covering the
        // whole image (packed mips included) from heap offset zero.
        unsafe {
            queue.UpdateTileMappings(
                native, 1, Some(&start), Some(&size), &created, 1,
                Some(&range), Some(&offset), Some(tiles), D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
        // Only the thread holding the transfer lock maps, so this cannot race.
        let _ = backing.set(Backing { heap: created, system });
        if system {
            budget::spilled(bytes);
        }
        BACKED_BYTES.fetch_add(bytes, Ordering::Relaxed);
        report("first use", BACKED.fetch_add(1, Ordering::Relaxed) + 1);
        Ok(true)
    }
}

impl Texture {
    /// What queued work must keep until the GPU has finished with this image.
    pub(super) fn hold(&self) -> Held {
        let heap = match &self.residency {
            Residency::Committed => None,
            Residency::Reserved { backing, .. } => backing.get().map(|backing| backing.heap.clone()),
        };
        Held { _image: self.native.clone(), _heap: heap }
    }

    fn unbacked(&self) -> bool {
        matches!(self.residency.place(), Place::Unbacked(_))
    }
}

impl super::upload::Transfer {
    /// Back every reserved image in `textures` before work that uses them.
    /// Later uploads and the guest queue's wait for them are ordered after it.
    pub(super) fn back(&mut self, device: &ID3D12Device, textures: &[std::sync::Arc<Texture>]) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error.into());
        }
        let mut mapped = Vec::new();
        mapped.try_reserve(textures.len()).map_err(|_| windows::Win32::Foundation::E_OUTOFMEMORY)?;
        // The adapter is asked only when something needs memory.
        let mut room = if textures.iter().any(|texture| texture.unbacked()) {
            self.meter.get_or_insert_with(|| budget::Meter::new(device)).room()
        } else {
            budget::Room(None)
        };
        // A failure still has to cover the mappings queued before it.
        let result = textures.iter().try_for_each(|texture| {
            let flags = texture.desc.Flags;
            if texture.residency.map(device, &self.queue, &texture.native, flags, &mut room)? {
                mapped.push(texture);
            }
            Ok(())
        });
        if mapped.is_empty() {
            return result;
        }
        let tick = self.tick()?;
        // The title can release an image before the queue has mapped it.
        self.keep(tick, super::upload::Pending::Mapping(mapped.iter().map(|texture| texture.hold()).collect()))?;
        if let Err(error) = unsafe { self.queue.Signal(&self.fence, tick) } {
            self.failed = Some(error.code());
            return Err(error);
        }
        for texture in mapped {
            let mut contents = lock(&texture.contents);
            contents.uploaded = contents.uploaded.max(tick);
        }
        result
    }
}

#[cfg(test)]
#[path = "../../tests/device/placement/residency.rs"]
mod tests;
