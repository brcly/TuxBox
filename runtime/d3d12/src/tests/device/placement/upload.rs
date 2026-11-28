use super::super::{Images, Texture};
use super::{Change, Transfer};
use std::sync::Arc;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

fn hardware_device() -> Option<ID3D12Device> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        if unsafe { adapter.GetDesc1() }.is_ok_and(|desc| desc.Flags & 2 != 0) {
            continue;
        }
        let mut device = None;
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_ok() {
            return device;
        }
    }
    None
}

/// A placed texture over zero-filled host memory standing in for title memory.
struct Placed {
    texture: Arc<Texture>,
    memory: *mut core::ffi::c_void,
    guest: &'static mut [u8],
}

impl Placed {
    fn new(device: &ID3D12Device, images: &Arc<Images>, desc: &D3D12_RESOURCE_DESC) -> Self {
        let plane = super::super::color_plane(desc).expect("a supported layout");
        let memory = unsafe { VirtualAlloc(None, plane.size as usize + (1 << 16), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        assert!(!memory.is_null());
        let address = (memory as u64).next_multiple_of(plane.alignment);
        let texture = Texture::new(device, images, address, desc, None).expect("placed texture");
        let guest = unsafe { std::slice::from_raw_parts_mut(address as *mut u8, plane.size as usize) };
        Self { texture, memory, guest }
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        unsafe { VirtualFree(self.memory, 0, MEM_RELEASE).unwrap() };
    }
}

fn texture(format: DXGI_FORMAT, width: u64, height: u32, slices: u16, mips: u16, layout: i32) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: width,
        Height: height,
        DepthOrArraySize: slices,
        MipLevels: mips,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT(layout),
        ..Default::default()
    }
}

/// Each element lands where its own address query says, in the upload and
/// in the runs writeback uses. R8 in the 64 KiB display layout keeps only
/// eight bytes of a row together, which 16-byte runs copied wrongly.
#[test]
fn placed_mips_detile_from_every_element_address() {
    let Some(device) = hardware_device() else {
        eprintln!("skipped: no hardware adapter");
        return;
    };
    let images = Arc::new(Images::default());
    for desc in [
        texture(DXGI_FORMAT_R8_UNORM, 300, 77, 2, 9, 0x10a),
        texture(DXGI_FORMAT_R8_UNORM, 300, 77, 1, 1, 0x10a),
        texture(DXGI_FORMAT_BC1_UNORM, 260, 132, 2, 9, 0x105),
        texture(DXGI_FORMAT_BC7_UNORM, 512, 256, 1, 10, 0x10a),
        texture(DXGI_FORMAT_R8G8B8A8_UNORM, 129, 65, 2, 8, 0x105),
        texture(DXGI_FORMAT_R16G16B16A16_FLOAT, 70, 33, 1, 7, 0x105),
        texture(DXGI_FORMAT_R8G8B8A8_UNORM, 100, 30, 1, 1, 0x100),
    ] {
        let placed = Placed::new(&device, &images, &desc);
        placed.guest.iter_mut().enumerate().for_each(|(i, b)| *b = (i.wrapping_mul(2_654_435_761) >> 11) as u8);
        let texture = &placed.texture;
        let plane = texture.plane.as_ref().unwrap();
        for (level, mip) in texture.mips.iter().enumerate() {
            let source = mip.source.start as usize..mip.source.end as usize;
            let change = Change {
                texture: texture.clone(),
                level,
                bytes: placed.guest[source].to_vec(),
                digest: super::super::Digest::of(&[]),
            };
            let mut uploaded = vec![0u8; mip.bytes as usize];
            change.detile(&mut uploaded).unwrap();
            let mut runs = vec![0u8; mip.bytes as usize];
            texture.layout_runs(level, |tiled, linear| {
                runs[linear].copy_from_slice(&change.bytes[tiled]);
                Ok(())
            }).unwrap();
            let element = mip.element_bytes as usize;
            let pitch = mip.footprint.Footprint.RowPitch as usize;
            for y in 0..mip.rows {
                for x in 0..mip.elements {
                    let at = (plane.element_offset_in_slice(mip.slice, mip.level, x.into(), y).unwrap() - mip.source.start) as usize;
                    let to = y as usize * pitch + x as usize * element;
                    let expected = &change.bytes[at..at + element];
                    assert_eq!(&uploaded[to..to + element], expected,
                        "format {} layout {:#x} subresource {level} ({x}, {y})", desc.Format.0, desc.Layout.0);
                    assert_eq!(&runs[to..to + element], expected, "writeback runs, subresource {level} ({x}, {y})");
                }
            }
        }
    }
}

/// Once a batch's copy has completed, the next batch records into its upload
/// buffer and command list instead of creating new ones.
#[test]
fn completed_uploads_lend_their_buffer_to_the_next_change() {
    let Some(device) = hardware_device() else {
        eprintln!("skipped: no hardware adapter");
        return;
    };
    let images = Arc::new(Images::default());
    let placed = Placed::new(&device, &images, &texture(DXGI_FORMAT_R8G8B8A8_UNORM, 256, 256, 1, 9, 0x105));
    let mut transfer = Transfer::new(&device).unwrap();
    for round in 0..4u8 {
        placed.guest.iter_mut().enumerate().for_each(|(i, b)| *b = (i as u8).wrapping_mul(7).wrapping_add(round));
        transfer.refresh(&device, std::slice::from_ref(&placed.texture)).unwrap();
        let tick = transfer.pending.back().expect("a changed texture is uploaded").0;
        unsafe { transfer.fence.SetEventOnCompletion(tick, HANDLE::default()).unwrap() };
        assert_eq!(transfer.buffers_created, 1, "round {round}");
    }
    // Unchanged contents upload nothing and return the last buffer to the pool.
    transfer.refresh(&device, std::slice::from_ref(&placed.texture)).unwrap();
    assert!(transfer.pending.is_empty());
    assert_eq!(transfer.idle.len(), 1);
}

#[test]
fn upload_buffers_round_up_to_shared_sizes() {
    assert_eq!(super::upload_capacity(1), Some(1 << 20));
    assert_eq!(super::upload_capacity((3 << 20) + 1), Some(4 << 20));
    assert_eq!(super::upload_capacity(32 << 20), Some(32 << 20));
    assert_eq!(super::upload_capacity((32 << 20) + 1), Some(48 << 20));
    assert_eq!(super::upload_capacity(u64::MAX), None);
}

/// What uploading a changed mip chain costs per MiB, beyond reading and
/// digesting it, for the layouts titles place. Run with `--ignored
/// --nocapture` on a release build and a GPU.
#[test]
#[ignore]
fn changed_upload_cost() {
    let Some(device) = hardware_device() else {
        println!("no hardware adapter");
        return;
    };
    let images = Arc::new(Images::default());
    let mut transfer = Transfer::new(&device).unwrap();
    for (name, desc) in [
        ("BC1 0x105", texture(DXGI_FORMAT_BC1_UNORM, 2048, 2048, 1, 12, 0x105)),
        ("BC7 0x10a", texture(DXGI_FORMAT_BC7_UNORM, 2048, 2048, 1, 12, 0x10a)),
        ("RGBA8 0x105", texture(DXGI_FORMAT_R8G8B8A8_UNORM, 2048, 2048, 1, 12, 0x105)),
        ("RGBA8 0x100", texture(DXGI_FORMAT_R8G8B8A8_UNORM, 2048, 2048, 1, 1, 0x100)),
    ] {
        let placed = Placed::new(&device, &images, &desc);
        let mib = placed.guest.len() as f64 / 1048576.0;
        let refresh = |transfer: &mut Transfer| {
            let start = std::time::Instant::now();
            transfer.refresh(&device, std::slice::from_ref(&placed.texture)).unwrap();
            let elapsed = start.elapsed().as_secs_f64() * 1e3;
            if let Some(&(tick, _)) = transfer.pending.back() {
                unsafe { transfer.fence.SetEventOnCompletion(tick, HANDLE::default()).unwrap() };
            }
            elapsed
        };
        let mut changed = Vec::new();
        let mut unchanged = Vec::new();
        for round in 0..8u8 {
            placed.guest.iter_mut().enumerate().for_each(|(i, b)| *b = (i as u8).wrapping_mul(31).wrapping_add(round));
            changed.push(refresh(&mut transfer));
            unchanged.push(refresh(&mut transfer));
        }
        let median = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let (first, steady, check) = (changed[0], median(&changed[1..]), median(&unchanged));
        println!("{name} {mib:.1} MiB: changed first {:.3} ms/MiB, steady {:.3} ms/MiB; unchanged {:.3} ms/MiB; upload beyond the check {:.3} ms/MiB",
            first / mib, steady / mib, check / mib, (steady - check) / mib);
    }
}
