use super::{Image, Kind, classify, family};

fn image(address: u64, bytes: u64, format: i32, width: u64, mips: u32) -> Image {
    Image {
        address,
        size: bytes,
        bytes,
        format,
        shape: (3, width, 64, 1),
        mips: (0..mips).map(|level| (0, level, u64::from(level) * 16, u64::from(level + 1) * 16)).collect(),
    }
}

#[test]
fn typed_and_srgb_formats_share_a_family() {
    assert_eq!(family(28), family(29));
    assert_eq!(family(71), family(72));
    assert_eq!(family(87), family(91));
    assert_ne!(family(87), family(28));
    assert_ne!(family(71), family(98));
}

#[test]
fn each_repeat_is_counted_by_its_kind() {
    let mut images = [
        image(0, 100, 28, 64, 3),
        image(0, 90, 29, 64, 3),  // sRGB view
        image(0, 80, 42, 64, 3),  // raw view
        image(0, 70, 28, 64, 2),  // fewer mips
        image(0, 60, 28, 32, 3),  // another shape
        image(50, 40, 28, 64, 3), // starts inside the first range
        image(500, 30, 28, 64, 3),
    ];
    let report = classify(&mut images);
    let expect = [(Kind::Cast, 90), (Kind::Reinterpreted, 80), (Kind::Subset, 70), (Kind::OtherAtAddress, 60), (Kind::Inside, 40)];
    for (kind, bytes) in expect {
        assert_eq!(report.kinds[&kind], (1, bytes), "{kind:?}");
    }
    assert_eq!(report.pairs[&(28, 29)], 90);
    assert!(report.top_pairs(1).starts_with("28/29 "));
}

#[test]
fn separate_images_repeat_nothing() {
    let mut images = [image(0, 16, 28, 64, 1), image(16, 16, 28, 64, 1)];
    assert!(classify(&mut images).kinds.is_empty());
}
