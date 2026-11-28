use super::*;

fn extents(entries: &[(u64, u64)]) -> BTreeMap<u64, u64> {
    entries.iter().copied().collect()
}

#[test]
fn a_structure_over_older_ones_releases_only_those_it_covers() {
    let reserved = extents(&[(0x1000, 0x200), (0x1200, 0x100), (0x1400, 0x400), (0x2000, 0x100)]);
    // Ends inside the third structure, which starts before the new end.
    assert_eq!(overlapping(&reserved, 0x1100, 0x380), vec![0x1000, 0x1200, 0x1400]);
}

#[test]
fn adjacent_structures_stay_live() {
    let reserved = extents(&[(0x1000, 0x100), (0x1200, 0x100)]);
    assert!(overlapping(&reserved, 0x1100, 0x100).is_empty());
}

#[test]
fn a_rebuild_at_the_same_address_is_not_an_overlap() {
    let reserved = extents(&[(0x1000, 0x100)]);
    assert!(overlapping(&reserved, 0x1000, 0x400).is_empty());
}

#[test]
fn a_structure_inside_an_older_one_releases_it() {
    let reserved = extents(&[(0x1000, 0x1000)]);
    assert_eq!(overlapping(&reserved, 0x1800, 0x100), vec![0x1000]);
}
