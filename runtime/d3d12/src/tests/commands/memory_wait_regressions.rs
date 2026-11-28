//! Regression tests for cross-submission wait elision, failed mid-list split
//! close semantics, null blend factor replay, and held-wait drop guards.
use super::*;
use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};

fn queue_signal(queue: &ComObj, fence: &ComObj, value: u64) {
    let sig: unsafe extern "system" fn(*mut c_void, *mut c_void, u64) -> HRESULT =
        unsafe { vslot(queue.0, 16) };
    let hr = unsafe { sig(queue.0, fence.0, value) };
    assert!(hr.is_ok(), "Queue::Signal failed: {hr:?}");
}

fn queue_wait(queue: &ComObj, fence: &ComObj, value: u64) {
    let wait: unsafe extern "system" fn(*mut c_void, *mut c_void, u64) -> HRESULT =
        unsafe { vslot(queue.0, 17) };
    let hr = unsafe { wait(queue.0, fence.0, value) };
    assert!(hr.is_ok(), "Queue::Wait failed: {hr:?}");
}

fn create_fence(device: &ComObj) -> ComObj {
    let mut fence = ptr::null_mut();
    let create_f: unsafe extern "system" fn(
        *mut c_void,
        u64,
        D3D12_FENCE_FLAGS,
        *const windows::core::GUID,
        *mut *mut c_void,
    ) -> HRESULT = unsafe { vslot(device.0, 36) };
    let hr = unsafe {
        create_f(
            device.0,
            0,
            D3D12_FENCE_FLAG_NONE,
            &ID3D12Fence::IID,
            &mut fence,
        )
    };
    assert!(hr.is_ok(), "CreateFence failed: {hr:?}");
    ComObj(fence)
}

/// F1 regression: an in-flight earlier submission writing to `flag` (`5 -> 0`)
/// must block wait elision on a separate subsequent `ExecuteCommandLists`, both
/// on the same queue and across queues ordered by `Queue::Signal`/`Queue::Wait`.
/// Once the write retires, subsequent read-only pre-satisfied waits still elide
/// with zero new fences.
#[test]
fn gpu_cross_submission_in_flight_write_blocks_unsafe_elision() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(device) = create_hardware_device() else {
        return;
    };
    let mut compute = GuestQueue::new(&device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    let mut copy = GuestQueue::new(&device, D3D12_COMMAND_LIST_TYPE_COPY);
    let mem = GuestMem::new(65536);

    // Part 1: Two separate ExecuteCommandLists calls on the SAME compute queue.
    // Offset 0 = gate, offset 8 = flag (starts 5), offset 16 = out (starts 0).
    mem.write64(0, 0);
    mem.write64(8, 5);
    mem.write64(16, 0);

    let list0 = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    list0.wait64(mem.addr(0), 3, 1); // gate == 1 (non-monotone, holds queue)
    list0.write64(mem.addr(8), 0); // resets flag 5 -> 0
    assert_eq!(list0.close(), S_OK);

    let list1 = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
    list1.wait64(mem.addr(8), 7, 5); // flag >= 5 (CPU currently sees 5!)
    list1.write64(mem.addr(16), 0xDEAD);
    assert_eq!(list1.close(), S_OK);

    let _t0 = compute.execute(&[&list0]);
    let t1 = compute.execute(&[&list1]);

    // Release list0 so it resets flag = 0 on the GPU.
    mem.write64(0, 1);
    let deadline = Instant::now();
    while mem.read64(8) != 0 {
        assert!(
            deadline.elapsed() < Duration::from_secs(2),
            "list0 did not reset flag to 0"
        );
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(40));
    assert_eq!(
        mem.read64(16),
        0,
        "F1 regression: list1 unsafely elided Wait(flag >= 5) across separate submissions"
    );

    mem.write64(8, 5);
    assert!(
        compute.wait_done(t1, 2000),
        "list1 did not release after flag reached 5"
    );
    assert_eq!(mem.read64(16), 0xDEAD);

    // Part 2: Cross-queue ordered via Queue::Signal / Queue::Wait (COPY -> COMPUTE).
    let handoff = create_fence(&device);
    mem.write64(0, 0);
    mem.write64(8, 5);
    mem.write64(16, 0);

    let copy_list = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_COPY);
    copy_list.wait64(mem.addr(0), 3, 2); // gate == 2
    copy_list.write64(mem.addr(8), 0); // resets flag 5 -> 0
    assert_eq!(copy_list.close(), S_OK);

    list1.reopen();
    list1.wait64(mem.addr(8), 7, 5); // flag >= 5
    list1.write64(mem.addr(16), 0xBEEF);
    assert_eq!(list1.close(), S_OK);

    let _tc = copy.execute(&[&copy_list]);
    queue_signal(&copy.queue, &handoff, 1);
    queue_wait(&compute.queue, &handoff, 1);
    let t2 = compute.execute(&[&list1]);

    mem.write64(0, 2);
    let deadline = Instant::now();
    while mem.read64(8) != 0 {
        assert!(
            deadline.elapsed() < Duration::from_secs(2),
            "copy_list did not reset flag to 0"
        );
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(Duration::from_millis(40));
    assert_eq!(
        mem.read64(16),
        0,
        "F1 regression: cross-queue ordered list1 unsafely elided Wait(flag >= 5)"
    );

    mem.write64(8, 5);
    assert!(
        compute.wait_done(t2, 2000),
        "cross-queue list1 did not release after flag reached 5"
    );
    assert_eq!(mem.read64(16), 0xBEEF);

    // Part 3: After in-flight writes retire, a read-only pre-satisfied wait on a
    // fresh queue still elides with zero persistent fence allocations.
    let mut fresh_queue = GuestQueue::new(&device, D3D12_COMMAND_LIST_TYPE_COPY);
    let fences_before = FENCES_CREATED.load(Ordering::Relaxed);
    let elided_before = WAITS_ELIDED.load(Ordering::Relaxed);
    let ro_list = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_COPY);
    ro_list.wait64(mem.addr(8), 7, 5); // flag is already 5 and all prior writes retired
    assert_eq!(ro_list.close(), S_OK);
    let tr = fresh_queue.execute(&[&ro_list]);
    assert!(fresh_queue.wait_done(tr, 2000));
    assert_eq!(
        FENCES_CREATED.load(Ordering::Relaxed) - fences_before,
        0,
        "retired write must not prevent zero-fence elision"
    );
    assert_eq!(
        WAITS_ELIDED.load(Ordering::Relaxed) - elided_before,
        1,
        "retired pre-satisfied wait should elide"
    );
}

/// F2 regression: when `split_active()` fails inside `native.Close()`, `phase`
/// must transition to `native_open: false` so the guest's subsequent `Close()`
/// does not double-close the native command list or overwrite an earlier error.
#[test]
fn gpu_failed_mid_list_split_close_does_not_double_close_or_clobber_error() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(device) = create_hardware_device() else {
        return;
    };
    let mem = GuestMem::new(65536);
    mem.write64(0, 1);

    let list = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    list.write64(mem.addr(8), 0x1234);

    // Pre-close the underlying native list so `split_active()`'s `native.Close()`
    // fails deterministically with `E_FAIL` across `vkd3d`, `vkd3d-proton` (which
    // accepts same-state transition barriers without failing `Close()`), and
    // Windows D3D12, while `state.phase` still expects `native_open: true`.
    let guest = unsafe { &*(list.list.0 as *const List) };
    let pre_close = unsafe { recording(guest).active.native.Close() };
    assert!(
        pre_close.is_ok(),
        "pre-closing native list failed: {pre_close:?}"
    );

    // Mid-list wait triggers split_active() -> native.Close() returns E_FAIL
    // and must immediately mark `native_open: false`.
    list.wait64(mem.addr(0), 7, 1);
    assert!(
        matches!(recording(guest).phase, ListState::Open { native_open: false }),
        "F2 regression: failed split_active() must mark native_open = false"
    );
    assert_eq!(recording(guest).error, E_FAIL);

    // Substitute a distinct sentinel error and verify guest Close() skips calling
    // `native.Close()` again and preserves the earlier error instead of overwriting it.
    recording(guest).error = E_INVALIDARG;
    let hr = list.close();
    assert_eq!(
        hr, E_INVALIDARG,
        "F2 regression: guest Close() returned {hr:?} instead of original E_INVALIDARG"
    );
    assert!(matches!(recording(guest).phase, ListState::Closed));

    // A second GuestList on the same device remains completely healthy.
    let ok_list = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    ok_list.write64(mem.addr(8), 0xCAFE);
    assert_eq!(ok_list.close(), S_OK);
}

/// F3 regression: `OMSetBlendFactor(NULL)` must not dereference NULL, must
/// default the tracked blend factor to `[1.0; 4]`, and must replay cleanly
/// across a mid-list split on a DIRECT command list.
#[test]
fn gpu_null_blend_factor_defaults_to_ones_and_survives_mid_list_split() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(device) = create_hardware_device() else {
        return;
    };
    let mut direct = GuestQueue::new(&device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    let mem = GuestMem::new(65536);
    mem.write64(0, 1);
    mem.write64(8, 0);
    mem.write64(16, 0);

    let list = GuestList::new(&device, D3D12_COMMAND_LIST_TYPE_DIRECT);
    let set_blend: unsafe extern "system" fn(*mut c_void, *const f32) =
        unsafe { vslot(list.list.0, 23) };
    // Must not access-violate when guest passes NULL.
    unsafe { set_blend(list.list.0, ptr::null()) };

    // Inspect tracked state directly through the proxy to assert exact [1.0; 4].
    let guest = unsafe { &*(list.list.0 as *const List) };
    assert_eq!(
        recording(guest).graphics_state.blend_factor,
        Some([1.0, 1.0, 1.0, 1.0]),
        "F3 regression: OMSetBlendFactor(NULL) must default to [1.0; 4]"
    );

    // Trigger a mid-list split so GraphicsState::replay replays OMSetBlendFactor.
    list.write64(mem.addr(8), 11);
    list.wait64(mem.addr(0), 7, 1);
    list.write64(mem.addr(16), 22);
    assert_eq!(list.close(), S_OK);

    let ticket = direct.execute(&[&list]);
    assert!(direct.wait_done(ticket, 2000));
    assert_eq!(mem.read64(8), 11);
    assert_eq!(mem.read64(16), 22);
}

/// F4 regression: `PendingWaits` and `Job::Drop` release every counted wait on
/// early submission errors, normal completion, and worker channel teardown
/// without leaking or double-decrementing.
#[test]
fn pending_waits_and_job_drop_guards_never_leak_or_double_decrement() {
    use crate::device::placement::held::Waits;

    // 1. Early return before handing off any split releases full count.
    let w1 = Waits::new(3.into());
    {
        let _pending = PendingWaits::new(Some(w1.clone()), 3);
    }
    assert_eq!(w1.load(Ordering::Acquire), 0);

    // 2. Partial handoff followed by early failure releases remaining counts.
    let w2 = Waits::new(4.into());
    let handed: Option<Waits>;
    {
        let mut pending = PendingWaits::new(Some(w2.clone()), 4);
        handed = pending.take_one();
        assert_eq!(pending.remaining, 3);
    }
    assert_eq!(w2.load(Ordering::Acquire), 1);
    handed.unwrap().fetch_sub(1, Ordering::AcqRel);
    assert_eq!(w2.load(Ordering::Acquire), 0);

    // 3. Real Job Drop guard: calling release_wait() + drop decrements once,
    //    and dropping an unprocessed queued Job decrements once.
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(guest_device) = create_hardware_device() else {
        return;
    };
    let mem = GuestMem::new(65536);
    let list = GuestList::new(&guest_device, D3D12_COMMAND_LIST_TYPE_COPY);
    let proxy = unsafe { &*(list.list.0 as *const List) };
    let mut native_device: Option<ID3D12Device> = None;
    unsafe { proxy.native.GetDevice(&mut native_device).unwrap() };
    let native_device = native_device.unwrap();
    let fence: ID3D12Fence =
        unsafe { native_device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap() };
    let word = Word::map(proxy, mem.addr(0)).unwrap();
    let segment = Arc::new(Segment {
        prefix: None,
        word,
        address: mem.addr(0),
        comparison: Comparison::GreaterEqual,
        target: 1,
        prior_write_in_list: false,
    });

    let w3 = Waits::new(2.into());
    let mut pending = PendingWaits::new(Some(w3.clone()), 2);
    let mut job_completed = Job {
        kind: 0,
        fence: fence.clone(),
        signal_val: 1,
        release_val: 2,
        segment: segment.clone(),
        device: native_device.clone(),
        queue: 0,
        waits: pending.take_one(),
    };
    let job_dropped_unprocessed = Job {
        kind: 0,
        fence,
        signal_val: 3,
        release_val: 4,
        segment,
        device: native_device,
        queue: 0,
        waits: pending.take_one(),
    };
    drop(pending);
    assert_eq!(w3.load(Ordering::Acquire), 2);

    // Complete job 1 explicitly, then drop it -> must decrement exactly once.
    job_completed.release_wait();
    assert_eq!(w3.load(Ordering::Acquire), 1);
    drop(job_completed);
    assert_eq!(w3.load(Ordering::Acquire), 1);

    // Drop queued job 2 without running it (simulating worker break) -> reaches 0.
    drop(job_dropped_unprocessed);
    assert_eq!(w3.load(Ordering::Acquire), 0);
}
