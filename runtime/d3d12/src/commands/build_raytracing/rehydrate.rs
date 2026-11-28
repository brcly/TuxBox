//! Bottom-level structures a title loads already built for the console.
//!
//! A title streams such a structure from disk in its "dehydrated" form and
//! asks the console's builder library to rehydrate it in place: the library
//! fills in the triangles from the geometry the call names. The host cannot
//! traverse console nodes, but the call carries everything a build needs, so
//! each entry is recorded as a host build of its geometry. The result goes to
//! private storage published under the title's address, as for any structure
//! in title memory.
//!
//! What is known of the console form comes from Leonida's `docs/bvh.md`
//! (sections 3.1 and 4a, at d6f8a66f), which measured it on the title's
//! archives: a 64-byte `XboxBvh4Header` begins the structure (flags, a count
//! of 64-byte nodes that leaves the header out, a CRC and an element count),
//! the structure spans `64 * (nodes + 1)` bytes, it holds no vertices, and
//! its leaves name triangles by geometry index and by primitive order of the
//! geometry's index buffer. A host build of the same geometry descriptions
//! in the same order therefore reports the same GeometryIndex and
//! PrimitiveIndex.
use super::super::{
    List,
    raytracing::{SceneCommand, retain},
    recording,
};
use super::guest_addresses::{self, Late};
use crate::device::blas;
use crate::diagnostics::log;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, S_OK};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{HRESULT, IUnknown};

type Build = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC;

const ALIGNMENT: u64 = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BYTE_ALIGNMENT as u64;

/// The first words of a console structure.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Header {
    flags: u32,
    nodes: u32,
    crc: u32,
    elements: u32,
}

const BOTTOM_LEVEL: u32 = 0x8000_0000;

fn header(bytes: &[u8]) -> Option<Header> {
    let word = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    Some(Header { flags: word(0)?, nodes: word(4)?, crc: word(8)?, elements: word(12)? })
}

/// The bytes the title holds for a structure with this header: what a later
/// structure must overlap before this one counts as overwritten. A title
/// packs small structures one alignment apart, so anything that is not a
/// bottom-level header claims only that much and no neighbour is released.
fn extent(header: Option<Header>) -> u64 {
    match header {
        Some(h) if h.flags & BOTTOM_LEVEL != 0 && h.nodes != 0 => {
            (64 * (u64::from(h.nodes) + 1)).next_multiple_of(ALIGNMENT)
        }
        _ => ALIGNMENT,
    }
}

/// Title memory as its CPU sees it; see `device::read_title_bytes`.
fn read(address: u64, out: &mut [u8]) -> bool {
    address != 0 && crate::device::read_title_bytes(address, out)
}

fn header_at(address: u64) -> Option<Header> {
    let mut bytes = [0u8; 16];
    read(address, &mut bytes).then(|| header(&bytes)).flatten()
}

/// Decodes left for submission: the geometry could not be read when the
/// build was recorded.
pub(in crate::commands) struct Deferred {
    dest: u64,
    late: Vec<Late>,
}

impl Deferred {
    /// Fill the recorded build's inputs from title memory as it is now. A
    /// geometry that still cannot be decoded stays empty; the submission goes
    /// ahead either way.
    pub(in crate::commands) fn prepare(&self, owner: &IUnknown) {
        static DONE: AtomicU64 = AtomicU64::new(0);
        static FAILED: AtomicU64 = AtomicU64::new(0);
        for (index, late) in self.late.iter().enumerate() {
            let result = late.decode(owner);
            let counter = if result.is_ok() { &DONE } else { &FAILED };
            let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 16 || count.is_power_of_two() {
                let dest = self.dest;
                match result {
                    Ok(()) => log(&format!(
                        "rehydration at {dest:#x}: geometry {index} decoded at submission (#{count})"
                    )),
                    Err(why) => log(&format!(
                        "rehydration at {dest:#x}: geometry {index} left empty at submission, {why} (#{count})"
                    )),
                }
            }
        }
    }
}

/// One entry ready to record.
struct Planned {
    build: Build,
    // The recorded build points into this.
    _geometry: Vec<D3D12_RAYTRACING_GEOMETRY_DESC>,
    late: Vec<Late>,
    result: u64,
    scratch: u64,
    extent: u64,
}

fn plan(
    state: &super::super::Recording,
    owner: &IUnknown,
    device: &ID3D12Device5,
    entry: &Build,
    extent: u64,
) -> Result<Planned, (&'static str, HRESULT)> {
    let inputs = &entry.Inputs;
    if inputs.Type != D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL {
        return Err(("not a bottom-level structure", E_NOTIMPL));
    }
    // SAFETY: the title supplies the descriptions for the duration of the call.
    if !unsafe { crate::device::ray_build::valid(inputs) } {
        return Err(("invalid inputs", E_INVALIDARG));
    }
    if entry.DestAccelerationStructureData == 0 || !entry.DestAccelerationStructureData.is_multiple_of(ALIGNMENT) {
        return Err(("destination address", E_INVALIDARG));
    }
    if inputs.Flags.contains(D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE) {
        return Err(("update", E_NOTIMPL));
    }
    let mut late = Vec::new();
    let geometry = match guest_addresses::geometry(state, owner, inputs) {
        Ok(geometry) => geometry,
        // Indices or positions that cannot be decoded yet: the title may
        // still be writing them. Decode at submission instead.
        Err(error) if error.code() == E_NOTIMPL => {
            let geometry = guest_addresses::geometry_with(state, owner, inputs, Some(&mut late))
                .map_err(|error| ("geometry", error.code()))?;
            if late.is_empty() {
                return Err(("geometry", E_NOTIMPL));
            }
            geometry
        }
        Err(error) => return Err(("geometry", error.code())),
    };
    let mut build = *entry;
    // The source is the console structure at the destination, which the host
    // does not read, and the title gives no scratch.
    build.SourceAccelerationStructureData = 0;
    if !geometry.is_empty() {
        build.Inputs.DescsLayout = D3D12_ELEMENTS_LAYOUT_ARRAY;
        build.Inputs.Anonymous.pGeometryDescs = geometry.as_ptr();
    }
    let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
    unsafe { device.GetRaytracingAccelerationStructurePrebuildInfo(&build.Inputs, &mut info) };
    if info.ResultDataMaxSizeInBytes == 0 {
        return Err(("host sizes", E_INVALIDARG));
    }
    Ok(Planned {
        build,
        _geometry: geometry,
        late,
        result: info.ResultDataMaxSizeInBytes,
        scratch: info.ScratchDataSizeInBytes.next_multiple_of(ALIGNMENT),
        extent,
    })
}

/// `XboxBvhBuilder`'s rehydration calls arrive here: `count` build
/// descriptions, each naming a console structure in place and the geometry it
/// was made from, and optionally each structure's address in another view.
/// Entries that cannot be built are left out one by one.
///
/// # Safety
/// `list` is a live TuxBox command list, and the arrays hold `count` entries
/// (`addresses` may be null).
#[unsafe(export_name = "TuxBoxRehydrateAccelerationStructures")]
pub unsafe extern "system" fn rehydrate(
    list: *const List,
    count: u32,
    builds: *const Build,
    addresses: *const u64,
) -> HRESULT {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    static BUILT: AtomicU64 = AtomicU64::new(0);
    static DEFERRED: AtomicU64 = AtomicU64::new(0);
    static SKIPPED: AtomicU64 = AtomicU64::new(0);
    if count == 0 {
        return S_OK;
    }
    let Some(list) = (unsafe { list.as_ref() }) else { return E_INVALIDARG };
    if builds.is_null() {
        return E_INVALIDARG;
    }
    let mut state = recording(list);
    if !state.prepare_state() {
        return state.error;
    }
    let Some(host) = state.raytracing().cloned() else { return E_NOTIMPL };
    let Some(owner) = &list.parent else { return E_INVALIDARG };
    let mut device = None::<ID3D12Device5>;
    if unsafe { host.GetDevice(&mut device) }.is_err() {
        return E_NOTIMPL;
    }
    let Some(device) = device else { return E_NOTIMPL };
    let entries = unsafe { core::slice::from_raw_parts(builds, count as usize) };
    let skip = |entry: &Build, reason: &str, code: HRESULT| {
        // Its own count, so other skipped commands cannot use up the log
        // budget before a rehydration's reason is seen.
        let skipped = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped <= 64 || skipped.is_power_of_two() {
            log(&format!(
                "skipped rehydration #{skipped} of the structure at {:#x}: {reason} ({:#010x}, flags={:#x} count={})",
                entry.DestAccelerationStructureData, code.0, entry.Inputs.Flags.0, entry.Inputs.NumDescs
            ));
        }
    };

    let mut planned = Vec::new();
    if planned.try_reserve_exact(entries.len()).is_err() {
        return E_OUTOFMEMORY;
    }
    for (index, entry) in entries.iter().enumerate() {
        let dest = entry.DestAccelerationStructureData;
        let alias = if addresses.is_null() { 0 } else { unsafe { addresses.add(index).read_unaligned() } };
        // The title names the structure by the address the GPU uses. The CPU
        // may only be able to read it through the other view.
        let used = header_at(dest).or_else(|| header_at(alias));
        match plan(&state, owner, &device, entry, extent(used)) {
            Ok(plan) => planned.push((entry, plan)),
            Err((reason, code)) => skip(entry, reason, code),
        }
    }

    let scratch_bytes: u64 = planned.iter().map(|(_, plan)| plan.scratch).sum();
    let scratch = if scratch_bytes == 0 {
        None
    } else {
        // SAFETY: `owner` is the list's controlling guest device.
        match unsafe { blas::scratch_buffer(owner, scratch_bytes) }.and_then(|buffer| {
            retain(&state, &[&buffer])?;
            Ok(buffer)
        }) {
            Ok(buffer) => Some(buffer),
            Err(error) => {
                for (entry, _) in &planned {
                    skip(entry, "scratch", error.code());
                }
                planned.clear();
                None
            }
        }
    };
    let mut scratch_at = scratch.as_ref().map_or(0, |buffer| unsafe { buffer.GetGPUVirtualAddress() });
    let (mut built, mut deferred) = (0u64, 0u64);
    for (entry, mut plan) in planned {
        let guest = entry.DestAccelerationStructureData;
        if state.scene_commands.try_reserve(2).is_err() {
            skip(entry, "memory", E_OUTOFMEMORY);
            continue;
        }
        // SAFETY: `owner` is the list's controlling guest device.
        let placed = unsafe { blas::replacement_destination(owner, guest, plan.extent, plan.result) }
            .and_then(|(storage, native)| {
                retain(&state, &[&storage])?;
                Ok(native)
            });
        let native = match placed {
            Ok(placed) => placed,
            Err(error) => {
                skip(entry, "destination", error.code());
                continue;
            }
        };
        plan.build.DestAccelerationStructureData = native;
        plan.build.ScratchAccelerationStructureData = if plan.scratch == 0 { 0 } else { scratch_at };
        scratch_at += plan.scratch;
        state.active.has_commands = true;
        state.record_unknown_write();
        let late = !plan.late.is_empty();
        unsafe { host.BuildRaytracingAccelerationStructure(&plan.build, None) };
        if late {
            deferred += 1;
            state.scene_commands.push(SceneCommand::Rehydrate(Deferred { dest: guest, late: core::mem::take(&mut plan.late) }));
        } else {
            built += 1;
        }
        state.scene_commands.push(SceneCommand::Native {
            guest,
            native,
            bytes: plan.result,
            // SAFETY: the title's descriptions are live for this call.
            build: unsafe { blas::NativeBuild::record(owner, guest, native, &entry.Inputs) },
        });
    }
    if built + deferred != 0 {
        // A scene built later in this list reads these structures.
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: core::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER { pResource: core::mem::ManuallyDrop::new(None) }),
            },
        };
        unsafe { host.ResourceBarrier(&[barrier]) };
    }
    let call = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let built_total = BUILT.fetch_add(built, Ordering::Relaxed) + built;
    let deferred_total = DEFERRED.fetch_add(deferred, Ordering::Relaxed) + deferred;
    if call <= 8 || call.is_power_of_two() {
        log(&format!(
            "rehydration call #{call}: {count} structures, {built} built from geometry read when recorded, {deferred} with geometry left for submission, {} skipped ({built_total}, {deferred_total} and {} so far)",
            u64::from(count) - built - deferred,
            SKIPPED.load(Ordering::Relaxed),
        ));
    }
    S_OK
}

#[cfg(test)]
#[path = "../../tests/commands/rehydrate.rs"]
mod tests;
