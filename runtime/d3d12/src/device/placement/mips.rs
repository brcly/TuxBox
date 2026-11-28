//! Which mips of a placed image the title holds.
//!
//! A console mip chain lies smallest level first, so a title can allocate only
//! an image's small mips and put the next image where the large ones would be,
//! while still describing the whole chain. In ProjectScarlett the memory past
//! the next image's start never held those mips. They get no memory here.
use super::{Alias, Texture};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use windows::Win32::Graphics::Direct3D12::*;

/// `TUXBOX_PARTIAL_MIPS=0` gives every placed image its whole mip chain.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| super::residency::parse(std::env::var("TUXBOX_PARTIAL_MIPS").ok().as_deref()))
}

/// The largest mip from which every smaller one ends within `room` bytes of
/// the image's start. `ends` holds each level's end, largest level first. The
/// smallest level is held whatever the room.
pub(super) fn first_held(ends: impl DoubleEndedIterator<Item = u64> + ExactSizeIterator, room: u64) -> u32 {
    let levels = ends.len();
    let held = ends.rev().take_while(|&end| end <= room).count();
    (levels - held.max(1).min(levels)) as u32
}

/// The first level of `texture` that needs memory: 0 unless it is a sampled 2D
/// mip chain that a later image starts inside.
pub(super) fn first_needed(images: &BTreeMap<u64, Vec<Alias>>, texture: &Texture) -> u32 {
    let desc = &texture.desc;
    let written = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
        | D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL
        | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
    if !enabled()
        || desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.DepthOrArraySize != 1
        || texture.mips.len() < 2
        || texture.plane.is_none()
        || (desc.Flags & written).0 != 0
    {
        return 0;
    }
    let range = texture.range();
    // Only the count is read: releasing an image takes the caller's lock.
    let next = images
        .range(range.start + 1..range.end)
        .find(|(_, aliases)| aliases.iter().any(|alias| alias.texture.strong_count() != 0));
    let Some((&next, _)) = next else {
        return 0;
    };
    first_held(texture.mips.iter().map(|mip| mip.source.end), next - range.start)
}

#[cfg(test)]
#[path = "../../tests/device/placement/mips.rs"]
mod tests;
