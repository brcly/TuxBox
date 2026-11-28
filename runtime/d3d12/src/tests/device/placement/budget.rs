use super::{RESERVE, Room, covered, parse_reserve};
use windows::Win32::Graphics::Direct3D12::*;

#[test]
fn the_reserve_is_set_in_mebibytes_or_turned_off() {
    assert_eq!(parse_reserve(None), Some(RESERVE));
    assert_eq!(parse_reserve(Some("nonsense")), Some(RESERVE));
    assert_eq!(parse_reserve(Some(" 512 ")), Some(512 << 20));
    assert_eq!(parse_reserve(Some("0")), Some(0));
    assert_eq!(parse_reserve(Some("off")), None);
}

#[test]
fn sampled_images_spill_once_the_room_is_used() {
    let mut room = Room(Some(100));
    let sampled = D3D12_RESOURCE_FLAG_NONE;
    assert!(!room.spill(60, sampled));
    assert!(room.spill(60, sampled));
    // A smaller image still fits afterwards.
    assert!(!room.spill(40, sampled));
    assert!(room.spill(1, sampled));
}

#[test]
fn images_the_gpu_writes_stay_on_the_card() {
    let mut room = Room(Some(10));
    for flags in [
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    ] {
        assert!(!room.spill(8, flags));
    }
    // They still use up the room.
    assert!(room.spill(1, D3D12_RESOURCE_FLAG_NONE));
}

#[test]
fn an_unlimited_card_never_spills() {
    assert!(!Room(None).spill(u64::MAX, D3D12_RESOURCE_FLAG_NONE));
}

#[test]
fn overlapping_title_ranges_are_counted_once() {
    assert_eq!(covered(&mut []), 0);
    assert_eq!(covered(&mut [20..30, 0..10]), 20);
    assert_eq!(covered(&mut [0..10, 5..15, 5..8, 15..20]), 20);
}
