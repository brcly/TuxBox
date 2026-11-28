//! Views the API replaced, kept for the records a title copied before.
//!
//! On console a descriptor copy holds the descriptor's bytes, so a title can
//! reuse a CPU heap slot as soon as it has copied from it. ProjectScarlett
//! writes a descriptor into a CPU heap of 3,128 slots, copies its 32 bytes into
//! its shader-visible heap and writes the next descriptor into the same slot.
//! Here the copied bytes are a record naming the slot and the version written
//! there, so the view of that version has to outlive the slot's next write.
//! Versions are unique across the device: a ring indexed by version keeps the
//! views most recently replaced.
use super::{Arena, INLINE_BYTES, RetiredViews};
use crate::device::{Device, blas::SceneView, placement::DescriptorRange, placement::PlacedView, views::DescriptorBacking};
use std::sync::Mutex;
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Result;

/// Replaced views kept. A copy resolves while fewer than this many views have
/// been replaced since it was made; ProjectScarlett's lag about 12,500 behind
/// when a submission reaches them.
const KEPT: usize = 1 << 16;

pub(super) struct History {
    _heap: ID3D12DescriptorHeap,
    start: usize,
    stride: usize,
    _imported: DescriptorRange<DescriptorBacking>,
    _placed: DescriptorRange<PlacedView>,
    _scenes: DescriptorRange<SceneView>,
    // The slot address and version whose view each ring entry holds.
    tags: Mutex<Vec<(usize, u64)>>,
}

impl History {
    pub(super) fn new(owner: &Device) -> Result<Self> {
        let device = &owner.native;
        let desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            NumDescriptors: KEPT as u32,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        };
        let heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&desc)? };
        let start = unsafe { heap.GetCPUDescriptorHandleForHeapStart() }.ptr;
        let stride = unsafe { device.GetDescriptorHandleIncrementSize(desc.Type) } as usize;
        // The kept views carry the same tracking as a heap slot's.
        let state = &owner.state;
        let imported = state.imported_views.heap(device, &heap, &desc, desc.Type).ok_or(E_INVALIDARG)?;
        let placed = state.placements.views.heap(device, &heap, &desc, desc.Type).ok_or(E_INVALIDARG)?;
        let scenes = state.blas.views.heap(device, &heap, &desc, desc.Type).ok_or(E_INVALIDARG)?;
        Ok(Self {
            _heap: heap,
            start,
            stride,
            _imported: imported,
            _placed: placed,
            _scenes: scenes,
            tags: Mutex::new(vec![(0, 0); KEPT]),
        })
    }

    fn entry(&self, version: u64) -> (usize, D3D12_CPU_DESCRIPTOR_HANDLE) {
        let ring = (version % KEPT as u64) as usize;
        (ring, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.start + ring * self.stride })
    }

    /// Where the view that `version` had at `address` is kept, while it is.
    pub(super) fn find(&self, address: usize, version: u64) -> Option<D3D12_CPU_DESCRIPTOR_HANDLE> {
        let (ring, handle) = self.entry(version);
        let tags = self.tags.lock().unwrap_or_else(|e| e.into_inner());
        (tags[ring] == (address, version)).then_some(handle)
    }
}

impl Arena {
    /// Keep the views about to be replaced at `count` slots from `address`
    /// (see `History`). Call before the host writes those slots.
    pub(in crate::device) fn preserve(&self, address: usize, count: u32) {
        if !self.inline {
            return;
        }
        let Some(history) = self.registry.history.get().and_then(Option::as_ref) else {
            return;
        };
        let replaced: Vec<(usize, u64)> = {
            let authored = self.authored.lock().unwrap_or_else(|e| e.into_inner());
            (0..count as usize)
                .filter_map(|i| {
                    let slot = address + i * INLINE_BYTES;
                    authored.get(&slot).map(|&version| (slot, version))
                })
                .collect()
        };
        if replaced.is_empty() {
            return;
        }
        let mut retired = RetiredViews::default();
        {
            let mut tags = history.tags.lock().unwrap_or_else(|e| e.into_inner());
            for (slot, version) in replaced {
                let (ring, to) = history.entry(version);
                self.copy_native_view(to, self.cpu(slot), &mut retired);
                tags[ring] = (slot, version);
            }
        }
        drop(retired);
    }
}
