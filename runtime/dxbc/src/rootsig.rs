//! Serialized Direct3D 12 root signatures (RTS0 versions 1.0 and 1.1).
//!
//! Layouts follow the [DXC serialized structures](https://github.com/microsoft/DirectXShaderCompiler/blob/main/include/dxc/DxilRootSignature/DxilRootSignature.h)
//! and [LLVM's RTS0 description](https://llvm.org/docs/DirectX/DXContainer.html#root-signature-rts0-part).
//! All offsets in the payload are byte offsets from the version field.

// Serialized descriptor tables can share ranges. Each decoded table owns a copy.
const MAX_DECODED_BYTES: usize = 64 << 20;

#[derive(Clone, Debug)]
pub struct RootRange {
    pub range_type: u32,
    pub num: u32,
    pub base_register: u32,
    pub space: u32,
    pub offset_in_table: u32,
}

#[derive(Clone, Debug)]
pub enum RootParamKind {
    Table(Vec<RootRange>),
    Constants {
        register: u32,
        space: u32,
        num32: u32,
    },
    Descriptor {
        kind: u32,
        register: u32,
        space: u32,
    },
}

#[derive(Clone, Debug)]
pub struct RootParam {
    pub visibility: u32,
    pub kind: RootParamKind,
}

#[derive(Clone, Debug)]
pub struct RootSig {
    pub version: u32,
    pub flags: u32,
    pub params: Vec<RootParam>,
    pub num_static_samplers: u32,
    pub static_samplers: Vec<[u32; 13]>,
}

fn words<const N: usize>(mut record: &[u8]) -> Result<[u32; N], String> {
    let mut values = [0; N];
    for value in &mut values {
        let (word, rest) = record
            .split_first_chunk::<4>()
            .ok_or("truncated RTS0 record")?;
        *value = u32::from_le_bytes(*word);
        record = rest;
    }
    Ok(values)
}

fn section(body: &[u8], offset: u32, count: u32, stride: usize) -> Result<&[u8], String> {
    if count != 0 && offset < 24 {
        return Err("RTS0 section overlaps its header".into());
    }
    let start = offset as usize;
    let end = (count as usize)
        .checked_mul(stride)
        .and_then(|n| start.checked_add(n))
        .ok_or("RTS0 section size overflow")?;
    body.get(start..end)
        .ok_or_else(|| "RTS0 section out of bounds".into())
}

fn charge_records<T>(remaining: &mut usize, count: u32) -> Result<(), String> {
    *remaining = (count as usize)
        .checked_mul(std::mem::size_of::<T>())
        .and_then(|bytes| remaining.checked_sub(bytes))
        .ok_or("RTS0 records exceed decoded size limit")?;
    Ok(())
}

fn read_table(
    body: &[u8],
    offset: u32,
    v11: bool,
    remaining: &mut usize,
) -> Result<RootParamKind, String> {
    let [count, ranges_offset] = words(section(body, offset, 1, 8)?)?;
    let stride = if v11 { 24 } else { 20 };
    let records = section(body, ranges_offset, count, stride)?;
    charge_records::<RootRange>(remaining, count)?;
    let mut ranges = Vec::with_capacity(count as usize);
    for row in records.chunks_exact(stride) {
        let [range_type, num, base_register, space] = words(row)?;
        // Version 1.1 inserts range flags before the table-relative offset.
        let [offset_in_table] = words(&row[stride - 4..])?;
        ranges.push(RootRange {
            range_type,
            num,
            base_register,
            space,
            offset_in_table,
        });
    }
    Ok(RootParamKind::Table(ranges))
}

/// Reads binding locations, visibility, and the raw words of static samplers.
///
/// # Errors
///
/// Returns an error for an unsupported version or parameter type, a truncated
/// record, a section extending beyond the supplied RTS0 payload, or decoded
/// parameters, ranges and static samplers exceeding 64 MiB in total.
pub fn parse_rts0(body: &[u8]) -> Result<RootSig, String> {
    parse_rts0_with_limit(body, MAX_DECODED_BYTES)
}

fn parse_rts0_with_limit(body: &[u8], mut remaining: usize) -> Result<RootSig, String> {
    let [
        version,
        num_params,
        params_offset,
        num_static_samplers,
        sampler_offset,
        flags,
    ] = words(body)?;
    let v11 = match version {
        1 => false,
        2 => true,
        _ => return Err(format!("unsupported RTS0 version {version}")),
    };
    let headers = section(body, params_offset, num_params, 12)?;
    let samplers = section(body, sampler_offset, num_static_samplers, 52)?;
    charge_records::<RootParam>(&mut remaining, num_params)?;
    charge_records::<[u32; 13]>(&mut remaining, num_static_samplers)?;
    let mut params = Vec::with_capacity(num_params as usize);
    for header in headers.chunks_exact(12) {
        let [tag, visibility, offset] = words(header)?;
        let kind = match tag {
            0 => read_table(body, offset, v11, &mut remaining)?,
            1 => {
                let [register, space, num32] = words(section(body, offset, 1, 12)?)?;
                RootParamKind::Constants {
                    register,
                    space,
                    num32,
                }
            }
            2..=4 => {
                // Version 1.1 appends a flags word after the register space.
                let stride = if v11 { 12 } else { 8 };
                let [register, space] = words(section(body, offset, 1, stride)?)?;
                RootParamKind::Descriptor {
                    kind: tag,
                    register,
                    space,
                }
            }
            _ => return Err(format!("unknown RTS0 parameter type {tag}")),
        };
        params.push(RootParam { visibility, kind });
    }
    let mut static_samplers = Vec::with_capacity(num_static_samplers as usize);
    for sampler in samplers.chunks_exact(52) {
        static_samplers.push(words(sampler)?);
    }
    Ok(RootSig {
        version,
        flags,
        params,
        num_static_samplers,
        static_samplers,
    })
}

impl RootSig {
    /// Whether a direct-indexed root can append the two-DWORD map after any
    /// extra descriptor tables (for example mesh pipeline pixel mirrors).
    pub fn heap_index_map_fits(&self, extra_tables: usize) -> bool {
        let cost = self.params.iter().fold(extra_tables as u64, |cost, param| cost.saturating_add(
            match param.kind {
                RootParamKind::Table(_) => 1,
                RootParamKind::Constants { num32, .. } => u64::from(num32),
                RootParamKind::Descriptor { .. } => 2,
            },
        ));
        self.flags & 0x400 != 0 && cost <= 62
    }

    /// Finite ranges that can address textures from the CBV/SRV/UAV heap.
    /// Unbounded tables and direct resource heap indexing require the full heap.
    pub fn placed_texture_tables(&self) -> Option<Vec<(u32, std::ops::Range<u32>)>> {
        const RESOURCE_HEAP_DIRECTLY_INDEXED: u32 = 0x400;
        if self.flags & RESOURCE_HEAP_DIRECTLY_INDEXED != 0 {
            return None;
        }
        let mut tables = Vec::new();
        for (index, param) in self.params.iter().enumerate() {
            let RootParamKind::Table(ranges) = &param.kind else {
                continue;
            };
            let mut next = 0;
            for range in ranges {
                if range.num == u32::MAX {
                    return None;
                }
                let start = if range.offset_in_table == u32::MAX {
                    next
                } else {
                    range.offset_in_table
                };
                next = start.checked_add(range.num)?;
                match range.range_type {
                    0 | 1 if start < next => tables.push((index as u32, start..next)),
                    0..=3 => {} // SRV, UAV, CBV, sampler.
                    _ => return None,
                }
            }
        }
        Some(tables)
    }
}

/// Append TuxBox's resource heap index map without a platform serializer.
/// Existing records retain their offsets and bytes. Version 1.0 records are
/// copied to 1.1 records so the new SRV can carry DATA_VOLATILE; their implicit
/// volatile range/descriptor semantics are preserved.
pub fn append_heap_index_map(body: &[u8]) -> Result<Vec<u8>, String> {
    parse_rts0(body)?;
    let [version, count, offset, _, _, _] = words::<6>(body)?;
    let mut out = body.to_vec();
    while out.len() % 4 != 0 { out.push(0); }
    let mut headers = section(body, offset, count, 12)?.to_vec();
    if version == 1 {
        for header in headers.chunks_exact_mut(12) {
            let [tag, _, at] = words::<3>(header)?;
            let record = match tag {
                0 => {
                    let [n, at] = words::<2>(section(body, at, 1, 8)?)?;
                    let ranges_at = out.len() as u32;
                    for row in section(body, at, n, 20)?.chunks_exact(20) {
                        out.extend_from_slice(&row[..16]);
                        let flags: u32 = if words::<1>(row)?[0] == 3 { 1 } else { 3 };
                        out.extend_from_slice(&flags.to_le_bytes());
                        out.extend_from_slice(&row[16..]);
                    }
                    Some([n, ranges_at].into_iter().flat_map(u32::to_le_bytes).collect::<Vec<_>>())
                }
                2..=4 => {
                    let mut record = section(body, at, 1, 8)?.to_vec();
                    record.extend_from_slice(&2u32.to_le_bytes());
                    Some(record)
                }
                _ => None,
            };
            if let Some(record) = record {
                header[8..12].copy_from_slice(&(out.len() as u32).to_le_bytes());
                out.extend(record);
            }
        }
    }
    let descriptor = out.len() as u32;
    out.extend([0u32, 32766, 2].into_iter().flat_map(u32::to_le_bytes));
    headers.extend([3u32, 0, descriptor].into_iter().flat_map(u32::to_le_bytes));
    let headers_at = out.len() as u32;
    out.extend(headers);
    out[..4].copy_from_slice(&2u32.to_le_bytes());
    out[4..8].copy_from_slice(&count.checked_add(1).ok_or("RTS0 parameter count overflow")?.to_le_bytes());
    out[8..12].copy_from_slice(&headers_at.to_le_bytes());
    Ok(out)
}

/// A program's translation request with the resource heap index map appended
/// to the root it carries (see `append_heap_index_map`). It is derived from the
/// request on the declared root rather than rebuilt, so that a tool can derive
/// it byte for byte from the requests a launch recorded. Other parts keep their
/// bytes and directory order, later parts move by a multiple of four bytes, and
/// the digest is cleared, as when the runtime gives a program its pipeline root.
pub fn with_index_map(request: &[u8]) -> Result<Vec<u8>, String> {
    let dxbc = crate::Dxbc::parse(request)?;
    let part = dxbc.chunk(b"RTS0").ok_or("the request carries no root signature")?;
    let root = request.get(part.offset..part.offset + part.size).ok_or("truncated RTS0 part")?;
    let mapped = append_heap_index_map(root)?;
    let padding = (part.size % 4 + 4 - mapped.len() % 4) % 4;
    let span = mapped.len() + padding;
    let size = request.len() - part.size + span;
    let total = u32::try_from(size).map_err(|_| "request too large")?;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&request[..part.offset - 4]);
    out[4..20].fill(0);
    out[24..28].copy_from_slice(&total.to_le_bytes());
    for (index, other) in dxbc.chunks.iter().enumerate() {
        if other.offset > part.offset {
            let moved = (other.offset - 8 - part.size + span) as u32;
            out[32 + 4 * index..36 + 4 * index].copy_from_slice(&moved.to_le_bytes());
        }
    }
    out.extend_from_slice(&(mapped.len() as u32).to_le_bytes());
    out.extend_from_slice(&mapped);
    out.resize(part.offset + span, 0);
    out.extend_from_slice(&request[part.offset + part.size..]);
    Ok(out)
}

#[cfg(test)]
#[path = "tests/rootsig.rs"]
mod tests;

/// Append a stride-table descriptor to a shader's embedded root signature.
///
/// A console buffer descriptor keeps its element stride in word 1. A raw
/// IDXEN access cannot scale the index without that stride, so the runtime
/// appends a volatile root SRV (`t1, space32766`, one u32 per guest slot) to
/// the root signature and translates the program against it. This builds the
/// same variant from a shader's own embedded root: the parameters and their
/// bodies are kept verbatim, the stride parameter is appended before the
/// static samplers, and everything after the RTS0 chunk is shifted by the
/// growth.
///
/// Returns `None` for a container without a parseable embedded RTS0 chunk.
pub fn stride_table_variant(shader: &[u8]) -> Option<Vec<u8>> {
    fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    }
    let dxbc = crate::Dxbc::parse(shader).ok()?;
    let chunk = dxbc.chunk(b"RTS0")?;
    let body_start = chunk.offset;
    let body_end = body_start.checked_add(chunk.size)?;
    let body = shader.get(body_start..body_end)?;
    let version = u32_at(body, 0)?;
    let num_params = u32_at(body, 4)? as usize;
    let params_offset = u32_at(body, 8)? as usize;
    let num_static_samplers = u32_at(body, 12)? as usize;
    let flags = u32_at(body, 20)?;
    if !matches!(version, 1 | 2) || params_offset < 24 { return None; }
    let headers_end = params_offset.checked_add(num_params.checked_mul(12)?)?;
    if headers_end > body.len() { return None; }
    // Samplers occupy the end of the body when present. With no samplers,
    // the whole remainder is the body region.
    let sampler_offset = u32_at(body, 16)? as usize;
    let samplers_start = if num_static_samplers == 0 {
        body.len()
    } else {
        if sampler_offset < headers_end { return None; }
        if sampler_offset.checked_add(num_static_samplers.checked_mul(52)?)? > body.len() { return None; }
        sampler_offset
    };
    let samplers_end = if num_static_samplers == 0 {
        body.len()
    } else {
        sampler_offset.checked_add(num_static_samplers.checked_mul(52)?)?
    };
    // Every parameter header and its body must be inside the body region.
    let mut tables = Vec::new();
    for index in 0..num_params {
        let at = params_offset.checked_add(index.checked_mul(12)?)?;
        let header = body.get(at..at + 12)?;
        let tag = u32_at(header, 0)?;
        let offset = u32_at(header, 8)? as usize;
        if offset < headers_end || offset > samplers_start { return None; }
        if tag == 0 {
            if offset.checked_add(8)? > samplers_start { return None; }
            let count = u32_at(body, offset)? as usize;
            let ranges_offset = u32_at(body, offset + 4)? as usize;
            let stride = if version == 2 { 24 } else { 20 };
            if ranges_offset.checked_add(count.checked_mul(stride)?)? > samplers_end { return None; }
            tables.push((offset, ranges_offset));
        } else {
            // Constant or descriptor bodies are small and must end before
            // the static samplers.
            let stride = if tag == 1 { 12 } else if version == 2 { 12 } else { 8 };
            if offset.checked_add(stride)? > samplers_start { return None; }
        }
    }
    // One extra param header shifts the whole body region by 12 bytes; the
    // stride parameter adds one more 12-byte body before the samplers.
    let new_headers_end = params_offset.checked_add((num_params + 1).checked_mul(12)?)?;
    let region_len = samplers_start.checked_sub(headers_end)?;
    let stride_offset = new_headers_end.checked_add(region_len)?;
    let stride_body_len = if version == 2 { 12 } else { 8 };
    let samplers_end_new = stride_offset.checked_add(stride_body_len)?;
    let growth = 12 + stride_body_len; // one appended header + the stride body
    let mut new_body = Vec::with_capacity(24 + 12 * (num_params + 1) + region_len + 12
        + (samplers_end - samplers_start));
    new_body.extend_from_slice(&version.to_le_bytes());
    new_body.extend_from_slice(&((num_params + 1) as u32).to_le_bytes());
    new_body.extend_from_slice(&(params_offset as u32).to_le_bytes());
    new_body.extend_from_slice(&(num_static_samplers as u32).to_le_bytes());
    new_body.extend_from_slice(&(samplers_end_new as u32).to_le_bytes());
    new_body.extend_from_slice(&flags.to_le_bytes());
    // Old param headers, offsets shifted by one header (12 bytes).
    for index in 0..num_params {
        let at = params_offset.checked_add(index.checked_mul(12)?)?;
        let header = body.get(at..at + 12)?;
        let tag = u32_at(header, 0)?;
        let visibility = u32_at(header, 4)?;
        let offset = u32_at(header, 8)?;
        new_body.extend_from_slice(&tag.to_le_bytes());
        new_body.extend_from_slice(&visibility.to_le_bytes());
        new_body.extend_from_slice(&offset.checked_add(12)?.to_le_bytes());
    }
    // The stride parameter: a volatile root SRV at t1, space 32766.
    new_body.extend_from_slice(&3u32.to_le_bytes());
    new_body.extend_from_slice(&0u32.to_le_bytes());
    new_body.extend_from_slice(&(stride_offset as u32).to_le_bytes());
    // The body region, unchanged.
    new_body.extend_from_slice(body.get(headers_end..samplers_start)?);
    // Table ranges moved by the same header shift.
    for (offset, ranges_offset) in &tables {
        let at = new_headers_end + (*offset - headers_end) + 4;
        let shifted = (*ranges_offset as u32).saturating_add(12);
        new_body[at..at + 4].copy_from_slice(&shifted.to_le_bytes());
    }
    // The stride table itself: a volatile root SRV at t1, space 32766.
    new_body.extend_from_slice(&1u32.to_le_bytes());
    new_body.extend_from_slice(&32766u32.to_le_bytes());
    if version == 2 {
        new_body.extend_from_slice(&2u32.to_le_bytes());
    }
    // Samplers, unchanged.
    new_body.extend_from_slice(body.get(samplers_start..samplers_end)?);
    let new_body_len = new_body.len();
    let old_size = shader.len();
    let new_size = old_size.checked_add(growth)?;
    let tag_offset = body_start.checked_sub(8)?;
    let mut out = Vec::with_capacity(new_size);
    out.extend_from_slice(&shader[..tag_offset]);
    out.extend_from_slice(b"RTS0");
    out.extend_from_slice(&(new_body_len as u32).to_le_bytes());
    out.extend_from_slice(&new_body);
    out.extend_from_slice(&shader[body_end..]);
    // Patch the size field (bytes 24..28) and the directory entries of any
    // chunks that physically follow the RTS0 chunk.
    if out.len() < 28 { return None; }
    out[24..28].copy_from_slice(&(new_size as u32).to_le_bytes());
    for (index, entry) in dxbc.chunks.iter().enumerate() {
        let directory_at = 32usize.checked_add(index.checked_mul(4)?)?;
        if directory_at + 4 > out.len() { return None; }
        let chunk_tag = entry.offset.checked_sub(8)?;
        if chunk_tag > tag_offset {
            out[directory_at..directory_at + 4].copy_from_slice(
                &((chunk_tag + growth) as u32).to_le_bytes(),
            );
        }
    }
    if out.len() != new_size { return None; }
    Some(out)
}
