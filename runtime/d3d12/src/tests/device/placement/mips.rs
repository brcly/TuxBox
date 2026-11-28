use super::first_held;

/// A 1024x1024 BC1 chain as the console lays it out, smallest level first.
const ENDS: [u64; 9] = [700_416, 175_104, 44_032, 12_288, 4_096, 2_048, 1_536, 1_280, 768];

#[test]
fn only_the_mips_ending_before_the_next_image_are_held() {
    assert_eq!(first_held(ENDS.into_iter(), 700_416), 0);
    assert_eq!(first_held(ENDS.into_iter(), 700_415), 1);
    assert_eq!(first_held(ENDS.into_iter(), 65_536), 2);
    assert_eq!(first_held(ENDS.into_iter(), 16_384), 3);
}

#[test]
fn the_smallest_mip_is_always_held() {
    assert_eq!(first_held(ENDS.into_iter(), 1), 8);
    assert_eq!(first_held(ENDS.into_iter(), 0), 8);
    assert_eq!(first_held([64u64].into_iter(), 0), 0);
}
