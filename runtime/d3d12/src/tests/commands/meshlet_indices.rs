use super::*;

/// A stream with two meshlets. Meshlet 0's list starts right after the
/// triangle words; meshlet 1's list starts 8 bytes later.
fn stream() -> Vec<u8> {
    let words = |w: &[u32]| w.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>();
    let mut bytes = words(&[0x8000_0002, 3, 0]);
    // Meshlet records: list offsets 0 and 4 (in u16 entries), relative to the lists.
    bytes.extend(words(&[0, 0, 0, 0]));
    bytes.extend(words(&[0, 4, 0, 0]));
    // Triangles: meshlet 0 (0, 1, 2), meshlet 0 (2, 1, 3), meshlet 1 (0, 2, 1).
    bytes.extend(words(&[2 << 14 | 1 << 7, 3 << 14 | 1 << 7 | 2, 1 << 21 | 1 << 14 | 2 << 7]));
    // Vertex lists: meshlet 0 = 10, 11, 12, 13; meshlet 1 = 500, 501, 502.
    for v in [10u16, 11, 12, 13, 500, 501, 502] {
        bytes.extend(v.to_le_bytes());
    }
    bytes
}

fn from(bytes: &[u8]) -> impl Fn(usize, usize) -> Option<Vec<u8>> + '_ {
    |offset, len| bytes.get(offset..offset.checked_add(len)?).map(<[u8]>::to_vec)
}

#[test]
fn meshlet_triangles_become_list_indices() {
    let bytes = stream();
    assert_eq!(decode(9, from(&bytes)), Some(vec![10, 11, 12, 12, 11, 13, 500, 502, 501]));
}

#[test]
fn a_list_offset_uses_the_high_bit_of_the_first_word() {
    let mut bytes = stream();
    // Bit 14 of the first record word adds 0x10000 entries to meshlet 0's list offset.
    bytes[12..16].copy_from_slice(&(1u32 << 14).to_le_bytes());
    assert_eq!(decode(9, from(&bytes)), None, "the list now lies past the stream");
}

#[test]
fn streams_that_do_not_match_the_build_are_refused() {
    let bytes = stream();
    // The index count must be three per triangle in the header.
    assert_eq!(decode(6, from(&bytes)), None);
    assert_eq!(decode(10, from(&bytes)), None);
    assert_eq!(decode(0, from(&bytes)), None);
    // Without the coded flag in the first word.
    let mut plain = bytes.clone();
    plain[3] = 0;
    assert_eq!(decode(9, from(&plain)), None);
    // A triangle naming a meshlet that does not exist.
    let mut stray = bytes.clone();
    stray[44..48].copy_from_slice(&(2u32 << 21).to_le_bytes());
    assert_eq!(decode(9, from(&stray)), None);
    // A vertex list cut short.
    assert_eq!(decode(9, from(&bytes[..bytes.len() - 2])), None);
}
