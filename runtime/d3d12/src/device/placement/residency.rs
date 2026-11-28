//! Give a placed texture's desktop image memory only when GPU work first uses it.
//!
//! A title creates a view over most guest textures long before it draws with
//! them, and many it never draws with at all. A committed image for each holds
//! graphics memory from the moment the view exists (#292). A reserved image
//! holds none until its tiles are mapped, which happens on the upload queue
//! ahead of the first batch, copy or upload that touches it: every GPU use of a
//! placed texture passes through `submit` or a CPU write flush, and both back
//! the image first. Only the mips the title holds are mapped (see `mips`); a
//! read of another returns zero.
use super::{Images, Texture, budget, lock, mips};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

const TILE: u64 = D3D12_TILED_RESOURCE_TILE_SIZE_IN_BYTES as u64;

/// An image and the heap mapped under it. A mapping holds no reference to its
/// heap, so queued work that names a reserved image has to hold both.
pub(super) struct Held {
    _image: ID3D12Resource,
    _heaps: Vec<ID3D12Heap>,
}

pub(super) enum Residency {
    /// Created with its own memory.
    Committed,
    /// Reserved; `backing` is set once tiles are mapped.
    Reserved {
        tiles: u32,
        backing: Mutex<Option<Backing>>,
        /// Set once GPU work writes the image, which then needs every mip.
        whole: AtomicBool,
    },
}

/// The heaps under a reserved image: one, and one more each time larger mips
/// are mapped later.
pub(super) struct Backing {
    heaps: Vec<ID3D12Heap>,
    /// The first mip with memory.
    first: u32,
    /// Tiles with memory.
    tiles: u32,
    system: bool,
}

/// One run of tiles to map: a standard mip as a box, or the packed tail.
#[derive(Debug, PartialEq)]
pub(super) struct Region {
    pub(super) subresource: u32,
    pub(super) tiles: u32,
    pub(super) shape: Option<(u32, u16, u16)>,
}

/// The tiles that take an image from mips `mapped` onwards to mips `first`
/// onwards. `standard` holds each unpacked mip's size in tiles and `tail` the
/// packed mips' tile count, which are mapped with the first request.
pub(super) fn regions(standard: &[(u32, u16, u16)], tail: u32, first: u32, mapped: Option<u32>) -> Vec<Region> {
    let count = standard.len() as u32;
    let end = mapped.map_or(count, |mapped| mapped.min(count));
    let mut regions: Vec<_> = (first.min(count)..end)
        .map(|subresource| {
            let (width, height, depth) = standard[subresource as usize];
            Region {
                subresource,
                tiles: width * u32::from(height) * u32::from(depth),
                shape: Some((width, height, depth)),
            }
        })
        .collect();
    if mapped.is_none() && tail != 0 {
        regions.push(Region { subresource: count, tiles: tail, shape: None });
    }
    regions
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
    Some((native, Residency::Reserved { tiles, backing: Mutex::default(), whole: AtomicBool::new(false) }))
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

/// The tile shapes of an image's unpacked mips and the packed tail's tiles.
fn tiling(device: &ID3D12Device, native: &ID3D12Resource) -> (Vec<(u32, u16, u16)>, u32) {
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    // SAFETY: the description is a COM result of a live resource.
    let mut count = u32::from(unsafe { native.GetDesc() }.MipLevels);
    let mut mips = vec![D3D12_SUBRESOURCE_TILING::default(); count as usize];
    // SAFETY: `mips` holds the `count` entries asked for.
    unsafe {
        device.GetResourceTiling(native, None, Some(&mut packed), None, Some(&mut count), 0, mips.as_mut_ptr());
    }
    mips.truncate(count.min(u32::from(packed.NumStandardMips)) as usize);
    let standard = mips.iter().map(|mip| (mip.WidthInTiles, mip.HeightInTiles, mip.DepthInTiles)).collect();
    (standard, packed.NumTilesForPackedMips)
}

impl Residency {
    pub(super) fn place(&self) -> Place {
        match self {
            Self::Committed => Place::Committed,
            Self::Reserved { tiles, backing, .. } => {
                match lock(backing).as_ref() {
                    None => Place::Unbacked(u64::from(*tiles) * TILE),
                    Some(backing) => {
                        let bytes = u64::from(backing.tiles) * TILE;
                        if backing.system { Place::System(bytes) } else { Place::Card(bytes) }
                    }
                }
            }
        }
    }

    /// The first mip with memory; 0 for an image not yet backed.
    pub(super) fn first(&self) -> u32 {
        match self {
            Self::Committed => 0,
            Self::Reserved { backing, .. } => lock(backing).as_ref().map_or(0, |backing| backing.first),
        }
    }

    /// Reserved bytes left without memory in a backed image.
    pub(super) fn left_out(&self) -> u64 {
        match self {
            Self::Committed => 0,
            Self::Reserved { tiles, backing, .. } => lock(backing)
                .as_ref()
                .map_or(0, |backing| u64::from(tiles - backing.tiles) * TILE),
        }
    }

    /// Ask for every mip from the next mapping on.
    pub(super) fn need_whole(&self) {
        if let Self::Reserved { whole, .. } = self {
            whole.store(true, Ordering::Relaxed);
        }
    }

    /// Whether mapping from mip `first` would add memory.
    fn can_grow(&self, first: u32) -> bool {
        match self {
            Self::Committed => false,
            Self::Reserved { backing, whole, .. } => lock(backing).as_ref().is_none_or(|backing| {
                backing.first != 0 && (first < backing.first || whole.load(Ordering::Relaxed))
            }),
        }
    }

    /// Whether a mapping could still give this image memory.
    fn incomplete(&self) -> bool {
        match self {
            Self::Committed => false,
            Self::Reserved { backing, .. } => lock(backing).as_ref().is_none_or(|backing| backing.first != 0),
        }
    }

    /// Give a reserved image's mips from `first` onwards memory through
    /// `queue`, in system memory if `room` has no space for them. Returns
    /// whether it mapped anything.
    pub(super) fn map(
        &self,
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        native: &ID3D12Resource,
        flags: D3D12_RESOURCE_FLAGS,
        first: u32,
        room: &mut budget::Room,
    ) -> Result<bool> {
        let Self::Reserved { tiles: total, backing, whole } = self else {
            return Ok(false);
        };
        let first = if whole.load(Ordering::Relaxed) { 0 } else { first };
        let mut backing = lock(backing);
        let mapped = backing.as_ref().map(|backing| backing.first);
        if mapped.is_some_and(|mapped| first >= mapped) {
            return Ok(false);
        }
        // A whole image takes one region, packed mips included.
        let (list, first) = if first == 0 && mapped.is_none() {
            (vec![Region { subresource: 0, tiles: *total, shape: None }], 0)
        } else {
            let (standard, tail) = tiling(device, native);
            let first = first.min(standard.len() as u32);
            (regions(&standard, tail, first, mapped), first)
        };
        let tiles: u32 = list.iter().map(|region| region.tiles).sum();
        if tiles == 0 {
            if let Some(backing) = backing.as_mut() {
                backing.first = first;
            }
            return Ok(false);
        }
        let bytes = u64::from(tiles) * TILE;
        let mut system = room.spill(bytes, flags);
        let created = match system.then(|| heap(device, bytes, true)) {
            Some(Ok(created)) => created,
            // A driver that refuses the system heap keeps the image on the card.
            _ => {
                system = false;
                heap(device, bytes, false)?
            }
        };
        let starts: Vec<_> = list
            .iter()
            .map(|region| D3D12_TILED_RESOURCE_COORDINATE { Subresource: region.subresource, ..Default::default() })
            .collect();
        let sizes: Vec<_> = list
            .iter()
            .map(|region| {
                let (width, height, depth) = region.shape.unwrap_or_default();
                D3D12_TILE_REGION_SIZE {
                    NumTiles: region.tiles,
                    UseBox: region.shape.is_some().into(),
                    Width: width,
                    Height: height,
                    Depth: depth,
                }
            })
            .collect();
        let range = D3D12_TILE_RANGE_FLAG_NONE;
        let offset = 0u32;
        // SAFETY: `regions` matching coordinates and sizes, and one range of
        // all their tiles from heap offset zero.
        unsafe {
            queue.UpdateTileMappings(
                native, list.len() as u32, Some(starts.as_ptr()), Some(sizes.as_ptr()), &created, 1,
                Some(&range), Some(&offset), Some(&tiles), D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
        match backing.as_mut() {
            Some(backing) => {
                backing.heaps.push(created);
                backing.first = first;
                backing.tiles += tiles;
                backing.system |= system;
            }
            None => {
                *backing = Some(Backing { heaps: vec![created], first, tiles, system });
                report("first use", BACKED.fetch_add(1, Ordering::Relaxed) + 1);
            }
        }
        if system {
            budget::spilled(bytes);
        }
        BACKED_BYTES.fetch_add(bytes, Ordering::Relaxed);
        Ok(true)
    }
}

impl Texture {
    /// What queued work must keep until the GPU has finished with this image.
    pub(super) fn hold(&self) -> Held {
        let heaps = match &self.residency {
            Residency::Committed => Vec::new(),
            Residency::Reserved { backing, .. } => {
                lock(backing).as_ref().map_or_else(Vec::new, |backing| backing.heaps.clone())
            }
        };
        Held { _image: self.native.clone(), _heaps: heaps }
    }
}

impl super::upload::Transfer {
    /// Back every reserved image in `textures` before work that uses them.
    /// Later uploads and the guest queue's wait for them are ordered after it.
    pub(super) fn back(
        &mut self,
        device: &ID3D12Device,
        images: &Images,
        textures: &[std::sync::Arc<Texture>],
    ) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error.into());
        }
        let out_of_memory = |_| windows::core::Error::from(windows::Win32::Foundation::E_OUTOFMEMORY);
        let mut wanted = Vec::new();
        wanted.try_reserve(textures.len()).map_err(out_of_memory)?;
        {
            let images = lock(&images.0);
            wanted.extend(
                textures
                    .iter()
                    .filter(|texture| texture.residency.incomplete())
                    .map(|texture| (texture, mips::first_needed(&images, texture))),
            );
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let mut mapped = Vec::new();
        mapped.try_reserve(wanted.len()).map_err(out_of_memory)?;
        // Images whose title still holds only their small mips ask every time,
        // so the adapter is queried only when one of them can grow.
        let mut room = if wanted.iter().any(|(texture, first)| texture.residency.can_grow(*first)) {
            self.meter.get_or_insert_with(|| budget::Meter::new(device)).room()
        } else {
            return Ok(());
        };
        // A failure still has to cover the mappings queued before it.
        let result = wanted.into_iter().try_for_each(|(texture, first)| {
            let flags = texture.desc.Flags;
            if texture.residency.map(device, &self.queue, &texture.native, flags, first, &mut room)? {
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
