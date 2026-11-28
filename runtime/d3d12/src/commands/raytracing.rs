//! Xbox list slot 119 records AS copies and deserializes cooked console geometry.
use super::{List, Recording, recording};
use crate::device::blas;
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{IUnknown, Result};

// Replay geometry publications in GPU command order. Upload-backed BLAS
// vertices, indices, transforms and cooked payloads can all change after Close.
pub(super) enum SceneCommand {
    Native {
        guest: u64,
        native: u64,
        build: blas::NativeBuild,
        bytes: u64,
    },
    Fixup(super::build_raytracing::Instances),
    Source {
        guest: u64,
        native: u64,
    },
    Cooked(blas::Input),
    Rehydrate(super::build_raytracing::rehydrate::Deferred),
    Instances {
        guest: u64,
        native: u64,
        records: Option<ID3D12Resource>,
        bytes: u64,
    },
    Copy {
        guest: u64,
        native: u64,
        source_guest: u64,
        source_native: u64,
        capacity: Option<u64>,
    },
    Check(super::compute_bindings::SceneCheck),
}

impl SceneCommand {
    pub(super) fn prepare(&self, list: &List) -> Result<()> {
        let owner = list.parent.as_ref().ok_or(E_INVALIDARG)?;
        unsafe {
            match self {
                Self::Native {
                    guest,
                    native,
                    build,
                    bytes,
                } => {
                    build.prepare(owner);
                    blas::publish_destination(owner, *guest, *native, Some(*bytes));
                }
                Self::Fixup(instances) => instances.prepare(owner)?,
                Self::Cooked(input) => input.prepare(owner)?,
                Self::Rehydrate(deferred) => deferred.prepare(owner),
                Self::Instances {
                    guest,
                    native,
                    records,
                    bytes,
                } => {
                    blas::set_inline_instances(owner, *guest, *native, records.clone());
                    blas::publish_destination(owner, *guest, *native, Some(*bytes));
                }
                Self::Source { guest, native } => {
                    validate_source(owner, *guest, *native)?;
                }
                Self::Copy {
                    guest,
                    native,
                    source_guest,
                    source_native,
                    capacity,
                } => {
                    // A replaced source's size belongs to its successor. The
                    // destination was sized for the storage the list copies.
                    let bytes = match validate_source(owner, *source_guest, *source_native)? {
                        Source::Current => blas::submitted_size(owner, *source_guest, *source_native),
                        Source::Replaced => *capacity,
                    };
                    if let Some(capacity) = capacity
                        && bytes.is_none_or(|needed| needed > *capacity)
                    {
                        static FIRST: std::sync::Once = std::sync::Once::new();
                        crate::diagnostics::log_once(&FIRST, || {
                            format!(
                                "AS copy {source_guest:#x} ({source_native:#x}) to {guest:#x} ({native:#x}): submitted source bound {bytes:?}, private capacity {capacity}"
                            )
                        });
                        return Err(E_NOTIMPL.into());
                    }
                    blas::copy_geometry(owner, *guest, *native, *source_native);
                    blas::publish_destination(owner, *guest, *native, bytes);
                }
                Self::Check(check) => check.validate(list).inspect_err(|_| {
                    static FIRST: std::sync::Once = std::sync::Once::new();
                    crate::diagnostics::log_once(&FIRST, || {
                        "inline ray scene or geometry changed after recording, rejecting submission"
                            .into()
                    });
                })?,
            }
        }
        Ok(())
    }
}

/// What a list's recorded source address still names.
#[derive(Debug, PartialEq)]
enum Source {
    Current,
    /// A later recording gave the title's address new storage. The list keeps
    /// the storage it recorded, which still holds the structure it meant.
    Replaced,
}

/// Decide whether a list that recorded `recorded` for the structure at `guest`
/// may run now that the address resolves to `submitted`.
///
/// Storage in a title's own memory is published when a build is recorded, so a
/// later recording can replace it before an earlier list is submitted. Refusing
/// that list would stop its queue for good, and every label the queue was to
/// write. A raw guest address is refused: the host cannot read a structure there.
fn source_state(
    guest: u64,
    recorded: u64,
    submitted: u64,
    published_when_recorded: impl FnOnce() -> bool,
) -> Option<Source> {
    if submitted == recorded {
        Some(Source::Current)
    } else if recorded != guest && published_when_recorded() {
        Some(Source::Replaced)
    } else {
        None
    }
}

fn validate_source(owner: &IUnknown, guest: u64, recorded: u64) -> Result<Source> {
    let submitted = unsafe { blas::submitted_address(owner, guest) };
    let state = source_state(guest, recorded, submitted, || unsafe {
        blas::published_when_recorded(owner, guest)
    });
    match state {
        Some(Source::Replaced) => {
            static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if count <= 8 || count.is_power_of_two() {
                crate::diagnostics::log(&format!(
                    "AS source {guest:#x} was given new storage after a list recorded it (#{count}): the list keeps {recorded:#x}, the address now resolves to {submitted:#x}"
                ));
            }
        }
        Some(Source::Current) => {}
        None => {
            crate::diagnostics::recording_error(format_args!(
                "recorded AS source {guest:#x} used {recorded:#x}, now resolves to {submitted:#x}"
            ));
            return Err(E_INVALIDARG.into());
        }
    }
    Ok(state.unwrap_or(Source::Current))
}

pub(super) fn retain(recording: &Recording, resources: &[&ID3D12Resource]) -> Result<()> {
    let mut held = recording
        .resources
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    held.try_reserve(resources.len())
        .map_err(|_| E_OUTOFMEMORY)?;
    held.extend(resources.iter().map(|r| ID3D12Pageable::from((*r).clone())));
    Ok(())
}

fn record(
    state: &mut Recording,
    host: &ID3D12GraphicsCommandList4,
    owner: &IUnknown,
    destination: u64,
    source: u64,
    mode: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE,
) -> Result<()> {
    let alignment = u64::from(D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BYTE_ALIGNMENT);
    if source == 0
        || destination == 0
        || !source.is_multiple_of(alignment)
        || !destination.is_multiple_of(alignment)
    {
        return Err(E_INVALIDARG.into());
    }
    match mode {
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE
        | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT => {
            // SAFETY: owner is the controlling device retained by the list.
            let resolve = |address, role| {
                unsafe { blas::resolve(owner, address) }.inspect_err(|err| {
                static FIRST: std::sync::Once = std::sync::Once::new();
                crate::diagnostics::log_once(&FIRST, || format!(
                    "AS copy {source:#x} to {destination:#x}: {role} registration at {address:#x}: {:?}", err.code()
                ));
            })
            };
            let source_as = resolve(source, "source")?;
            let destination_as = resolve(destination, "destination")?;
            if source == destination {
                return Err(E_INVALIDARG.into());
            }
            let (src, dst, capacity) = if let Some(backing) = source_as {
                let (dst, va) = unsafe { blas::copy_dest(owner, destination, &backing) }?;
                retain(state, &[&backing, &dst])?;
                (
                    unsafe { backing.GetGPUVirtualAddress() },
                    va,
                    Some(unsafe { dst.GetDesc() }.Width),
                )
            } else if let Some(backing) = destination_as {
                let direct = unsafe { blas::direct_source(owner, source) }.map_err(|err| {
                    static FIRST: std::sync::Once = std::sync::Once::new();
                    crate::diagnostics::log_once(&FIRST, || format!(
                        "AS copy {source:#x} to relocated {destination:#x}: no unique registered native source, {:?}", err.code()
                    ));
                    if err.code() == E_INVALIDARG { windows::core::Error::from(E_NOTIMPL) } else { err }
                })?;
                retain(state, &[&direct, &backing])?;
                (
                    source,
                    unsafe { backing.GetGPUVirtualAddress() },
                    Some(unsafe { backing.GetDesc() }.Width),
                )
            } else {
                (source, destination, None)
            };
            state
                .scene_commands
                .try_reserve(1)
                .map_err(|_| E_OUTOFMEMORY)?;
            unsafe {
                host.CopyRaytracingAccelerationStructure(dst, src, mode);
                blas::copy_geometry(owner, destination, dst, src);
            }
            state.scene_commands.push(SceneCommand::Copy {
                guest: destination,
                native: dst,
                source_guest: source,
                source_native: src,
                capacity,
            });
        }
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_DESERIALIZE => {
            state
                .scene_commands
                .try_reserve(1)
                .map_err(|_| E_OUTOFMEMORY)?;
            let result = unsafe { blas::deserialize(owner, source, destination) }?;
            retain(state, &[&result.source, &result.destination])?;
            state
                .scene_commands
                .push(SceneCommand::Cooked(result.input));
            // SAFETY: the private build is complete, the copy fits the checked
            // guest suballocation or its dedicated backing. The guest allocator
            // retains both resources until all executions using it have completed.
            unsafe {
                host.CopyRaytracingAccelerationStructure(
                    result.destination_address,
                    result.source.GetGPUVirtualAddress(),
                    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT,
                )
            };
        }
        _ => return Err(E_NOTIMPL.into()),
    }
    Ok(())
}

pub(super) unsafe extern "system" fn copy(
    list: &List,
    destination: u64,
    source: u64,
    mode: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE,
) {
    let mut state = recording(list);
    if !state.can_record() {
        return;
    }
    let Some(host) = state.raytracing().cloned() else {
        state.error = E_NOTIMPL;
        return;
    };
    let Some(owner) = &list.parent else {
        state.error = E_POINTER;
        return;
    };
    if let Err(error) = record(&mut state, &host, owner, destination, source, mode) {
        crate::diagnostics::recording_error(format_args!(
            "slot 119 AS copy mode {}, source {source:#x}, destination {destination:#x}: {:?}",
            mode.0,
            error.code()
        ));
        state.error = error.code();
    } else {
        state.record_unknown_write();
    }
}

pub(super) fn validate_size_output(
    desc: &D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC,
) -> Result<()> {
    if !matches!(
        desc.InfoType,
        D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_CURRENT_SIZE
    ) {
        return Err(E_NOTIMPL.into());
    }
    if desc.DestBuffer == 0 || !desc.DestBuffer.is_multiple_of(8) {
        return Err(E_INVALIDARG.into());
    }
    Ok(())
}

// FD3D12RayTracingCompactionRequestHandler::Update passes a destination,
// count and array of BLAS GPU addresses to Xbox list slot 118 (RVA 0x57a95d5).
pub(super) unsafe extern "system" fn postbuild(
    list: &List,
    desc: *const D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC,
    count: u32,
    addresses: *const u64,
) {
    let mut state = recording(list);
    if !state.can_record() {
        return;
    }
    let result = (|| -> Result<()> {
        let desc = unsafe { desc.as_ref() }.ok_or(E_POINTER)?;
        let host = state.raytracing().cloned().ok_or(E_NOTIMPL)?;
        let owner = list.parent.as_ref().ok_or(E_POINTER)?;
        validate_size_output(desc)?;
        if count == 0 {
            return Ok(());
        }
        if addresses.is_null() {
            return Err(E_POINTER.into());
        }
        let sources = unsafe { std::slice::from_raw_parts(addresses, count as usize) };
        let alignment = u64::from(D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BYTE_ALIGNMENT);
        if sources
            .iter()
            .any(|&va| va == 0 || !va.is_multiple_of(alignment))
        {
            return Err(E_INVALIDARG.into());
        }
        let mut patched = Vec::new();
        patched
            .try_reserve_exact(sources.len())
            .map_err(|_| E_OUTOFMEMORY)?;
        state
            .scene_commands
            .try_reserve(sources.len())
            .map_err(|_| E_OUTOFMEMORY)?;
        for &guest in sources {
            let native = if let Some(target) = unsafe { blas::resolve(owner, guest) }? {
                retain(&state, &[&target])?;
                unsafe { target.GetGPUVirtualAddress() }
            } else {
                guest
            };
            patched.push(native);
            state
                .scene_commands
                .push(SceneCommand::Source { guest, native });
        }
        let mut output = *desc;
        output.DestBuffer = super::build_raytracing::guest_addresses::native(
            &state,
            owner,
            desc.DestBuffer,
            u64::from(count).saturating_mul(8),
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        )?;
        state.record_write(desc.DestBuffer, u64::from(count).saturating_mul(8));
        unsafe { host.EmitRaytracingAccelerationStructurePostbuildInfo(&output, &patched) };
        Ok(())
    })();
    if let Err(error) = result {
        state.error = error.code();
        crate::diagnostics::recording_error(format_args!(
            "slot 118 AS size query: {:?}",
            error.code()
        ));
    }
}

#[cfg(test)]
#[path = "../tests/commands/replaced_source.rs"]
mod tests;
