use super::super::budget::{Meter, Room};
use super::{Place, parse, reserve};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

#[test]
fn reserved_images_are_the_default() {
    assert!(parse(None));
    assert!(parse(Some("1")));
    assert!(parse(Some("")));
}

#[test]
fn zero_keeps_every_placed_image_committed() {
    for value in ["0", " 0 ", "off", "false", "no"] {
        assert!(!parse(Some(value)), "{value:?}");
    }
}

const SIDE: u32 = 2048;
const BYTES: u64 = SIDE as u64 * SIDE as u64 * 4;
const IMAGES: usize = 16;

fn hardware_device() -> Option<ID3D12Device> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        if unsafe { adapter.GetDesc1() }.is_ok_and(|desc| desc.Flags & 2 != 0) {
            continue;
        }
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_ok() {
            return device;
        }
    }
    None
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: BYTES,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut resource = None;
    unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut resource) }.unwrap();
    resource.unwrap()
}

/// Copy `source` to `destination` on `queue` and wait for it.
fn copy(device: &ID3D12Device, queue: &ID3D12CommandQueue, destination: &D3D12_TEXTURE_COPY_LOCATION, source: &D3D12_TEXTURE_COPY_LOCATION) {
    unsafe {
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
        list.CopyTextureRegion(destination, 0, 0, 0, source, None);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        wait(device, queue);
    }
}

fn wait(device: &ID3D12Device, queue: &ID3D12CommandQueue) {
    unsafe {
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        // A null event makes the call block until the value is reached.
        fence.SetEventOnCompletion(1, Default::default()).unwrap();
    }
}

#[test]
fn gpu_images_without_room_use_system_memory_and_keep_their_contents() {
    let Some(device) = hardware_device() else { return };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: SIDE.into(),
        Height: SIDE,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
        ..Default::default()
    };
    let queue: ID3D12CommandQueue = unsafe {
        device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())
    }
    .unwrap();
    let images: Vec<_> = (0..IMAGES).filter_map(|_| reserve(&device, &desc, None)).collect();
    if images.len() != IMAGES {
        return; // No reserved images on this adapter.
    }
    let meter = Meter::new(&device);
    let usage = |group| meter.segment(group).expect("the adapter reports its memory").CurrentUsage;
    let before = (usage(DXGI_MEMORY_SEGMENT_GROUP_LOCAL), usage(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL));
    let mut room = Room(Some(0));
    for (native, residency) in &images {
        assert!(residency.map(&device, &queue, native, desc.Flags, &mut room).unwrap());
        assert!(matches!(residency.place(), Place::System(bytes) if bytes >= BYTES));
    }
    wait(&device, &queue);
    let card = usage(DXGI_MEMORY_SEGMENT_GROUP_LOCAL).saturating_sub(before.0);
    let system = usage(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL).saturating_sub(before.1);
    let total = BYTES * IMAGES as u64;
    println!("{total} bytes of images: card +{card}, system +{system}");
    assert!(card < total / 4, "card +{card}");

    let upload = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    let readback = buffer(&device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    let pattern = |i: usize| (i.wrapping_mul(31) ^ (i >> 11)) as u8;
    let mut mapped = core::ptr::null_mut();
    unsafe { upload.Map(0, None, Some(&mut mapped)) }.unwrap();
    for i in 0..BYTES as usize {
        unsafe { mapped.cast::<u8>().add(i).write(pattern(i)) };
    }
    unsafe { upload.Unmap(0, None) };
    let linear = |resource: &ID3D12Resource| {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        unsafe { device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, None) };
        D3D12_TEXTURE_COPY_LOCATION {
            pResource: unsafe { core::mem::transmute_copy(resource) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        }
    };
    let image = D3D12_TEXTURE_COPY_LOCATION {
        pResource: unsafe { core::mem::transmute_copy(&images[0].0) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
    };
    copy(&device, &queue, &image, &linear(&upload));
    copy(&device, &queue, &linear(&readback), &image);
    unsafe { readback.Map(0, None, Some(&mut mapped)) }.unwrap();
    let read = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), BYTES as usize) };
    assert!(read.iter().enumerate().all(|(i, &b)| b == pattern(i)));
    unsafe { readback.Unmap(0, None) };
}
