//! Console meshlet-coded triangle indices (index format 53).
//!
//! A bottom-level build can name index format 53, which D3D12 does not
//! define. ProjectScarlett's own builds use it for their static geometry. The
//! stream holds:
//! - a header: `0x80000000 | meshlets`, the triangle count, one more word;
//! - one 16-byte record per meshlet, whose first two words give the byte
//!   offset of the meshlet's vertex list;
//! - one word per triangle: `meshlet << 21 | c << 14 | b << 7 | a`, where
//!   a, b, c index the meshlet's vertex list;
//! - the vertex lists, u16 vertex indices.
//!
//! A host build cannot read this, so the stream is decoded on the CPU into
//! R32_UINT indices.

pub(super) const FORMAT: u32 = 53;
const HEADER: usize = 12;
const MESHLET: usize = 16;
/// Larger streams are refused rather than read.
const LIMIT: usize = 64 << 20;

/// The bytes of `bytes` at `address` in this process, or None if any of them
/// cannot be read.
fn read(address: u64, bytes: usize) -> Option<Vec<u8>> {
    if address == 0 || bytes == 0 || bytes > LIMIT {
        return None;
    }
    let mut out = vec![0u8; bytes];
    // The address is the one the title gives the GPU, which need not be
    // where its CPU wrote the data.
    crate::device::read_title_bytes(address, &mut out).then_some(out)
}

/// The decoded indices of the stream at `address`, which the build describes
/// with `index_count` indices.
pub(super) fn at(address: u64, index_count: u32) -> Option<Vec<u32>> {
    decode(index_count, |offset, bytes| read(address.checked_add(offset as u64)?, bytes))
}

fn word(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

/// Decodes a stream through `read(offset, bytes)`, which returns that many
/// bytes from the stream's start. Every triangle's indices must name an entry
/// of its meshlet's list inside the stream.
pub(super) fn decode(index_count: u32, read: impl Fn(usize, usize) -> Option<Vec<u8>>) -> Option<Vec<u32>> {
    if index_count == 0 || index_count % 3 != 0 {
        return None;
    }
    let triangles = (index_count / 3) as usize;
    let header = read(0, HEADER)?;
    let first = word(&header, 0)?;
    if first & 0x8000_0000 == 0 || word(&header, 4)? as usize != triangles {
        return None;
    }
    let meshlets = (first & 0x1ffff) as usize;
    let records = meshlets.checked_mul(MESHLET)?;
    let tables = read(HEADER, records.checked_add(triangles.checked_mul(4)?)?)?;
    let lists = HEADER + tables.len();
    let mut extent = 0usize;
    let mut coded = Vec::with_capacity(triangles);
    for triangle in 0..triangles {
        let code = word(&tables, records + 4 * triangle)?;
        let meshlet = (code >> 21) as usize;
        if meshlet >= meshlets {
            return None;
        }
        let a = word(&tables, MESHLET * meshlet)?;
        let b = word(&tables, MESHLET * meshlet + 4)?;
        let base = 2 * ((((a << 2) & 0x1_0000) | (b & 0xffff)) as usize);
        let last = [0, 7, 14].into_iter().map(|shift| (code >> shift) & 0x7f).max()? as usize;
        extent = extent.max(base + 2 * (last + 1));
        coded.push((base, code));
    }
    let vertices = read(lists, extent)?;
    let mut out = Vec::with_capacity(3 * triangles);
    for (base, code) in coded {
        for shift in [0, 7, 14] {
            let at = base + 2 * ((code >> shift) & 0x7f) as usize;
            out.push(u32::from(u16::from_le_bytes(vertices.get(at..at + 2)?.try_into().ok()?)));
        }
    }
    Some(out)
}

#[cfg(test)]
#[path = "../../tests/commands/meshlet_indices.rs"]
mod tests;
