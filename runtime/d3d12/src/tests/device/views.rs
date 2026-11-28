use super::{default_texture_uav_desc, desktop_cbv_bytes, earlier_page, root_view_window, CommandImports, HeldPages};
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16_UINT, DXGI_FORMAT_R32_UINT};

#[test]
fn an_earlier_page_puts_a_view_on_a_whole_element() {
    // ProjectScarlett: 20-byte elements starting 352 bytes into a page.
    let page = earlier_page(0xaa8f_0160, 20, 0xaa00_0000).unwrap();
    assert_eq!(page, 0xaa8c_0000);
    assert_eq!((0xaa8f_0160 - page) % 20, 0);
    // The allocation starts at the view's own page: nothing earlier to use.
    assert_eq!(earlier_page(0xaa8f_0160, 20, 0xaa8f_0000), None);
    // No page boundary can fix an offset the stride's page factor rules out.
    assert_eq!(earlier_page(0xaa8f_0162, 20, 0), None);
    assert_eq!(earlier_page(0x2_0028, 20, 0), Some(0x2_0000));
}

#[test]
fn a_root_view_maps_up_to_its_reach_of_committed_memory() {
    const MIB: u64 = 1 << 20;
    let base = 0x1_0000_0000;
    // The root-srv-reach control: 0x4480 into an allocation with 2 MiB after
    // it, committed in 4 KiB pages. The import is every whole page of the run.
    assert_eq!(root_view_window(base + 0x4480, base, base + 0x20_5000, |_| unreachable!()), Some((base, 2 * MIB)));
    // Inside a larger run the import starts at the 4 MiB boundary below the
    // address and spans 8 MiB, so the bindings after it reuse it.
    let address = base + 0x123_4560;
    let committed = |at| {
        assert_eq!(at, base + 16 * MIB);
        Some(base + 64 * MIB)
    };
    assert_eq!(root_view_window(address, base, base + 64 * MIB, committed), Some((base + 16 * MIB, 8 * MIB)));
    // Memory between that boundary and the address that is not committed
    // moves the start to the address's own page.
    assert_eq!(root_view_window(address, base, base + 64 * MIB, |_| None), Some((base + 0x123_0000, 8 * MIB)));
    assert_eq!(root_view_window(address, base, base + 64 * MIB, |_| Some(base + 0x110_0000)), Some((base + 0x123_0000, 8 * MIB)));
    // An allocation that begins above the boundary: the import starts with it.
    let allocation = base + 3 * MIB;
    assert_eq!(root_view_window(allocation + 0x2_0010, allocation, allocation + 16 * MIB, |_| Some(allocation + 16 * MIB)),
        Some((allocation, 8 * MIB)));
    // The import ends at the last whole page of the run, and an address in a
    // partial page after it has no window.
    assert_eq!(root_view_window(base + 0x4480, base, base + 0x1_8000, |_| unreachable!()), Some((base, 0x1_0000)));
    assert_eq!(root_view_window(base + 0x1_4480, base, base + 0x1_8000, |_| Some(base + 0x1_8000)), None);
}

struct Pages(bool);

impl HeldPages for Pages {
    fn pages_held(&self) -> bool {
        self.0
    }
}

#[test]
fn command_imports_keep_only_imports_whose_pages_stay_mapped() {
    let mut kept = CommandImports::<Pages>::default();
    let title = Arc::new(Pages(false));
    kept.keep(&title);
    assert_eq!(Arc::strong_count(&title), 1);
    let held = Arc::new(Pages(true));
    kept.keep(&held);
    kept.keep(&held);
    assert_eq!(Arc::strong_count(&held), 2);
}

#[test]
fn command_imports_release_the_least_recently_used_first() {
    let mut kept = CommandImports::<Pages>::default();
    let (first, second) = (Arc::new(Pages(true)), Arc::new(Pages(true)));
    kept.keep(&first);
    kept.keep(&second);
    let others: Vec<_> = (2..CommandImports::<Pages>::LIMIT).map(|_| Arc::new(Pages(true))).collect();
    for other in &others {
        kept.keep(other);
    }
    // Full. Using `first` again leaves `second` the least recent.
    kept.keep(&first);
    let newest = Arc::new(Pages(true));
    kept.keep(&newest);
    assert_eq!(Arc::strong_count(&first), 2);
    assert_eq!(Arc::strong_count(&second), 1);
    assert_eq!(Arc::strong_count(&newest), 2);
    assert!(others.iter().all(|other| Arc::strong_count(other) == 2));
}

#[test]
fn preceding_entries_look_past_released_ones() {
    let views = super::super::placement::Views::<u32>::default();
    let live: Vec<_> = (0..4).map(Arc::new).collect();
    for (i, entry) in live.iter().enumerate() {
        views.set(0x1000 * (i + 1), Some(entry));
    }
    let released = Arc::new(9);
    views.set(0x3800, Some(&released));
    drop(released);
    // The nearest entry alone is the released one.
    assert!(views.preceding(0x3900).is_none());
    let found = |address, limit| views.preceding_entries(address, limit).iter().map(|entry| **entry).collect::<Vec<_>>();
    assert_eq!(found(0x3900, 8), [2, 1, 0]);
    assert_eq!(found(0x4000, 2), [3, 2]);
    assert!(found(0xfff, 8).is_empty());
}

#[test]
fn desktop_cbv_size_is_a_256_byte_multiple_up_to_the_limit() {
    assert_eq!(desktop_cbv_bytes(0), 0);
    assert_eq!(desktop_cbv_bytes(1), 256);
    assert_eq!(desktop_cbv_bytes(144), 256);
    assert_eq!(desktop_cbv_bytes(256), 256);
    assert_eq!(desktop_cbv_bytes(257), 512);
    assert_eq!(desktop_cbv_bytes(65536), 65536);
    assert_eq!(desktop_cbv_bytes(65537), 65537);
}

#[test]
fn default_texture_uav_desc_synthesizes_raw_views_for_all_texture_dimensions() {
    let base = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 16,
        Height: 16,
        DepthOrArraySize: 1,
        MipLevels: 1,
        ..Default::default()
    };
    let v2d = default_texture_uav_desc(&base, DXGI_FORMAT_R32_UINT).expect("2D UAV");
    assert_eq!(v2d.Format, DXGI_FORMAT_R32_UINT);
    assert_eq!(v2d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2D);

    let v2da = default_texture_uav_desc(
        &D3D12_RESOURCE_DESC { DepthOrArraySize: 6, ..base },
        DXGI_FORMAT_R32_UINT,
    )
    .expect("2D array UAV");
    assert_eq!(v2da.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
    assert_eq!(unsafe { v2da.Anonymous.Texture2DArray.ArraySize }, 6);

    let v1d = default_texture_uav_desc(
        &D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE1D, Height: 1, ..base },
        DXGI_FORMAT_R16_UINT,
    )
    .expect("1D UAV");
    assert_eq!(v1d.Format, DXGI_FORMAT_R16_UINT);
    assert_eq!(v1d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE1D);

    let v1da = default_texture_uav_desc(
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE1D,
            Height: 1,
            DepthOrArraySize: 4,
            ..base
        },
        DXGI_FORMAT_R16_UINT,
    )
    .expect("1D array UAV");
    assert_eq!(v1da.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE1DARRAY);
    assert_eq!(unsafe { v1da.Anonymous.Texture1DArray.ArraySize }, 4);

    let v3d = default_texture_uav_desc(
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
            DepthOrArraySize: 8,
            ..base
        },
        DXGI_FORMAT_R32_UINT,
    )
    .expect("3D UAV");
    assert_eq!(v3d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
    assert_eq!(unsafe { v3d.Anonymous.Texture3D.WSize }, 8);

    assert!(default_texture_uav_desc(
        &D3D12_RESOURCE_DESC { Dimension: D3D12_RESOURCE_DIMENSION_BUFFER, ..base },
        DXGI_FORMAT_R32_UINT,
    )
    .is_none());
}
