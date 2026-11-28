//! A writable Xbox descriptor arena can exceed the shader-visible heap limit.
//! Store all native descriptors in CPU heaps and materialize bounded root
//! tables in a shader-visible heap when the command list references them.
mod spans;
mod staging;
mod leases;
mod lookup;
use spans::FreeSpans;
pub(crate) use leases::DescriptorLeases;
pub(crate) use leases::PendingSubmissions;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};
use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

use super::title_memory::GuestMemory;
mod index_map;
mod strides;
pub(crate) use index_map::index_map;

#[derive(Default)]
pub(crate) struct Registry {
    arenas: Mutex<BTreeMap<usize, Weak<Arena>>>,
    count: std::sync::atomic::AtomicUsize,
    next_version: AtomicU64,
    identity_map: Mutex<Option<ID3D12Resource>>,
}
impl Registry {
    pub(crate) fn find(&self, address: usize) -> Option<Arc<Arena>> {
        if self.count.load(Ordering::Acquire) == 0 {
            return None;
        }
        let entries = self.arenas.lock().unwrap_or_else(|e| e.into_inner());
        let arena = entries.range(..=address).next_back()?.1.upgrade()?;
        (address - arena.base() < arena.count as usize * arena.guest_stride()).then_some(arena)
    }
    pub(super) fn copy_source(
        &self,
        handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let Some(arena) = self.find(handle.ptr) else {
            return handle;
        };
        if arena.inline && (handle.ptr - arena.base()).is_multiple_of(arena.guest_stride())
            && let Some(source) = arena.resolve_inline(arena.inline_record(handle.ptr))
        {
            return source;
        }
        arena.cpu(handle.ptr)
    }
}

// Only the opaque record lives in guest-writable memory. The host descriptor
// stays in a CPU-only heap and is never edited by the guest's 32-byte copies.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct InlineDescriptor {
    source: usize,
    version: u64,
    reserved: [u64; 2],
}
pub(super) const INLINE_BYTES: usize = size_of::<InlineDescriptor>();

/// Fence completion oracle used for non-blocking retirement of in-flight batches.
#[derive(Clone)]
pub(crate) enum RetireFence {
    Native(ID3D12Fence),
    #[cfg(test)]
    Mock(Arc<AtomicU64>),
}

impl RetireFence {
    fn completed_value(&self) -> u64 {
        match self {
            Self::Native(fence) => unsafe { fence.GetCompletedValue() },
            #[cfg(test)]
            Self::Mock(value) => value.load(Ordering::Acquire),
        }
    }
}

struct TableEntry {
    id: u64,
    slot: u32,
    pins: u32,
    direct_pinned: bool,
    last_used: u64,
}

struct InFlightBatch {
    fence: RetireFence,
    value: u64,
    ids: Vec<u64>,
}

/// Displaced host view metadata collected under `tables` and dropped after releasing the lock.
#[derive(Default)]
struct RetiredViews {
    imported: Vec<super::views::DescriptorBacking>,
    placed: Vec<super::placement::PlacedView>,
    scenes: Vec<super::blas::SceneView>,
}

#[derive(Default)]
struct Tables {
    used: u32,
    free: FreeSpans,
    ranges: BTreeMap<(u32, u32), TableEntry>,
    // No range in `ranges` is longer than this, so one that overlaps a slot
    // starts at most this far before it.
    longest: u32,
    by_id: HashMap<u64, (u32, u32)>,
    next_id: u64,
    tick: u64,
    in_flight: VecDeque<InFlightBatch>,
    direct: index_map::Direct,
    resident_records: BTreeMap<usize, InlineDescriptor>,
}

impl Tables {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// Non-blocking scan across all queued submissions (queues share an arena
    /// and can retire out of order).
    fn retire_completed(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.in_flight.len() {
            let completed = self.in_flight[i].fence.completed_value();
            if completed != u64::MAX && completed >= self.in_flight[i].value {
                let batch = self.in_flight.swap_remove_back(i).expect("in-flight index valid");
                for id in batch.ids {
                    if let Some(&key) = self.by_id.get(&id)
                        && let Some(entry) = self.ranges.get_mut(&key)
                    {
                        debug_assert!(entry.pins > 0);
                        entry.pins = entry.pins.saturating_sub(1);
                    }
                }
            } else {
                i += 1;
            }
        }
    }

    fn evict_oldest(&mut self, arena: &Arena, retired: &mut RetiredViews) -> bool {
        let Some((&victim_key, _)) = self
            .ranges
            .iter()
            .filter(|(_, entry)| entry.pins == 0 && !entry.direct_pinned)
            .min_by_key(|&(key, entry)| (entry.last_used, *key))
        else {
            return false;
        };
        let victim = self.ranges.remove(&victim_key).expect("victim key exists");
        self.by_id.remove(&victim.id);
        let len = victim_key.1;
        if arena.inline && !self.resident_records.is_empty() {
            let start = victim.slot as usize;
            let end = start + len as usize;
            self.resident_records
                .extract_if(start..end, |_, _| true)
                .for_each(drop);
        }
        arena.clear_host_tracking(victim.slot, len, retired);
        self.free.free(victim.slot, len, &mut self.used);
        true
    }

    fn try_alloc_contiguous(&mut self, limit: u32, length: u32) -> Option<u32> {
        if let Some(slot) = self.free.alloc(length) {
            return Some(slot);
        }
        let end = self.used.checked_add(length).filter(|&end| end <= limit)?;
        let slot = self.used;
        self.used = end;
        Some(slot)
    }

    fn allocate_contiguous(
        &mut self,
        arena: &Arena,
        length: u32,
        retired: &mut RetiredViews,
    ) -> Result<u32> {
        if let Some(slot) = self.try_alloc_contiguous(arena.limit, length) {
            return Ok(slot);
        }
        self.retire_completed();
        if let Some(slot) = self.try_alloc_contiguous(arena.limit, length) {
            return Ok(slot);
        }
        while self.evict_oldest(arena, retired) {
            if let Some(slot) = self.try_alloc_contiguous(arena.limit, length) {
                return Ok(slot);
            }
        }
        self.log_exhausted(arena.limit, length);
        Err(E_OUTOFMEMORY.into())
    }

    fn ensure_direct_capacity(
        &mut self,
        arena: &Arena,
        needed: u32,
        retired: &mut RetiredViews,
    ) -> Result<()> {
        let has_space = |tables: &Self| {
            u64::from(tables.free.free_slots()) + u64::from(arena.limit.saturating_sub(tables.used))
                >= u64::from(needed)
        };
        if has_space(self) {
            return Ok(());
        }
        self.retire_completed();
        if has_space(self) {
            return Ok(());
        }
        while !has_space(self) {
            if !self.evict_oldest(arena, retired) {
                self.log_exhausted(arena.limit, needed);
                return Err(E_OUTOFMEMORY.into());
            }
        }
        Ok(())
    }

    fn log_exhausted(&self, limit: u32, needed: u32) {
        let pinned = self.ranges.values().filter(|entry| entry.pins > 0).count();
        let direct_pinned = self.ranges.values().filter(|entry| entry.direct_pinned).count();
        let free_slots = self.free.free_slots();
        let largest_span = self.free.largest_span();
        static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            crate::diagnostics::log(&format!(
                "descriptor arena exhausted: used={} limit={} tables={} pinned={} direct_pinned={} in_flight_batches={} free_slots={} largest_span={} needed={}",
                self.used,
                limit,
                self.ranges.len(),
                pinned,
                direct_pinned,
                self.in_flight.len(),
                free_slots,
                largest_span,
                needed
            ));
        }
    }
}

// What each map knows about the views in one CPU heap, forgotten with it.
struct ChunkViews {
    imported: super::placement::DescriptorRange<super::views::DescriptorBacking>,
    placed: super::placement::DescriptorRange<super::placement::PlacedView>,
    scenes: super::placement::DescriptorRange<super::blas::SceneView>,
    _strides: super::placement::DescriptorRange<u32>,
}

impl ChunkViews {
    fn new(owner: &super::Device, heap: &ID3D12DescriptorHeap, desc: &D3D12_DESCRIPTOR_HEAP_DESC) -> Option<Self> {
        let device = &owner.native;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        Some(Self {
            imported: owner.state.imported_views.heap(device, heap, desc, kind)?,
            placed: owner.state.placements.views.heap(device, heap, desc, kind)?,
            scenes: owner.state.blas.views.heap(device, heap, desc, kind)?,
            _strides: owner.state.view_strides.heap(device, heap, desc, kind)?,
        })
    }
}

struct NativeArena {
    _memory: GuestMemory,
    // The maps are shared by every chunk; the first chunk's ranges reach them.
    imported: super::placement::DescriptorRange<super::views::DescriptorBacking>,
    placed: super::placement::DescriptorRange<super::placement::PlacedView>,
    scenes: super::placement::DescriptorRange<super::blas::SceneView>,
    _strides: super::placement::DescriptorRange<u32>,
    _later_chunks: Vec<ChunkViews>,
    _staging: Vec<ID3D12DescriptorHeap>,
    visible: ID3D12DescriptorHeap,
    device: ID3D12Device,
    visible_cpu_start: usize,
    visible_gpu_start: u64,
    staging: staging::Chunks,
}

pub(crate) struct Arena {
    base_address: usize,
    native: Option<NativeArena>,
    registry: Arc<Registry>,
    stride: usize,
    count: u32,
    limit: u32,
    inline: bool,
    authored: Mutex<BTreeMap<usize, u64>>,
    // Guest ranges can overlap. Keep stable slots for every table that has
    // been referenced; a CPU descriptor update is published to each alias.
    tables: Mutex<Tables>,
    strides: Mutex<strides::Strides>,
}

impl Drop for Arena {
    fn drop(&mut self) {
        let mut entries = self.registry.arenas.lock().unwrap_or_else(|e| e.into_inner());
        if entries
            .get(&self.base())
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            entries.remove(&self.base());
            self.registry
                .count
                .store(entries.len(), Ordering::Release);
        }
    }
}

impl Arena {
    pub(super) fn create(
        owner: &super::Device,
        desc: &D3D12_DESCRIPTOR_HEAP_DESC,
        limit: u32,
    ) -> Result<Arc<Self>> {
        let registry = &owner.state.descriptor_arenas;
        let device = &owner.native;
        let cpu = D3D12_DESCRIPTOR_HEAP_DESC {
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            ..*desc
        };
        let gpu = D3D12_DESCRIPTOR_HEAP_DESC {
            NumDescriptors: limit,
            ..*desc
        };
        let shader_visible = desc.Flags == D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE;
        let most = super::host_profile::cpu_view_heap();
        // Only a shader-visible arena keeps a separate visible heap; otherwise
        // the CPU heap is the one the guest's handles must reach in one piece.
        let (chunk, staging) = staging::create_chunks(desc.NumDescriptors, shader_visible, |count| {
            if most.is_some_and(|most| count > most) {
                return Err(E_OUTOFMEMORY.into());
            }
            let chunk = D3D12_DESCRIPTOR_HEAP_DESC { NumDescriptors: count, ..cpu };
            unsafe { device.CreateDescriptorHeap::<ID3D12DescriptorHeap>(&chunk) }
        })?;
        if staging.len() > 1 {
            crate::diagnostics::log(&format!(
                "descriptor arena: host refused one {}-descriptor CPU heap; using {} of {chunk}",
                desc.NumDescriptors,
                staging.len()
            ));
        }
        let visible = if shader_visible {
            unsafe { device.CreateDescriptorHeap(&gpu)? }
        } else {
            staging[0].clone()
        };
        let stride = unsafe { device.GetDescriptorHandleIncrementSize(desc.Type) } as usize;
        let inline = owner.state.device11_queue
            && stride >= INLINE_BYTES
            && stride.is_multiple_of(INLINE_BYTES);
        let guest_stride = if inline { INLINE_BYTES } else { stride };
        let bytes = guest_stride
            .checked_mul(desc.NumDescriptors as usize)
            .ok_or(E_OUTOFMEMORY)?;
        let memory = GuestMemory::allocate(bytes, 0x0480_0004)?;
        let base_address = memory.address;
        // The title's allocator need not clear what it returns. A slot nobody
        // has written must read as no descriptor: leftover bytes there are
        // neither an authored record nor an empty one, and a table that spans
        // such a slot would be refused whole.
        // SAFETY: the allocation above is `bytes` long and owned by this arena.
        unsafe { core::ptr::write_bytes(base_address as *mut u8, 0, bytes) };
        let visible_cpu_start = unsafe { visible.GetCPUDescriptorHandleForHeapStart() }.ptr;
        let visible_gpu_start = if desc.Flags == D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE {
            unsafe { visible.GetGPUDescriptorHandleForHeapStart() }.ptr
        } else {
            0
        };
        let starts = staging
            .iter()
            .map(|heap| unsafe { heap.GetCPUDescriptorHandleForHeapStart() }.ptr)
            .collect();
        let mut views = staging
            .iter()
            .enumerate()
            .map(|(index, heap)| {
                let held = chunk.min(desc.NumDescriptors - index as u32 * chunk);
                ChunkViews::new(owner, heap, &D3D12_DESCRIPTOR_HEAP_DESC { NumDescriptors: held, ..cpu })
                    .ok_or(E_INVALIDARG)
            })
            .collect::<core::result::Result<Vec<_>, _>>()?
            .into_iter();
        let first = views.next().ok_or(E_INVALIDARG)?;
        let arena = Arc::new(Self {
            base_address,
            native: Some(NativeArena {
                _memory: memory,
                imported: first.imported,
                placed: first.placed,
                scenes: first.scenes,
                _strides: first._strides,
                _later_chunks: views.collect(),
                _staging: staging,
                visible,
                device: device.clone(),
                visible_cpu_start,
                visible_gpu_start,
                staging: staging::Chunks { starts, chunk },
            }),
            registry: registry.clone(),
            stride,
            count: desc.NumDescriptors,
            limit,
            inline,
            authored: Mutex::default(),
            tables: Mutex::new(Tables::default()),
            strides: Mutex::default(),
        });
        let mut entries = registry.arenas.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(arena.base(), Arc::downgrade(&arena));
        registry.count.store(entries.len(), Ordering::Release);
        Ok(arena)
    }

    pub(crate) fn uses_inline_records(&self) -> bool {
        self.inline
    }

    pub(crate) fn guest_stride(&self) -> usize {
        if self.inline { INLINE_BYTES } else { self.stride }
    }

    pub(crate) fn base(&self) -> usize {
        self.base_address
    }

    pub(crate) fn contains(&self, address: u64) -> bool {
        address
            .checked_sub(self.base() as u64)
            .is_some_and(|offset| offset < self.count as u64 * self.guest_stride() as u64)
    }

    pub(crate) fn visible(&self) -> &ID3D12DescriptorHeap {
        &self.native.as_ref().expect("native descriptor arena").visible
    }

    pub(crate) fn cpu(&self, address: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = (address - self.base()) / self.guest_stride();
        let ptr = match &self.native {
            Some(native) => native.staging.handle(slot as u32, self.stride),
            None => 0x3000_0000 + slot * self.stride,
        };
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr }
    }

    fn inline_record(&self, address: usize) -> InlineDescriptor {
        // The arena owns this committed memory. Bounds are checked by callers
        // for the whole record, and the title synchronizes descriptor writes.
        unsafe { (address as *const InlineDescriptor).read_unaligned() }
    }

    pub(super) fn author(&self, address: usize, count: u32) {
        let guest_stride = self.guest_stride();
        if !self.inline
            || !self.contains(address as u64)
            || !(address - self.base()).is_multiple_of(guest_stride)
            || count as usize > self.count as usize - (address - self.base()) / guest_stride
        {
            return;
        }
        let mut versions = self.authored.lock().unwrap_or_else(|e| e.into_inner());
        for slot in 0..count as usize {
            let source = address + slot * guest_stride;
            let version = self.registry.next_version.fetch_add(1, Ordering::Relaxed) + 1;
            versions.insert(source, version);
            let record = InlineDescriptor {
                source,
                version,
                reserved: [0; 2],
            };
            // The caller validated the destination span through the heap API.
            unsafe { (source as *mut InlineDescriptor).write_unaligned(record) };
        }
    }

    fn resolve_inline(&self, record: InlineDescriptor) -> Option<D3D12_CPU_DESCRIPTOR_HANDLE> {
        if record.source == 0 || record.version == 0 {
            return None;
        }
        let source = self.registry.find(record.source)?;
        let versions = source.authored.lock().unwrap_or_else(|e| e.into_inner());
        (versions.get(&record.source) == Some(&record.version)).then(|| source.cpu(record.source))
    }

    fn copy_native_view(
        &self,
        to: D3D12_CPU_DESCRIPTOR_HANDLE,
        from: D3D12_CPU_DESCRIPTOR_HANDLE,
        retired: &mut RetiredViews,
    ) {
        let Some(native) = &self.native else {
            return;
        };
        let stride = self.stride;
        retired
            .imported
            .extend(native.imported.copy_to_deferred(stride, to.ptr, from.ptr, 1));
        retired
            .placed
            .extend(native.placed.copy_to_deferred(stride, to.ptr, from.ptr, 1));
        retired
            .scenes
            .extend(native.scenes.copy_to_deferred(stride, to.ptr, from.ptr, 1));
        crate::diagnostics::display::copy_texture_views(to.ptr, from.ptr, 1, stride);
        unsafe {
            native
                .device
                .CopyDescriptorsSimple(1, to, from, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
        };
    }

    fn copy(&self, src: u32, dst: u32, count: u32, retired: &mut RetiredViews) {
        let Some(native) = &self.native else {
            return;
        };
        // The visible heap is one piece; the CPU heaps may not be.
        for (slot, length) in native.staging.runs(src, count) {
            self.copy_run(native, slot, dst + (slot - src), length, retired);
        }
    }

    fn copy_run(&self, native: &NativeArena, src: u32, dst: u32, count: u32, retired: &mut RetiredViews) {
        let from = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: native.staging.handle(src, self.stride),
        };
        let to = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: native.visible_cpu_start + dst as usize * self.stride,
        };
        retired
            .imported
            .extend(native.imported.copy_to_deferred(self.stride, to.ptr, from.ptr, count));
        retired
            .placed
            .extend(native.placed.copy_to_deferred(self.stride, to.ptr, from.ptr, count));
        retired
            .scenes
            .extend(native.scenes.copy_to_deferred(self.stride, to.ptr, from.ptr, count));
        crate::diagnostics::display::copy_texture_views(to.ptr, from.ptr, count, self.stride);
        unsafe {
            native.device.CopyDescriptorsSimple(
                count,
                to,
                from,
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            )
        };
    }

    fn clear_host_tracking(&self, slot: u32, count: u32, retired: &mut RetiredViews) {
        let Some(native) = &self.native else {
            return;
        };
        let dst = native.visible_cpu_start + slot as usize * self.stride;
        retired
            .imported
            .extend(native.imported.clear_deferred(self.stride, dst, count));
        retired
            .placed
            .extend(native.placed.clear_deferred(self.stride, dst, count));
        retired
            .scenes
            .extend(native.scenes.clear_deferred(self.stride, dst, count));
        if crate::diagnostics::display::trace_views() {
            for i in 0..count as usize {
                crate::diagnostics::display::texture_view(dst + i * self.stride, None, None);
            }
        }
    }

    fn pin_ids(&self, ids: &[u64]) {
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        for &id in ids {
            if let Some(&key) = tables.by_id.get(&id)
                && let Some(entry) = tables.ranges.get_mut(&key)
            {
                entry.pins = entry.pins.saturating_add(1);
            } else {
                debug_assert!(false, "pin_ids called for non-resident table id {id}");
            }
        }
    }

    fn unpin_ids(&self, ids: impl IntoIterator<Item = u64>) {
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        for id in ids {
            if let Some(&key) = tables.by_id.get(&id)
                && let Some(entry) = tables.ranges.get_mut(&key)
            {
                debug_assert!(entry.pins > 0);
                entry.pins = entry.pins.saturating_sub(1);
            } else {
                debug_assert!(false, "unpin_ids called for unknown table id {id}");
            }
        }
    }

    fn push_in_flight(&self, fence: RetireFence, value: u64, ids: Vec<u64>) {
        let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        tables.retire_completed();
        tables
            .in_flight
            .push_back(InFlightBatch { fence, value, ids });
    }

    pub(crate) fn publish(&self, address: usize, count: u32) {
        let first = ((address - self.base()) / self.guest_stride()) as u32;
        let end = first.saturating_add(count);
        if end > self.count || count == 0 {
            return;
        }
        let mut retired = RetiredViews::default();
        {
            let mut tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
            let tables = &mut *tables;
            if tables.direct.is_resident() {
                let missing = tables.direct.missing_in_range(first..end);
                if missing > 0 && tables.ensure_direct_capacity(self, missing, &mut retired).is_err() {
                    tables.direct.set_exhausted();
                } else {
                    let Tables {
                        direct,
                        free,
                        used,
                        ..
                    } = tables;
                    direct.publish_resident(self, first..end, free, used, &mut retired);
                }
            } else {
                tables.direct.record_written(first..end);
            }
            let Tables {
                ranges,
                resident_records,
                longest,
                ..
            } = tables;
            // Only tables that start less than the longest table's length
            // before `first` can reach it. Walking every table made each
            // descriptor write cost as much as the whole table set.
            let low = first.saturating_sub(*longest);
            for (&(start, length), entry) in ranges.range((low, 0)..(end, 0)) {
                let resident = entry.slot;
                let lo = start.max(first);
                let hi = (start + length).min(end);
                if lo < hi {
                    let destination = resident + lo - start;
                    self.copy(lo, destination, hi - lo, &mut retired);
                    if self.inline {
                        for slot in destination..destination + hi - lo {
                            resident_records.remove(&(slot as usize));
                        }
                    }
                }
            }
        }
        drop(retired);
    }

}

// Keep the logical slot and publish descriptor writes after the host call.
pub(super) struct Write {
    arena: Option<Arc<Arena>>,
    address: usize,
    // The console buffer stride of the new view (0 when it has none).
    stride: std::cell::Cell<u32>,
}
impl Write {
    pub(super) fn new(registry: &Registry, handle: &mut D3D12_CPU_DESCRIPTOR_HANDLE) -> Self {
        let address = handle.ptr;
        let arena = registry.find(address);
        if let Some(a) = &arena {
            *handle = a.cpu(address);
        }
        Self { arena, address, stride: std::cell::Cell::new(0) }
    }

    pub(super) fn set_stride(&self, stride: u32) {
        self.stride.set(stride);
    }
}
impl Drop for Write {
    fn drop(&mut self) {
        if let Some(arena) = &self.arena {
            // Every write replaces the slot's stride, so a later view of another
            // kind does not inherit one.
            arena.set_strides(self.address, &[self.stride.get()]);
            arena.author(self.address, 1);
            arena.publish(self.address, 1);
        }
    }
}

#[cfg(test)]
#[path = "../tests/device/descriptor_paging.rs"]
mod tests;

/// The host CPU handle that holds the view a guest CPU handle names, in
/// whichever arena it lives, or the handle itself outside every arena.
///
/// # Safety
/// `device` is a live provider device.
pub(crate) unsafe fn host_handle(device: *mut core::ffi::c_void, handle: D3D12_CPU_DESCRIPTOR_HANDLE) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    let device = unsafe { &*device.cast::<super::Device>() };
    device.state.descriptor_arenas.copy_source(handle)
}
