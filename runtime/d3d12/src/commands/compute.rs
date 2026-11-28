//! Compute commands on the Xbox graphics command list.
use super::{List, parent, recording};
use crate::device;
use core::ffi::c_void;
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_FLAGS,
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_FLAG_NONE,
};
use windows::core::HRESULT;

/// `COMPUTE_DISPATCH_INITIATOR__USE_THREAD_DIMENSIONS__SHIFT` (bit 5) in AMD
/// RDNA2 (`drivers/gpu/drm/amd/include/asic_reg/gc/gc_10_3_0_sh_mask.h`).
const DISPATCH_USE_THREAD_DIMENSIONS: u32 = 0x20;
/// `COMPUTE_DISPATCH_INITIATOR__CS_W32_EN__SHIFT` (bit 15). Wave width is static
/// in desktop DXIL; no dynamic wave-mode switch is needed at dispatch.
const DISPATCH_CS_W32_EN: u32 = 0x8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlaggedGroups {
    groups: [u32; 3],
    used_threads: bool,
    partial: bool,
}

fn resolve_flagged_groups(
    dims: [u32; 3],
    flags: u32,
    compute_threads: Option<[u32; 3]>,
    has_regular_pipeline: bool,
) -> Result<FlaggedGroups, HRESULT> {
    if flags & DISPATCH_USE_THREAD_DIMENSIONS == 0 {
        return Ok(FlaggedGroups {
            groups: dims,
            used_threads: false,
            partial: false,
        });
    }
    let Some(group_size) = compute_threads else {
        return Err(if has_regular_pipeline { E_NOTIMPL } else { E_INVALIDARG });
    };
    if group_size.contains(&0) {
        return Err(E_NOTIMPL);
    }
    let groups = [
        dims[0].div_ceil(group_size[0]),
        dims[1].div_ceil(group_size[1]),
        dims[2].div_ceil(group_size[2]),
    ];
    let partial = !dims.contains(&0) && (0..3).any(|i| !dims[i].is_multiple_of(group_size[i]));
    Ok(FlaggedGroups {
        groups,
        used_threads: true,
        partial,
    })
}

pub(super) unsafe extern "system" fn bind_root(list: &List, signature: *mut c_void) {
    let mut state = recording(list);
    if !state.prepare_state() {
        return;
    }
    // SAFETY: the guest holds its signature during this call. Only a native
    // interface belonging to this list's device is passed to the driver.
    match unsafe { device::compute::root_proxy(signature, parent(&list.parent)) } {
        Ok(root) => {
            let native = root.map(|r| &r.native);
            state.bindings.compute_root(native, root.map(|r| &r.state));
            unsafe { state.native().SetComputeRootSignature(native) };
        }
        Err(error) => state.error = error,
    }
}

pub(super) unsafe extern "system" fn one_constant<const COMPUTE: bool>(list: &List, index: u32, data: u32, offset: u32) {
    unsafe { constants::<COMPUTE>(list, index, 1, (&raw const data).cast(), offset) };
}

pub(super) unsafe extern "system" fn constants<const COMPUTE: bool>(
    list: &List,
    index: u32,
    count: u32,
    data: *const c_void,
    offset: u32,
) {
    let mut state = recording(list);
    if !state.prepare_state() || count == 0 {
        return;
    }
    if data.is_null() || offset.checked_add(count).is_none_or(|end| end > 64) {
        state.error = E_INVALIDARG;
        return;
    }
    // SAFETY: the caller supplies count readable DWORDs for this command.
    let words = unsafe { core::slice::from_raw_parts(data.cast(), count as usize) };
    if COMPUTE {
        state.bindings.constants(index, offset, words);
        unsafe { state.native().SetComputeRoot32BitConstants(index, count, data, offset); }
    } else {
        state.bindings.graphics_constants(index, offset, words);
        unsafe { state.native().SetGraphicsRoot32BitConstants(index, count, data, offset); }
    }
}

use super::compute_bindings::BufferView;

/// The flags a root view's buffer needs: a root UAV is written through, as
/// address UAVs are.
fn root_view_flags(view: &BufferView) -> D3D12_RESOURCE_FLAGS {
    match view {
        BufferView::Uav(_) => D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        BufferView::Cbv(_) | BufferView::Srv(_) => D3D12_RESOURCE_FLAG_NONE,
    }
}

fn root_buffer<const COMPUTE: bool>(list: &List, index: u32, guest: u64, view: fn(u64) -> BufferView) {
    let mut state = recording(list);
    if state.prepare_state() {
        // A desktop root CBV has no size: vkd3d binds min(host buffer end -
        // address, 64 KiB). A one-byte import can leave a short, odd range,
        // which NVIDIA rejects (Xid 13, SKEDCHECK23_CONSTANT_BUFFER_SIZE).
        // Import the whole 64 KiB window a shader can read when it is mapped.
        let cbv = matches!(view(0), BufferView::Cbv(_));
        let wide = cbv.then(|| unsafe {
            device::command_buffer_address(parent(&list.parent), guest, 65536)
        }).and_then(Result::ok);
        let address = match wide {
            Some((buffer, va)) => {
                if let Some(buffer) = buffer {
                    if let Err(e) = super::raytracing::retain(&mut state, &[&buffer]) { state.error = e.code(); return; }
                }
                va
            }
            None if !cbv && guest != 0 => {
                // A root SRV or UAV has no size either: map title memory well
                // past it (see `root_view_address`).
                let flags = root_view_flags(&view(0));
                let resolved = unsafe { device::root_view_address(parent(&list.parent), guest, flags) }.and_then(|(buffer, va)| {
                    if let Some(buffer) = buffer { super::raytracing::retain(&state, &[&buffer])?; }
                    Ok(va)
                });
                match resolved { Ok(va) => va, Err(e) => { state.error = e.code(); return; } }
            }
            None => {
                let Some(va) = address(list, &mut state, guest, 1) else { return };
                if cbv && guest != 0 {
                    static SHORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    let n = SHORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if n.is_power_of_two() {
                        crate::diagnostics::log_once(&std::sync::Once::new(), || format!(
                            "root CBV {guest:#x} has no mapped 64 KiB window; bound with a short range ({n} so far)"));
                    }
                }
                va
            }
        };
        let view = view(address);
        unsafe { view.apply::<COMPUTE>(state.native(), index) };
        state.bindings.buffer::<COMPUTE>(index, view);
    }
}

pub(super) unsafe extern "system" fn constant_buffer<const COMPUTE: bool>(list: &List, index: u32, guest: u64) {
    root_buffer::<COMPUTE>(list, index, guest, BufferView::Cbv);
}

pub(super) unsafe extern "system" fn shader_resource<const COMPUTE: bool>(list: &List, index: u32, guest: u64) {
    root_buffer::<COMPUTE>(list, index, guest, BufferView::Srv);
}

pub(super) unsafe extern "system" fn unordered_access<const COMPUTE: bool>(list: &List, index: u32, guest: u64) {
    root_buffer::<COMPUTE>(list, index, guest, BufferView::Uav);
}

pub(super) unsafe extern "system" fn async_compute_budget(list: &List, budget: *const [u8; 24]) {
    let mut state = recording(list);
    if !state.prepare_state() {
        return;
    }
    // SAFETY: the Xbox caller supplies a readable 24-byte record for this call.
    let Some(budget) = (unsafe { budget.as_ref() }) else {
        state.error = E_INVALIDARG;
        return;
    };
    // SetAsyncComputeBudgetInternal passes six zero DWORDs at RVA 0x5765a97.
    // Host queues already use their existing scheduling policy for this default.
    if *budget != [0; 24] {
        crate::diagnostics::recording_error(format_args!(
            "nonzero asynchronous compute budget {budget:02x?}"
        ));
        state.error = E_NOTIMPL;
        return;
    }
    static FIRST: std::sync::Once = std::sync::Once::new();
    crate::diagnostics::log_once(&FIRST, || {
        "accepted default asynchronous compute budget (list slot 96)".into()
    });
}

pub(super) unsafe extern "system" fn dispatch(list: &List, x: u32, y: u32, z: u32) {
    let mut state = recording(list);
    if !state.can_record() {
        return;
    }
    record_dispatch(list, &mut state, x, y, z);
}

pub(super) unsafe extern "system" fn dispatch_flagged(list: &List, x: u32, y: u32, z: u32, flags: u32) {
    let mut state = recording(list);
    if !state.can_record() {
        return;
    }
    if flags & !(DISPATCH_USE_THREAD_DIMENSIONS | DISPATCH_CS_W32_EN) != 0 {
        static UNKNOWN_FLAGS: std::sync::Once = std::sync::Once::new();
        crate::diagnostics::log_once(&UNKNOWN_FLAGS, || format!(
            "slot 80 dispatch observed unexpected COMPUTE_DISPATCH_INITIATOR flags {flags:#x}"
        ));
    }
    let compute_threads = state.bindings.compute_threads();
    let has_regular_pipeline = matches!(
        state.bindings.pipeline,
        Some(super::compute_bindings::Pipeline::Regular(..))
    );
    let resolved = match resolve_flagged_groups([x, y, z], flags, compute_threads, has_regular_pipeline) {
        Ok(resolved) => resolved,
        Err(error) => {
            crate::diagnostics::recording_error(format_args!(
                "slot 80 thread-dimension dispatch ({x},{y},{z}) flags={flags:#x} without known compute [numthreads]"
            ));
            state.error = error;
            return;
        }
    };
    let [gx, gy, gz] = resolved.groups;
    static FIRST: std::sync::Once = std::sync::Once::new();
    crate::diagnostics::log_once(&FIRST, || if let Some([nx, ny, nz]) = compute_threads.filter(|_| resolved.used_threads) {
        format!("slot 80 dispatch threads=({x},{y},{z}) numthreads=({nx},{ny},{nz}) -> groups=({gx},{gy},{gz}) flags={flags:#x}")
    } else {
        format!("slot 80 dispatch groups=({gx},{gy},{gz}) flags={flags:#x}")
    });
    if resolved.partial && let Some([nx, ny, nz]) = compute_threads {
        static PARTIAL: std::sync::Once = std::sync::Once::new();
        crate::diagnostics::log_once(&PARTIAL, || format!(
            "slot 80 thread-dimension dispatch ({x},{y},{z}) rounded up for [numthreads({nx},{ny},{nz})] to ({gx},{gy},{gz}); desktop Dispatch does not mask surplus threads in the boundary group"
        ));
    }
    record_dispatch(list, &mut state, gx, gy, gz);
}

fn record_dispatch(list: &List, state: &mut super::Recording, x: u32, y: u32, z: u32) {
    if x.max(y).max(z) > D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION {
        record_split_dispatch(list, state, [x, y, z]);
        return;
    }
    let null_usage = state.bindings.null_usage.clone();
    // SAFETY: the active native list remains open under its recording lock.
    state.compute_dispatch(list, |native| unsafe {
        native.Dispatch(x, y, z);
        if let Some(tag) = &null_usage {
            crate::diagnostics::display::sample_null_dispatch(tag, [x, y, z]);
        }
        static FIRST: std::sync::Once = std::sync::Once::new();
        crate::diagnostics::log_once(&FIRST, || format!("recorded Dispatch list={list:p} groups=({x},{y},{z})"));
    });
}

/// The most desktop dispatches one console dispatch is split into.
const MAX_SPLIT_PARTS: usize = 1024;

/// A console dispatch with more groups in a dimension than a desktop dispatch
/// allows, as several dispatches that each receive the console group ID they
/// start at (see `tuxbox_dxbc::group_base`). A pipeline that cannot take that
/// base fails the list, as the dispatch would on its own.
fn record_split_dispatch(list: &List, state: &mut super::Recording, groups: [u32; 3]) {
    let refuse = |state: &mut super::Recording, why: &str| {
        crate::diagnostics::recording_error(format_args!(
            "compute dispatch of {groups:?} groups exceeds 65,535 in a dimension and cannot be split: {why}"
        ));
        state.error = E_INVALIDARG;
    };
    let Some(parts) = tuxbox_dxbc::group_base::split(groups, D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION, MAX_SPLIT_PARTS) else {
        return refuse(state, &format!("it needs more than {MAX_SPLIT_PARTS} dispatches"));
    };
    if state.bindings.scene_root().is_some() {
        return refuse(state, "an inline ray-query pipeline's root does not take the group base");
    }
    let Some((pipeline, group_base)) = state.bindings.group_base() else {
        return refuse(state, "the pipeline does not run a translated console program");
    };
    let variant = match group_base.variant() {
        Ok(variant) => variant,
        Err(why) => return refuse(state, why),
    };
    let (split, root, slot) = (variant.pipeline.clone(), variant.root.clone(), variant.slot);
    let count = parts.len();
    state.bindings.split_root = Some(root);
    // SAFETY: the active native list remains open under its recording lock.
    state.compute_dispatch(list, move |native| unsafe {
        native.SetPipelineState(&split);
        for part in &parts {
            native.SetComputeRoot32BitConstants(slot, 3, part.base.as_ptr().cast(), 0);
            let [x, y, z] = part.groups;
            native.Dispatch(x, y, z);
        }
        native.SetPipelineState(&pipeline);
    });
    state.bindings.split_root = None;
    static FIRST: std::sync::Once = std::sync::Once::new();
    crate::diagnostics::log_once(&FIRST, || format!(
        "split compute dispatch list={list:p} groups={groups:?} into {count} dispatches"
    ));
}

pub(super) fn buffer_offset(resource: &device::Resource, offset: u64, bytes: u64) -> Option<u64> {
    let desc = unsafe { resource.native.GetDesc() };
    let (base, length) = resource.state.buffer_window().unwrap_or((0, desc.Width));
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER || offset.checked_add(bytes)? > length {
        return None;
    }
    base.checked_add(offset)
}

pub(super) fn address(list: &List, state: &mut super::Recording, guest: u64, bytes: u64) -> Option<u64> {
    if guest == 0 { return Some(0); }
    let result = unsafe { device::command_buffer_address(parent(&list.parent), guest, bytes.max(1)) }
        .and_then(|(buffer, va)| {
            if let Some(buffer) = buffer { super::raytracing::retain(state, &[&buffer])?; }
            Ok(va)
        });
    match result { Ok(va) => Some(va), Err(e) => { state.error = e.code(); None } }
}

pub(super) unsafe extern "system" fn copy_buffer(
    list: &List,
    dst: *mut c_void,
    dst_offset: u64,
    src: *mut c_void,
    src_offset: u64,
    bytes: u64,
) {
    let mut state = recording(list);
    if !state.can_record() {
        return;
    }
    let owner = parent(&list.parent);
    // SAFETY: resource validates each guest proxy and its owning device.
    let (Some(dst), Some(src)) = (unsafe { device::resource_proxy(dst, owner) }, unsafe {
        device::resource_proxy(src, owner)
    }) else {
        state.error = E_INVALIDARG;
        return;
    };
    let guest_dst_offset = dst_offset;
    let (Some(dst_offset), Some(src_offset)) = (buffer_offset(dst, dst_offset, bytes), buffer_offset(src, src_offset, bytes)) else {
        state.error = E_INVALIDARG;
        return;
    };
    if let Some(base) = dst.state.guest_address() {
        state.record_write(base.saturating_add(guest_dst_offset), bytes);
    } else {
        state.record_unknown_write();
    }
    // SAFETY: both native buffers belong to the list and contain the requested range.
    unsafe {
        state
            .native()
            .CopyBufferRegion(&dst.native, dst_offset, &src.native, src_offset, bytes)
    };
}

#[cfg(test)]
#[path = "../tests/commands/compute.rs"]
mod tests;
