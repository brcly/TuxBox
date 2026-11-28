//! Fence a private copy queue against the guest queues that read these textures.
use super::budget::Meter;
use super::residency::Held;
use super::{Digest, Texture, committed, lock};
use core::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::VecDeque, sync::Arc};
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_OUTOFMEMORY, HANDLE};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::DXGI_SAMPLE_DESC};
use windows::core::{HRESULT, Interface, Result};

pub(super) struct Change {
    pub(super) texture: Arc<Texture>,
    pub(super) level: usize,
    pub(super) bytes: Vec<u8>,
    /// The digest of `bytes`, recorded once their upload is queued.
    pub(super) digest: Digest,
}

// These COM objects must survive the asynchronous copy, even if the guest
// drops all associated resource and queue references immediately after Submit.
pub(super) struct Upload {
    _buffer: ID3D12Resource,
    _allocator: ID3D12CommandAllocator,
    list: ID3D12CommandList,
    _textures: Vec<Held>,
    staging: u64,
}

/// Staging bytes of uploads the transfer queue has not released.
static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

pub(super) fn staging_in_flight() -> u64 {
    IN_FLIGHT.load(Ordering::Relaxed)
}

impl Drop for Upload {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(self.staging, Ordering::Relaxed);
    }
}

/// What work queued on the transfer queue holds until its fence value.
#[allow(dead_code)] // Held only to be dropped.
pub(super) enum Pending {
    Upload(Upload),
    /// Reserved images whose tiles are being mapped.
    Mapping(Vec<Held>),
}

/// Changed bytes that go into one upload. A title's first flush can change
/// gigabytes, and each upload holds them twice: the bytes read from the title
/// and the staging buffer they are detiled into.
const BATCH_BYTES: usize = 128 << 20;

pub(super) struct Transfer {
    pub(super) queue: ID3D12CommandQueue,
    pub(super) fence: ID3D12Fence,
    next: u64,
    pub(super) failed: Option<HRESULT>,
    pending: VecDeque<(u64, Pending)>,
    scratch: Vec<u8>,
    /// The adapter's memory, asked before images are backed.
    pub(super) meter: Option<Meter>,
}

impl Transfer {
    pub(super) fn new(device: &ID3D12Device) -> Result<Self> {
        let desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            ..Default::default()
        };
        Ok(Self {
            queue: unsafe { device.CreateCommandQueue(&desc)? },
            fence: unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? },
            next: 0,
            failed: None,
            pending: VecDeque::new(),
            scratch: Vec::new(),
            meter: None,
        })
    }

    pub(super) fn tick(&mut self) -> Result<u64> {
        self.next = self.next.checked_add(1).ok_or(E_FAIL)?;
        Ok(self.next)
    }

    /// Hold `work` until the transfer fence reaches `tick`.
    pub(super) fn keep(&mut self, tick: u64, work: Pending) -> Result<()> {
        self.pending.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
        self.pending.push_back((tick, work));
        Ok(())
    }

    pub(super) fn refresh(
        &mut self,
        device: &ID3D12Device,
        textures: &[Arc<Texture>],
    ) -> Result<()> {
        if let Some(error) = self.failed {
            return Err(error.into());
        }
        let mut changes = Vec::new();
        let mut bytes = 0;
        for texture in textures {
            let found = changes.len();
            texture.changed(&mut changes, &mut self.scratch)?;
            bytes += changes[found..].iter().map(|change: &Change| change.bytes.len()).sum::<usize>();
            // All of a texture's mips stay in one upload.
            if bytes >= BATCH_BYTES {
                self.upload(device, std::mem::take(&mut changes))?;
                bytes = 0;
            }
        }
        self.upload(device, changes)
    }

    fn upload(&mut self, device: &ID3D12Device, changes: Vec<Change>) -> Result<()> {
        let completed = unsafe { self.fence.GetCompletedValue() };
        while self
            .pending
            .front()
            .is_some_and(|(tick, _)| *tick <= completed)
        {
            self.pending.pop_front();
        }
        if changes.is_empty() {
            return Ok(());
        }
        if crate::diagnostics::display::trace_views() {
            for change in &changes {
                let desc = change.texture.desc;
                if desc.Height == 1 && desc.Width <= 1024 {
                    use windows::core::Interface;
                    crate::diagnostics::recording_error(format_args!(
                        "uploading changed narrow texture {:p}, guest {:#x}, {}x{} format {} subresource {}",
                        change.texture.native.as_raw(), change.texture.address, desc.Width,
                        desc.Height, desc.Format.0, change.level,
                    ));
                }
            }
        }
        let upload = Upload::record(device, &changes)?;
        let mut reader_waits = Vec::new();
        for mips in changes.chunk_by(|a, b| Arc::ptr_eq(&a.texture, &b.texture)) {
            // A guest may use a different queue on each submission. Order
            // replacement writes after every outstanding reader of this texture.
            let contents = lock(&mips[0].texture.contents);
            for (fence, value) in &contents.readers {
                if unsafe { fence.GetCompletedValue() } < *value
                    && !super::held::blocked(fence, *value)
                {
                    super::waits::include(&mut reader_waits, fence, *value);
                }
            }
        }
        for (fence, value) in reader_waits {
            unsafe { self.queue.Wait(&fence, value)? };
            crate::commands::queue_trace::waited(&self.queue, "upload waits for texture reads", &fence, value);
        }
        let tick = self.tick()?;
        self.pending.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
        unsafe {
            self.queue.ExecuteCommandLists(&[Some(upload.list.clone())]);
        }
        self.pending.push_back((tick, Pending::Upload(upload)));
        if let Err(error) = unsafe { self.queue.Signal(&self.fence, tick) } {
            self.failed = Some(error.code());
            return Err(error);
        }
        for change in changes {
            let mut state = lock(&change.texture.contents);
            state.previous[change.level] = Some(change.digest);
            state.uploaded = tick;
            state.readers.clear();
        }
        Ok(())
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some((tick, _)) = self.pending.back() {
            // Resignal in case the original signal failed after ExecuteCommandLists.
            unsafe {
                if self.fence.GetCompletedValue() < *tick
                    && self.queue.Signal(&self.fence, *tick).is_ok()
                {
                    let _ = self.fence.SetEventOnCompletion(*tick, HANDLE::default());
                }
            }
        }
    }
}

impl Upload {
    fn record(device: &ID3D12Device, changes: &[Change]) -> Result<Self> {
        let alignment = u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT);
        let mut total = 0u64;
        let mut offsets = Vec::with_capacity(changes.len());
        for change in changes {
            let offset = total
                .checked_next_multiple_of(alignment)
                .ok_or(E_OUTOFMEMORY)?;
            offsets.push(offset);
            total = offset
                .checked_add(change.texture.mips[change.level].bytes)
                .ok_or(E_OUTOFMEMORY)?;
        }
        let size = usize::try_from(total)
            .ok()
            .filter(|n| *n <= isize::MAX as usize)
            .ok_or(E_OUTOFMEMORY)?;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: total,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let buffer = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
        )?;
        let allocator = unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)? };
        let list: ID3D12GraphicsCommandList =
            unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)? };
        let mut mapped = core::ptr::null_mut();
        let empty = D3D12_RANGE::default();
        unsafe {
            buffer.Map(0, Some(&empty), Some(&mut mapped))?;
        }
        // SAFETY: this CPU-writeable buffer covers 'size' bytes; this command
        // list has not been executed and the pointer is scoped to the mapping.
        let output = unsafe { core::slice::from_raw_parts_mut(mapped.cast::<u8>(), size) };
        let mut held = Vec::new();
        let mut last = core::ptr::null();
        let result: Result<()> = changes
            .iter()
            .zip(offsets)
            .try_for_each(|(change, offset)| {
                change.detile(&mut output[offset as usize..])?;
                let texture = &change.texture;
                let mut footprint = texture.mips[change.level].footprint;
                footprint.Offset = offset;
                let mut src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(buffer.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: footprint,
                    },
                };
                let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(texture.native.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: change.level as u32,
                    },
                };
                // SAFETY: both active union fields match their location tags. The
                // batch retains source and destinations until its fence completes.
                unsafe {
                    list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                    ManuallyDrop::drop(&mut src.pResource);
                    ManuallyDrop::drop(&mut dst.pResource);
                }
                // A texture's changed mips are adjacent.
                if !core::ptr::eq(last, Arc::as_ptr(texture)) {
                    last = Arc::as_ptr(texture);
                    held.push(texture.hold());
                }
                Ok(())
            });
        unsafe {
            buffer.Unmap(0, None);
        }
        result?;
        unsafe {
            list.Close()?;
        }
        let list = list.cast()?;
        IN_FLIGHT.fetch_add(total, Ordering::Relaxed);
        Ok(Self {
            _buffer: buffer,
            _allocator: allocator,
            list,
            _textures: held,
            staging: total,
        })
    }
}

impl Change {
    fn detile(&self, output: &mut [u8]) -> Result<()> {
        self.texture.layout_runs(self.level, |tiled, linear| {
            output
                .get_mut(linear)
                .ok_or(E_INVALIDARG)?
                .copy_from_slice(self.bytes.get(tiled).ok_or(E_INVALIDARG)?);
            Ok(())
        })
    }
}

impl Texture {
    /// Corresponding contiguous byte runs in the tiled Xbox mip and in its
    /// D3D12 footprint. Upload and writeback must use the same addresses.
    pub(super) fn layout_runs(
        &self,
        level: usize,
        mut visit: impl FnMut(std::ops::Range<usize>, std::ops::Range<usize>) -> Result<()>,
    ) -> Result<()> {
        let mip = &self.mips[level];
        let element = mip.element_bytes as usize;
        let pitch = mip.footprint.Footprint.RowPitch as usize;
        let Some(plane) = &self.plane else {
            let source_pitch = ((mip.source.end - mip.source.start) / u64::from(mip.rows)) as usize;
            let row = mip.elements as usize * element;
            for y in 0..mip.rows as usize {
                visit(y * source_pitch..y * source_pitch + row, y * pitch..y * pitch + row)?;
            }
            return Ok(());
        };
        if self.desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            for z in 0..mip.footprint.Footprint.Depth {
                for y in 0..mip.rows {
                    for x in 0..mip.elements {
                        let tiled = plane.element_offset_in_volume(z, mip.level, u64::from(x), y).ok_or(E_INVALIDARG)? as usize;
                        let linear = (z as usize * mip.rows as usize + y as usize) * pitch + x as usize * element;
                        visit(tiled..tiled + element, linear..linear + element)?;
                    }
                }
            }
            return Ok(());
        }
        // In layout 0x105, X occupies the low address bits through byte 15.
        let run = if self.desc.Layout.0 == 0x100 {
            mip.elements as usize
        } else {
            16 / element
        };
        for y in 0..mip.rows {
            for x in (0..mip.elements).step_by(run) {
                let count = (mip.elements - x).min(run as u32) as usize * element;
                let tiled = plane
                    .element_offset_in_slice(mip.slice, mip.level, x.into(), y)
                    .and_then(|a| a.checked_sub(mip.source.start))
                    .ok_or(E_INVALIDARG)? as usize;
                let linear = y as usize * pitch + x as usize * element;
                visit(tiled..tiled + count, linear..linear + count)?;
            }
        }
        Ok(())
    }
}
