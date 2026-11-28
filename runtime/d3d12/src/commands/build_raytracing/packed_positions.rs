//! Console packed triangle positions (R32G32_FLOAT vertices).
//!
//! The console's acceleration-structure builder has no two-component float
//! position path: for vertex format R32G32_FLOAT (DXGI 16) its leaf kernel
//! reads 8-byte records instead, with a 32-byte header just before the
//! vertex buffer's start:
//! - the origin, three floats;
//! - the axis order, a word;
//! - the scale, three floats; then one unused word.
//!
//! Each record holds three unsigned quantized coordinates: `a = lo >> 8`,
//! `b = (hi & 0xffff) << 8 | (lo & 0xff)` and `c = hi >> 16`. Each becomes
//! `q * scale + origin` component by component, then the axis order arranges
//! them: 0 gives (c, b, a), 1 gives (b, c, a), anything else (a, b, c). The
//! build's own transform still applies afterwards.
//!
//! The builder reads the vertex stride through a 14-bit field: titles pass
//! strides such as 0x8008 for 8-byte records, so only the low 14 bits count.
//!
//! A host build would read the records as two floats per vertex, so they are
//! decoded on the CPU into R32G32B32_FLOAT positions.

pub(super) const HEADER: u64 = 32;
const RECORD: usize = 8;
/// Larger vertex ranges are refused rather than read.
const LIMIT: usize = 64 << 20;

fn read(address: u64, bytes: usize) -> Option<Vec<u8>> {
    if address == 0 || bytes == 0 || bytes > LIMIT {
        return None;
    }
    let mut out = vec![0u8; bytes];
    // The address is the one the title gives the GPU, which need not be
    // where its CPU wrote the data.
    crate::device::read_title_bytes(address, &mut out).then_some(out)
}

/// The stride the console builder uses for a title's `StrideInBytes`: the
/// low 14 bits.
pub(super) fn stride(title: u64) -> u64 {
    title & 0x3fff
}

/// The positions of `count` records at `address`, `stride` (already
/// masked, see `stride`) bytes apart.
pub(super) fn at(address: u64, count: u32, stride: u64) -> Option<Vec<[f32; 3]>> {
    let header = read(address.checked_sub(HEADER)?, HEADER as usize)?;
    let stride = usize::try_from(stride).ok()?;
    let span = (count as usize).checked_sub(1)?.checked_mul(stride)?.checked_add(RECORD)?;
    decode(&header, &read(address, span)?, count as usize, stride)
}

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Decodes `count` records `stride` bytes apart from `records`, with the
/// 32-byte `header` that precedes them. A header whose origin or scale is not
/// finite is refused.
pub(super) fn decode(header: &[u8], records: &[u8], count: usize, stride: usize) -> Option<Vec<[f32; 3]>> {
    if header.len() != HEADER as usize || stride < RECORD || count == 0 {
        return None;
    }
    let float = |offset| f32::from_bits(word(header, offset));
    let origin = [float(0), float(4), float(8)];
    let order = word(header, 12);
    let scale = [float(16), float(20), float(24)];
    if !origin.iter().chain(&scale).all(|v| v.is_finite()) {
        return None;
    }
    (0..count)
        .map(|vertex| {
            let record = records.get(vertex * stride..vertex * stride + RECORD)?;
            let (lo, hi) = (word(record, 0), word(record, 4));
            let quantized = [lo >> 8, (hi & 0xffff) << 8 | (lo & 0xff), hi >> 16];
            let [a, b, c]: [f32; 3] = core::array::from_fn(|axis| quantized[axis] as f32 * scale[axis] + origin[axis]);
            Some(match order {
                0 => [c, b, a],
                1 => [b, c, a],
                _ => [a, b, c],
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "../../tests/commands/packed_positions.rs"]
mod tests;
