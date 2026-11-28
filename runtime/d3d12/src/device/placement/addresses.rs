//! Address tables that detile placed mips, shared by textures of one shape.
use super::{Texture, lock};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use tuxbox_xg_layout::MipAddresses;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DIMENSION_TEXTURE3D;

/// The texture description and copied extent a table was built for, and the
/// subresource index. The plane is a function of the description.
type Key = ([u64; 11], [u32; 3]);

struct Shared {
    tables: HashMap<Key, Weak<MipAddresses>>,
    prune_at: usize,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared { tables: HashMap::new(), prune_at: 64 }));

pub(super) type Slot = OnceLock<Option<Arc<MipAddresses>>>;

impl Texture {
    /// The address table for a 2D mip, built at its first upload or writeback.
    pub(super) fn addresses(&self, index: usize) -> Option<&MipAddresses> {
        self.addresses.get(index)?.get_or_init(|| self.build_addresses(index)).as_deref()
    }

    fn build_addresses(&self, index: usize) -> Option<Arc<MipAddresses>> {
        let plane = self.plane.as_ref()?;
        let mip = self.mips.get(index)?;
        if self.desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            return None;
        }
        let d = &self.desc;
        let key = (
            [
                d.Dimension.0 as u64, d.Alignment, d.Width, u64::from(d.Height), u64::from(d.DepthOrArraySize),
                u64::from(d.MipLevels), d.Format.0 as u64, u64::from(d.SampleDesc.Count),
                u64::from(d.SampleDesc.Quality), d.Layout.0 as u64, u64::from(d.Flags.0 as u32),
            ],
            [index as u32, mip.elements, mip.rows],
        );
        let mut shared = lock(&SHARED);
        if let Some(table) = shared.tables.get(&key).and_then(Weak::upgrade) {
            return Some(table);
        }
        let table = Arc::new(plane.mip_addresses(mip.slice, mip.level, mip.elements, mip.rows)?);
        if shared.tables.len() >= shared.prune_at {
            shared.tables.retain(|_, table| table.strong_count() != 0);
            shared.prune_at = (shared.tables.len() * 2).max(64);
        }
        shared.tables.insert(key, Arc::downgrade(&table));
        Some(table)
    }
}
