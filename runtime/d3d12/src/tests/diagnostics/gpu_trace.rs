use super::unfinished;

#[test]
fn only_commands_started_and_not_finished_are_named_oldest_first() {
    // 1 finished, 2 and 5 running, 3 never reached, 4 slot reused by a later id.
    let slot = |id| match id {
        1 => (1, 1),
        2 => (2, 0),
        3 => (0, 0),
        4 => (9, 9),
        5 => (5, 1),
        _ => (0, 0),
    };
    assert_eq!(unfinished([5, 4, 3, 2, 1, 0].into_iter(), slot), [2, 5]);
}

#[test]
fn gpu_markers_land_in_the_ring() {
    use super::{create, write};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::core::Interface;
    let mut device: Option<ID3D12Device> = None;
    if unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_err() {
        return;
    }
    let device = device.unwrap();
    let ring = create(&device).unwrap();
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default()).unwrap();
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList2 =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
        write(&list, ring.address + 8, 1, D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN);
        write(&list, ring.address + 12, 1, D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        fence.SetEventOnCompletion(1, Default::default()).unwrap();
        let words = ring.words as *const u32;
        assert_eq!((words.add(2).read_volatile(), words.add(3).read_volatile()), (1, 1));
    }
}
