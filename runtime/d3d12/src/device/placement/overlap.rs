//! Sort the memory placed images repeat by why their title ranges overlap, for
//! the memory log: views of the same data could share one image, while reused
//! memory could not.
use std::collections::BTreeMap;

/// What decides whether two images hold the same title data.
pub(super) struct Image {
    pub(super) address: u64,
    /// Title bytes from `address`.
    pub(super) size: u64,
    /// Adapter memory behind the image.
    pub(super) bytes: u64,
    pub(super) format: i32,
    /// Dimension, width, height and depth or slices.
    pub(super) shape: (i32, u64, u32, u16),
    /// Each subresource's slice, level and title range.
    pub(super) mips: Vec<(u32, u32, u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    /// The same subresources in a format of the same typeless family.
    Cast,
    /// The same subresources in an unrelated format.
    Reinterpreted,
    /// The same shape and family, with some of the subresources.
    Subset,
    /// The same start, holding something else.
    OtherAtAddress,
    /// A start inside an earlier image's range.
    Inside,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Report {
    /// Images and their adapter memory, by kind.
    pub(super) kinds: BTreeMap<Kind, (u64, u64)>,
    /// Adapter memory of each format pair among views of the same data.
    pub(super) pairs: BTreeMap<(i32, i32), u64>,
}

/// The first format number of `format`'s typeless family.
pub(super) fn family(format: i32) -> i32 {
    match format {
        1..=4 => 1,
        5..=8 => 5,
        9..=14 => 9,
        15..=18 => 15,
        19..=22 => 19,
        23..=25 => 23,
        27..=32 => 27,
        33..=38 => 33,
        39..=43 => 39,
        44..=47 => 44,
        48..=52 => 48,
        53..=59 => 53,
        60..=64 => 60,
        70..=72 => 70,
        73..=75 => 73,
        76..=78 => 76,
        79..=81 => 79,
        82..=84 => 82,
        87 | 90 | 91 => 90,
        88 | 92 | 93 => 92,
        94..=96 => 94,
        97..=99 => 97,
        other => other,
    }
}

/// How `image` relates to `other`, which starts at the same address and takes
/// at least as much memory.
fn relation(image: &Image, other: &Image) -> Kind {
    let related = family(image.format) == family(other.format);
    let contained = image.mips.iter().all(|mip| other.mips.contains(mip));
    if image.shape != other.shape {
        Kind::OtherAtAddress
    } else if image.mips == other.mips {
        if related { Kind::Cast } else { Kind::Reinterpreted }
    } else if related && contained {
        Kind::Subset
    } else {
        Kind::OtherAtAddress
    }
}

/// Count every image that repeats memory an earlier one covers. At each address
/// the largest image is the one the others are compared with.
pub(super) fn classify(images: &mut [Image]) -> Report {
    images.sort_by_key(|image| (image.address, std::cmp::Reverse(image.bytes)));
    let mut report = Report::default();
    let mut end = 0;
    for group in images.chunk_by(|a, b| a.address == b.address) {
        for (index, image) in group.iter().enumerate() {
            let found = if index == 0 {
                (image.address < end).then_some((Kind::Inside, None))
            } else {
                group[..index]
                    .iter()
                    .map(|other| (relation(image, other), Some(other.format)))
                    .min_by_key(|(kind, _)| *kind)
            };
            let Some((kind, other)) = found else { continue };
            let total = report.kinds.entry(kind).or_default();
            total.0 += 1;
            total.1 += image.bytes;
            if let (Kind::Cast | Kind::Reinterpreted | Kind::Subset, Some(other)) = (kind, other) {
                *report.pairs.entry((other, image.format)).or_default() += image.bytes;
            }
        }
        end = group.iter().map(|image| image.address + image.size).fold(end, u64::max);
    }
    report
}

impl Report {
    pub(super) fn text(&self) -> String {
        let kind = |kind| {
            let (count, bytes) = self.kinds.get(&kind).copied().unwrap_or_default();
            format!("{count} ({} MiB)", bytes >> 20)
        };
        format!(
            "cast {}, reinterpreted {}, subset {}, other at address {}, inside {}",
            kind(Kind::Cast),
            kind(Kind::Reinterpreted),
            kind(Kind::Subset),
            kind(Kind::OtherAtAddress),
            kind(Kind::Inside),
        )
    }

    /// The format pairs that repeat the most memory, as DXGI numbers.
    pub(super) fn top_pairs(&self, count: usize) -> String {
        let mut pairs: Vec<_> = self.pairs.iter().collect();
        pairs.sort_by_key(|(_, bytes)| std::cmp::Reverse(**bytes));
        pairs
            .iter()
            .take(count)
            .map(|((first, second), bytes)| format!("{first}/{second} {} MiB", **bytes >> 20))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
#[path = "../../tests/device/placement/overlap.rs"]
mod tests;
