//! Guest addresses in acceleration structure commands.
//!
//! Geometry, scratch, instance records and size reports can lie in a title's
//! own memory, which the host reaches only through an imported buffer, as for
//! any view of that memory. Native addresses pass through unchanged.
use super::super::{Recording, raytracing::retain};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::*};
use windows::core::{IUnknown, Interface, Result};

/// The native address of `bytes` at `address`. The list retains any import.
pub(in crate::commands) fn native(
    state: &Recording,
    owner: &IUnknown,
    address: u64,
    bytes: u64,
    flags: D3D12_RESOURCE_FLAGS,
) -> Result<u64> {
    Ok(imported(state, owner, address, bytes, flags)?.unwrap_or(address))
}

/// The native address of `bytes` at `address` if a buffer backs it there,
/// or None for an address the host already knows.
pub(in crate::commands) fn imported(
    state: &Recording,
    owner: &IUnknown,
    address: u64,
    bytes: u64,
    flags: D3D12_RESOURCE_FLAGS,
) -> Result<Option<u64>> {
    if address == 0 || bytes == 0 {
        return Ok(None);
    }
    let (buffer, native) = unsafe {
        crate::device::command_buffer_address_flags(owner.as_raw(), address, bytes, flags)
    }?;
    let Some(buffer) = buffer else { return Ok(None) };
    retain(state, &[&buffer])?;
    Ok(Some(native))
}

fn span(count: u64, stride: u64, element: u64) -> Option<u64> {
    if count == 0 {
        return Some(0);
    }
    (count - 1).checked_mul(stride)?.checked_add(element)
}

fn vertex_bytes(format: DXGI_FORMAT) -> u64 {
    match format {
        DXGI_FORMAT_R32G32B32_FLOAT => 12,
        DXGI_FORMAT_R32G32_FLOAT
        | DXGI_FORMAT_R16G16B16A16_FLOAT
        | DXGI_FORMAT_R16G16B16A16_SNORM
        | DXGI_FORMAT_R16G16B16A16_UNORM => 8,
        DXGI_FORMAT_R8G8_UNORM | DXGI_FORMAT_R8G8_SNORM => 2,
        _ => 4,
    }
}

/// Bottom-level geometry with every buffer address made native. The build
/// must be recorded while the returned descriptions are alive.
pub(super) fn geometry(
    state: &Recording,
    owner: &IUnknown,
    inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
) -> Result<Vec<D3D12_RAYTRACING_GEOMETRY_DESC>> {
    geometry_with(state, owner, inputs, None)
}

/// A geometry whose coded indices or packed positions are decoded when the
/// list is submitted, into upload buffers the recorded build already names.
/// Until then the buffers are zero, which the host builds as no triangles.
pub(in crate::commands) struct Late {
    /// The title's stream, its index count and the R32_UINT upload.
    indices: Option<(u64, u32, ID3D12Resource)>,
    /// The title's records, their stride's low 14 bits and the R32G32B32_FLOAT upload.
    positions: Option<(u64, u64, ID3D12Resource)>,
    /// Vertices the recorded build may name.
    vertices: u32,
}

impl Late {
    /// Decode from title memory as it is now. Nothing is written unless
    /// everything decodes.
    pub(in crate::commands) fn decode(&self, owner: &IUnknown) -> core::result::Result<(), &'static str> {
        let title = |address, bytes| {
            // SAFETY: `owner` is the list's controlling guest device.
            unsafe { crate::device::command_buffer_address_flags(owner.as_raw(), address, bytes, D3D12_RESOURCE_FLAG_NONE) }
                .is_ok_and(|(buffer, _)| buffer.is_some())
        };
        let mut vertices = self.vertices;
        let mut indices = None;
        if let Some((address, count, buffer)) = &self.indices {
            if !title(*address, 12) {
                return Err("indices are not title memory");
            }
            let decoded = super::meshlet_indices::at(*address, *count).ok_or("indices do not decode")?;
            let used = decoded.iter().max().map_or(0, |last| last + 1);
            if used > vertices {
                return Err("indices pass the vertex count");
            }
            vertices = used;
            indices = Some((buffer, decoded));
        }
        let mut positions = None;
        if let Some((address, stride, buffer)) = &self.positions {
            let header = address.checked_sub(super::packed_positions::HEADER).ok_or("no position header")?;
            if !title(header, super::packed_positions::HEADER) {
                return Err("positions are not title memory");
            }
            let decoded = super::packed_positions::at(*address, vertices, *stride).ok_or("positions do not decode")?;
            positions = Some((buffer, decoded));
        }
        if let Some((buffer, decoded)) = indices {
            let bytes: Vec<u8> = decoded.iter().flat_map(|index| index.to_le_bytes()).collect();
            crate::device::blas::fill_upload(buffer, &bytes).map_err(|_| "index upload")?;
        }
        if let Some((buffer, decoded)) = positions {
            let bytes: Vec<u8> = decoded.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
            crate::device::blas::fill_upload(buffer, &bytes).map_err(|_| "position upload")?;
        }
        Ok(())
    }
}

/// A zeroed upload buffer of `bytes` that the list retains, and its address.
fn late_upload(state: &Recording, owner: &IUnknown, bytes: u64) -> Result<(ID3D12Resource, u64)> {
    if bytes == 0 {
        return Err(E_NOTIMPL.into());
    }
    // SAFETY: `owner` is the list's controlling guest device.
    let buffer = unsafe { crate::device::blas::instance_buffer(owner, bytes) }?;
    retain(state, &[&buffer])?;
    let address = unsafe { buffer.GetGPUVirtualAddress() };
    Ok((buffer, address))
}

/// As `geometry`. With `late`, coded indices and packed positions are not
/// read now: each such geometry gets zeroed uploads sized by the title's
/// counts and an entry in `late` that fills them at submission.
pub(super) fn geometry_with(
    state: &Recording,
    owner: &IUnknown,
    inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    mut late: Option<&mut Vec<Late>>,
) -> Result<Vec<D3D12_RAYTRACING_GEOMETRY_DESC>> {
    let count = inputs.NumDescs as usize;
    let mut descs = Vec::new();
    descs.try_reserve_exact(count).map_err(|_| E_OUTOFMEMORY)?;
    for index in 0..count {
        // SAFETY: `ray_build::valid` checked the layout and every pointer.
        let mut geometry = unsafe {
            if inputs.DescsLayout == D3D12_ELEMENTS_LAYOUT_ARRAY {
                *inputs.Anonymous.pGeometryDescs.add(index)
            } else {
                *(*inputs.Anonymous.ppGeometryDescs.add(index)).as_ref().ok_or(E_INVALIDARG)?
            }
        };
        match geometry.Type {
            D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES => {
                // SAFETY: the descriptor's discriminant selects Triangles.
                let triangles = unsafe { &mut geometry.Anonymous.Triangles };
                // Indices in a format the host cannot read are decoded first:
                // the vertex range is then sized by the largest of them.
                let decoded = triangles.IndexFormat.0 as u32 == super::meshlet_indices::FORMAT;
                let mut pending = Late { indices: None, positions: None, vertices: triangles.VertexCount };
                if decoded && late.is_some() {
                    // A meshlet lists its vertices as 16-bit indices, so the
                    // title's count, which can be far too large, is cut there.
                    triangles.VertexCount = triangles.VertexCount.min(1 << 16);
                    pending.vertices = triangles.VertexCount;
                    let (buffer, address) = late_upload(state, owner, u64::from(triangles.IndexCount) * 4)?;
                    pending.indices = Some((triangles.IndexBuffer, triangles.IndexCount, buffer));
                    triangles.IndexBuffer = address;
                    triangles.IndexFormat = DXGI_FORMAT_R32_UINT;
                } else if decoded {
                    // Only a stream in title memory can be read on the CPU at its address.
                    imported(state, owner, triangles.IndexBuffer, 12, D3D12_RESOURCE_FLAG_NONE)?.ok_or(E_NOTIMPL)?;
                    let indices = super::meshlet_indices::at(triangles.IndexBuffer, triangles.IndexCount)
                        .ok_or(E_NOTIMPL)?;
                    let bytes: Vec<u8> = indices.iter().flat_map(|index| index.to_le_bytes()).collect();
                    // SAFETY: `owner` is the list's controlling guest device.
                    let buffer = unsafe { crate::device::blas::geometry_upload(owner, &bytes) }?;
                    retain(state, &[&buffer])?;
                    triangles.IndexBuffer = unsafe { buffer.GetGPUVirtualAddress() };
                    triangles.IndexFormat = DXGI_FORMAT_R32_UINT;
                    triangles.VertexCount = indices.iter().max().map_or(0, |last| last + 1);
                }
                let vertices = &mut triangles.VertexBuffer;
                let bytes = span(triangles.VertexCount.into(), vertices.StrideInBytes, vertex_bytes(triangles.VertexFormat))
                    .ok_or(E_INVALIDARG)?;
                let packed = triangles.VertexFormat == DXGI_FORMAT_R32G32_FLOAT && triangles.VertexCount != 0;
                if packed && late.is_some() {
                    let (buffer, address) = late_upload(state, owner, u64::from(triangles.VertexCount) * 12)?;
                    // Kept masked, as `packed_positions::at` takes it.
                    let stride = super::packed_positions::stride(vertices.StrideInBytes);
                    pending.positions = Some((vertices.StartAddress, stride, buffer));
                    vertices.StartAddress = address;
                    vertices.StrideInBytes = 12;
                    triangles.VertexFormat = DXGI_FORMAT_R32G32B32_FLOAT;
                } else if packed {
                    // The console builder reads this format as packed records
                    // with a header before them; both must be title memory.
                    // The title's stride carries high bits the builder ignores.
                    let stride = super::packed_positions::stride(vertices.StrideInBytes);
                    let bytes = span(triangles.VertexCount.into(), stride, vertex_bytes(triangles.VertexFormat))
                        .ok_or(E_INVALIDARG)?;
                    let header = vertices.StartAddress.checked_sub(super::packed_positions::HEADER).ok_or(E_NOTIMPL)?;
                    let whole = bytes.checked_add(super::packed_positions::HEADER).ok_or(E_INVALIDARG)?;
                    imported(state, owner, header, whole, D3D12_RESOURCE_FLAG_NONE)?.ok_or(E_NOTIMPL)?;
                    let positions =
                        super::packed_positions::at(vertices.StartAddress, triangles.VertexCount, stride)
                            .ok_or(E_NOTIMPL)?;
                    let bytes: Vec<u8> = positions.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
                    // SAFETY: `owner` is the list's controlling guest device.
                    let buffer = unsafe { crate::device::blas::geometry_upload(owner, &bytes) }?;
                    retain(state, &[&buffer])?;
                    vertices.StartAddress = unsafe { buffer.GetGPUVirtualAddress() };
                    vertices.StrideInBytes = 12;
                    triangles.VertexFormat = DXGI_FORMAT_R32G32B32_FLOAT;
                } else {
                    vertices.StartAddress = native(state, owner, vertices.StartAddress, bytes, D3D12_RESOURCE_FLAG_NONE)?;
                }
                let index_bytes = match triangles.IndexFormat {
                    DXGI_FORMAT_UNKNOWN => 0,
                    DXGI_FORMAT_R16_UINT => 2,
                    DXGI_FORMAT_R32_UINT => 4,
                    // Sized as nothing, the title's address would reach the
                    // host build as an index buffer.
                    _ => return Err(E_NOTIMPL.into()),
                };
                if !decoded {
                    let bytes = u64::from(triangles.IndexCount) * index_bytes;
                    triangles.IndexBuffer = native(state, owner, triangles.IndexBuffer, bytes, D3D12_RESOURCE_FLAG_NONE)?;
                }
                triangles.Transform3x4 = native(state, owner, triangles.Transform3x4, 48, D3D12_RESOURCE_FLAG_NONE)?;
                if let Some(late) = late.as_deref_mut()
                    && (pending.indices.is_some() || pending.positions.is_some())
                {
                    late.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
                    late.push(pending);
                }
            }
            D3D12_RAYTRACING_GEOMETRY_TYPE_PROCEDURAL_PRIMITIVE_AABBS => {
                // SAFETY: the descriptor's discriminant selects AABBs.
                let boxes = unsafe { &mut geometry.Anonymous.AABBs };
                let bytes = span(boxes.AABBCount, boxes.AABBs.StrideInBytes, 24).ok_or(E_INVALIDARG)?;
                boxes.AABBs.StartAddress = native(state, owner, boxes.AABBs.StartAddress, bytes, D3D12_RESOURCE_FLAG_NONE)?;
            }
            _ => return Err(E_NOTIMPL.into()),
        }
        descs.push(geometry);
    }
    Ok(descs)
}

/// Size reports written to native addresses. Each report is 8 bytes per structure.
pub(in crate::commands) fn reports(
    state: &Recording,
    owner: &IUnknown,
    outputs: &mut [D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC],
    structures: u64,
) -> Result<()> {
    for output in outputs {
        let flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        output.DestBuffer = native(state, owner, output.DestBuffer, structures * 8, flags)?;
    }
    Ok(())
}
