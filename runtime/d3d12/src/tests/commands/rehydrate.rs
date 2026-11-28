use super::*;

fn words(flags: u32, nodes: u32, crc: u32, elements: u32) -> Vec<u8> {
    [flags, nodes, crc, elements].iter().flat_map(|word| word.to_le_bytes()).collect()
}

#[test]
fn a_header_gives_its_first_four_words() {
    // The sample of Leonida's docs/bvh.md, section 3.1.
    let bytes = words(0x8102_0006, 9943, 0xeab7_667e, 5447);
    assert_eq!(header(&bytes), Some(Header { flags: 0x8102_0006, nodes: 9943, crc: 0xeab7_667e, elements: 5447 }));
    assert_eq!(header(&bytes[..15]), None);
}

#[test]
fn a_structure_spans_its_nodes_and_its_header() {
    // 64 * (nodes + 1) bytes, which the sample's end pointer confirms.
    assert_eq!(extent(header(&words(0x8102_0006, 9943, 0, 5447))), 64 * 9944);
    // Not a multiple of the structure alignment: the title's next one starts after it.
    assert_eq!(extent(header(&words(0x8102_0006, 4, 0, 2))), 512);
}

#[test]
fn a_small_structure_does_not_reach_its_neighbour() {
    // One leaf and its header: the next structure is 256 bytes on.
    assert_eq!(extent(header(&words(0x8102_0006, 1, 0, 1))), 256);
    assert_eq!(extent(header(&words(0x8102_0006, 3, 0, 1))), 256);
}

#[test]
fn bytes_that_are_no_bottom_level_header_claim_one_alignment() {
    assert_eq!(extent(None), 256);
    // No bottom-level flag, or no nodes.
    assert_eq!(extent(header(&words(0x0102_0006, 9943, 0, 5447))), 256);
    assert_eq!(extent(header(&words(0x8102_0006, 0, 0, 0))), 256);
    // The largest count does not overflow.
    assert_eq!(extent(header(&words(0x8000_0000, u32::MAX, 0, 0))), 64 * (1u64 << 32));
}
