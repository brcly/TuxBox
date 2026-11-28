use super::*;

fn header(origin: [f32; 3], order: u32, scale: [f32; 3]) -> Vec<u8> {
    let mut bytes: Vec<u8> = origin.iter().flat_map(|v| v.to_le_bytes()).collect();
    bytes.extend(order.to_le_bytes());
    bytes.extend(scale.iter().flat_map(|v| v.to_le_bytes()));
    bytes.extend(0u32.to_le_bytes());
    bytes
}

/// A record holding the quantized coordinates a, b, c.
fn record(a: u32, b: u32, c: u32) -> Vec<u8> {
    let lo = a << 8 | (b & 0xff);
    let hi = c << 16 | (b >> 8);
    [lo.to_le_bytes(), hi.to_le_bytes()].concat()
}

#[test]
fn records_scale_from_the_origin_in_axis_order() {
    let records = [record(1, 2, 3), record(0x00ff_ffff, 0x00ff_ffff, 0xffff)].concat();
    let plain = header([10.0, 20.0, 30.0], 2, [0.5, 0.25, 2.0]);
    assert_eq!(
        decode(&plain, &records, 2, 8),
        Some(vec![[10.5, 20.5, 36.0], [10.0 + 0x00ff_ffff as f32 * 0.5, 20.0 + 0x00ff_ffff as f32 * 0.25, 30.0 + 0xffff as f32 * 2.0]])
    );
    assert_eq!(decode(&header([0.0; 3], 0, [1.0; 3]), &record(1, 2, 3), 1, 8), Some(vec![[3.0, 2.0, 1.0]]));
    assert_eq!(decode(&header([0.0; 3], 1, [1.0; 3]), &record(1, 2, 3), 1, 8), Some(vec![[2.0, 3.0, 1.0]]));
}

#[test]
fn a_wider_stride_skips_the_bytes_between_records() {
    let records = [record(1, 0, 0), vec![0xee; 8], record(2, 0, 0)].concat();
    assert_eq!(decode(&header([0.0; 3], 2, [1.0; 3]), &records, 2, 16), Some(vec![[1.0, 0.0, 0.0], [2.0, 0.0, 0.0]]));
}

#[test]
fn unusable_headers_and_short_ranges_are_refused() {
    let records = record(1, 2, 3);
    assert_eq!(decode(&header([f32::NAN, 0.0, 0.0], 2, [1.0; 3]), &records, 1, 8), None);
    assert_eq!(decode(&header([0.0; 3], 2, [f32::INFINITY, 1.0, 1.0]), &records, 1, 8), None);
    assert_eq!(decode(&header([0.0; 3], 2, [1.0; 3])[..28], &records, 1, 8), None);
    assert_eq!(decode(&header([0.0; 3], 2, [1.0; 3]), &records, 2, 8), None);
    assert_eq!(decode(&header([0.0; 3], 2, [1.0; 3]), &records, 1, 4), None);
}

#[test]
fn the_builder_reads_only_the_low_14_bits_of_the_stride() {
    // ProjectScarlett passes 0x8008 for its 8-byte records.
    assert_eq!(stride(0x8008), 8);
    assert_eq!(stride(8), 8);
    assert_eq!(stride(16), 16);
    assert_eq!(stride(0x3fff), 0x3fff);
    let records = [record(1, 0, 0), record(2, 0, 0)].concat();
    assert_eq!(
        decode(&header([0.0; 3], 2, [1.0; 3]), &records, 2, stride(0x8008) as usize),
        Some(vec![[1.0, 0.0, 0.0], [2.0, 0.0, 0.0]])
    );
}
