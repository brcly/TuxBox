//! Acceleration structures a title builds into memory it allocated itself.
//!
//! A console GPU address is the CPU address, so a title can build a structure
//! anywhere in its own committed memory, with no resource naming the range.
//! The host cannot place a structure there. Each such allocation gets a pool
//! without native storage: every structure in it is built into private
//! device-local storage and published under its guest address, like an
//! oversized structure in a registered pool.
use super::{Buffer, Buffers, Kind, lock, rebuild};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
};
use windows::core::Result;

/// The committed private or mapped allocation holding `address`, as its
/// reserved base and size. Module images are never structure memory. With
/// `reserved`, the address itself need not be committed: a console title
/// names a structure by its address in the view the GPU uses, which the CPU
/// may never touch.
fn allocation(address: u64, reserved: bool) -> Option<(u64, u64)> {
    let query = |at: u64| {
        let mut region = MEMORY_BASIC_INFORMATION::default();
        let size = size_of_val(&region);
        (unsafe { VirtualQuery(Some(at as _), &mut region, size) } != 0).then_some(region)
    };
    let region = query(address)?;
    let committed = region.State == MEM_COMMIT;
    if !(committed || (reserved && region.State == MEM_RESERVE))
        || !(region.Type == MEM_PRIVATE || region.Type == MEM_MAPPED)
    {
        return None;
    }
    let base = region.AllocationBase.addr() as u64;
    // Later commits inside the same reservation stay in one pool.
    let mut end = base;
    while let Some(next) = query(end).filter(|r| r.AllocationBase.addr() as u64 == base) {
        end = next.BaseAddress.addr() as u64 + next.RegionSize as u64;
    }
    (end > address).then_some((base, end - base))
}

/// Guest ranges of earlier structures that a structure at `start` spanning
/// `bytes` overwrites. Each extent is the result size the title reserved.
pub(super) fn overlapping(extents: &BTreeMap<u64, u64>, start: u64, bytes: u64) -> Vec<u64> {
    let end = start.saturating_add(bytes);
    extents
        .range(..end)
        .filter(|&(&other, &length)| other != start && other.saturating_add(length) > start)
        .map(|(&other, _)| other)
        .collect()
}

impl Buffers {
    /// The structure pool covering `address`, adding one for a committed guest
    /// allocation that no registered resource covers.
    pub(super) fn structure_pool(&self, address: u64) -> Result<Arc<Buffer>> {
        self.structure_pool_in(address, false)
    }

    /// As `structure_pool`; with `reserved`, an address that is reserved but
    /// not committed also gets a pool.
    pub(super) fn structure_pool_in(&self, address: u64, reserved: bool) -> Result<Arc<Buffer>> {
        match self.find(address, Kind::Structure) {
            Err(error) if error.code() == E_INVALIDARG => (),
            found => return found,
        }
        let mut pools = lock(&self.guest_pools);
        match self.find(address, Kind::Structure) {
            Err(error) if error.code() == E_INVALIDARG => (),
            found => return found,
        }
        let (start, bytes) = allocation(address, reserved).ok_or(E_INVALIDARG)?;
        let pool = Arc::new(Buffer {
            native: None,
            start,
            bytes,
            kind: Kind::Structure,
            targets: Mutex::default(),
            sizes: Mutex::default(),
            extents: Mutex::default(),
            mappings: self.mappings.clone(),
        });
        let mut index = lock(&self.index);
        // A pool left from a released allocation at this base is stale.
        if let Some(stale) = pools.insert(start, pool.clone())
            && let Some(entries) = index.get_mut(&(Kind::Structure, start))
        {
            entries.retain(|b| !Arc::ptr_eq(b, &stale));
        }
        index.entry((Kind::Structure, start)).or_default().push(pool.clone());
        drop(index);
        drop(pools);
        note_pool(start, bytes);
        Ok(pool)
    }
}

fn note_pool(start: u64, bytes: u64) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 8 || count.is_power_of_two() {
        crate::diagnostics::log(&format!(
            "acceleration structures in guest memory (pool #{count}): allocation {start:#x}+{bytes:#x}, private storage per structure"
        ));
    }
}

/// Private storage of at least `capacity` bytes for a structure written into
/// a guest-memory pool at `address`, where the title reserved `extent` bytes.
/// Storage too small for a rewrite is replaced; recorded commands retain the
/// old one until they complete. An extent never covers more than the title
/// reserved, so a live neighbour is never released.
pub(super) fn target(
    device: &ID3D12Device,
    registry: &Buffers,
    pool: &Arc<Buffer>,
    address: u64,
    extent: u64,
    capacity: u64,
) -> Result<(ID3D12Resource, u64)> {
    if pool.native.is_some() || extent == 0 || capacity == 0 {
        return Err(E_INVALIDARG.into());
    }
    let mut evicted = Vec::new();
    let resource = {
        let mut targets = lock(&pool.targets);
        let mut extents = lock(&pool.extents);
        let reusable = targets
            .get(&address)
            .and_then(Clone::clone)
            .filter(|existing| unsafe { existing.GetDesc() }.Width >= capacity);
        let overwritten = overlapping(&extents, address, extent);
        if !overwritten.is_empty() {
            let mut mappings = lock(&pool.mappings);
            for other in overwritten {
                extents.remove(&other);
                mappings.previous.remove(&other);
                if mappings.addresses.remove(&other).is_some() {
                    mappings.dirty();
                }
                if let Some(Some(old)) = targets.remove(&other) {
                    evicted.push(old);
                }
            }
        }
        let resource = match reusable {
            Some(existing) => existing,
            None => {
                let width = capacity
                    .checked_next_multiple_of(D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT as u64)
                    .ok_or(E_INVALIDARG)?;
                let resource = rebuild::as_buffer(device, width)?;
                if let Some(Some(old)) = targets.insert(address, Some(resource.clone())) {
                    evicted.push(old);
                }
                note_storage(width as i64);
                resource
            }
        };
        extents.insert(address, extent);
        // Publish the storage now rather than at submission. A scene can be
        // submitted before the list that builds one of its structures, when
        // that list waits for a title fence; its instances must still reach
        // this storage, never the guest address.
        let native = unsafe { resource.GetGPUVirtualAddress() };
        let mut mappings = lock(&pool.mappings);
        if mappings.addresses.insert(address, native) != Some(native) {
            mappings.dirty();
        }
        resource
    };
    if !evicted.is_empty() {
        let mut geometry = lock(&registry.geometry);
        for old in &evicted {
            geometry.forget(pool, unsafe { old.GetGPUVirtualAddress() });
            note_storage(-(unsafe { old.GetDesc() }.Width as i64));
        }
    }
    let native = unsafe { resource.GetGPUVirtualAddress() };
    Ok((resource, native))
}

/// Total private storage held for guest-memory structures, logged as it grows.
fn note_storage(change: i64) {
    static TOTAL: AtomicU64 = AtomicU64::new(0);
    static REPORTED: AtomicU64 = AtomicU64::new(0);
    let total = if change >= 0 {
        TOTAL.fetch_add(change as u64, Ordering::Relaxed) + change as u64
    } else {
        TOTAL.fetch_sub(change.unsigned_abs(), Ordering::Relaxed) - change.unsigned_abs()
    };
    let mib = total >> 20;
    let step = mib.checked_ilog2().map_or(0, |bit| 1u64 << bit);
    if step != 0 && REPORTED.fetch_max(step, Ordering::Relaxed) < step {
        crate::diagnostics::log(&format!(
            "acceleration structures in guest memory hold {mib} MiB of private storage"
        ));
    }
}

#[cfg(test)]
#[path = "../../tests/device/blas/guest_memory.rs"]
mod tests;
