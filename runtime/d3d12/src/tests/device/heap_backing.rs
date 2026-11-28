use super::*;
use super::super::PagePools;
use crate::commands::page_mapping::GPU_APERTURE;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{
    E_INVALIDARG, E_NOTIMPL, ERROR_INVALID_ADDRESS, ERROR_MORE_DATA, ERROR_NOT_FOUND,
    ERROR_NOT_SUPPORTED, SetLastError, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

const PAGE: u64 = 65536;

// The fake XMem below is process-wide state.
static SERIAL: Mutex<()> = Mutex::new(());
// (guest page, backing) pairs the fake resolves; anything else is not XMem's.
static MAP: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static RUNS_MAP: Mutex<Vec<(u64, Vec<BackingRun>)>> = Mutex::new(Vec::new());
static FAIL_WITH: AtomicU64 = AtomicU64::new(0);
static ACQUIRED: AtomicU64 = AtomicU64::new(0);
static RELEASED: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn fake_acquire(address: u64, bytes: u64, backing: *mut u64, hold: *mut u64) -> i32 {
    let fail = FAIL_WITH.load(Ordering::Acquire);
    if fail != 0 {
        unsafe { SetLastError(windows::Win32::Foundation::WIN32_ERROR(fail as u32)) };
        return 0;
    }
    let map = MAP.lock().unwrap();
    if let Some(&(_, start)) = map.iter().find(|(guest, _)| *guest == address) {
        assert_eq!(bytes % PAGE, 0);
        let cookie = ACQUIRED.fetch_add(1, Ordering::AcqRel) + 1;
        unsafe {
            backing.write(start);
            hold.write(cookie);
        }
        return 1;
    }
    drop(map);
    let runs_map = RUNS_MAP.lock().unwrap();
    if runs_map.iter().any(|(guest, _)| *guest == address) {
        unsafe { SetLastError(ERROR_NOT_SUPPORTED) };
        return 0;
    }
    unsafe { SetLastError(ERROR_NOT_FOUND) };
    0
}

unsafe extern "system" fn fake_acquire_runs(
    address: u64,
    bytes: u64,
    out_runs: *mut BackingRun,
    count: *mut u32,
    hold: *mut u64,
) -> i32 {
    let fail = FAIL_WITH.load(Ordering::Acquire);
    if fail != 0 {
        unsafe { SetLastError(windows::Win32::Foundation::WIN32_ERROR(fail as u32)) };
        return 0;
    }
    let runs_map = RUNS_MAP.lock().unwrap();
    let Some((_, runs)) = runs_map.iter().find(|(guest, _)| *guest == address) else {
        unsafe { SetLastError(ERROR_NOT_FOUND) };
        return 0;
    };
    assert_eq!(bytes % PAGE, 0);
    let capacity = unsafe { count.read() } as usize;
    unsafe { count.write(runs.len() as u32) };
    if capacity < runs.len() {
        unsafe { SetLastError(ERROR_MORE_DATA) };
        return 0;
    }
    for (i, run) in runs.iter().enumerate() {
        unsafe { out_runs.add(i).write(*run) };
    }
    let cookie = ACQUIRED.fetch_add(1, Ordering::AcqRel) + 1;
    unsafe { hold.write(cookie) };
    1
}

unsafe extern "system" fn fake_release(_hold: u64) {
    RELEASED.fetch_add(1, Ordering::AcqRel);
}

struct FakeXmem(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl FakeXmem {
    fn reset_state(map: &[(u64, u64)], runs: &[(u64, Vec<BackingRun>)]) -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        *MAP.lock().unwrap() = map.to_vec();
        *RUNS_MAP.lock().unwrap() = runs.to_vec();
        FAIL_WITH.store(0, Ordering::Release);
        ACQUIRED.store(0, Ordering::Release);
        RELEASED.store(0, Ordering::Release);
        guard
    }

    fn install(map: &[(u64, u64)]) -> Self {
        let guard = Self::reset_state(map, &[]);
        *TEST_EXPORTS.lock().unwrap() = Some(Exports::new(fake_acquire, fake_release));
        Self(guard)
    }

    fn install_runs(runs: &[(u64, Vec<BackingRun>)]) -> Self {
        let guard = Self::reset_state(&[], runs);
        *TEST_EXPORTS.lock().unwrap() =
            Some(Exports::with_runs(fake_acquire, fake_acquire_runs, fake_release));
        Self(guard)
    }
}

impl Drop for FakeXmem {
    fn drop(&mut self) {
        *TEST_EXPORTS.lock().unwrap() = None;
        MAP.lock().unwrap().clear();
        RUNS_MAP.lock().unwrap().clear();
    }
}

#[test]
fn without_backing_exports_the_address_is_imported() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // The test process has no xmem.dll.
    let backing = acquire(0x7_0000_0000, PAGE).unwrap();
    assert_eq!(backing.address, 0x7_0000_0000);
    assert!(backing.hold.is_none());
}

#[test]
fn memory_outside_xmem_is_imported_at_its_address() {
    let _xmem = FakeXmem::install(&[]);
    let backing = acquire(0x7_0001_0000, PAGE).unwrap();
    assert_eq!(backing.address, 0x7_0001_0000);
    assert!(backing.hold.is_none());
    assert_eq!(ACQUIRED.load(Ordering::Acquire), 0);
}

#[test]
fn an_unusable_backing_fails_the_import() {
    let _xmem = FakeXmem::install(&[]);
    FAIL_WITH.store(ERROR_NOT_SUPPORTED.0 as u64, Ordering::Release);
    let error = acquire(0x7_0002_0000, 2 * PAGE).err().expect("scattered backing must fail");
    assert_eq!(error.code(), windows::core::HRESULT::from_win32(ERROR_NOT_SUPPORTED.0));
}

#[test]
fn a_backing_is_held_until_dropped_and_released_once() {
    let _xmem = FakeXmem::install(&[(0x7_0003_0000, 0x7_0100_0000)]);
    let backing = acquire(0x7_0003_0000, PAGE).unwrap();
    assert_eq!(backing.address, 0x7_0100_0000);
    assert!(backing.hold.is_some());
    assert_eq!((ACQUIRED.load(Ordering::Acquire), RELEASED.load(Ordering::Acquire)), (1, 0));
    drop(backing);
    assert_eq!(RELEASED.load(Ordering::Acquire), 1);
}

struct Region(*mut core::ffi::c_void);

impl Region {
    fn private(bytes: usize) -> Self {
        let p = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!p.is_null());
        Self(p)
    }

    // A GPU-only alias: an address with no CPU memory behind it.
    fn reserved(bytes: usize) -> Self {
        let p = unsafe { VirtualAlloc(None, bytes, MEM_RESERVE, PAGE_NOACCESS) };
        assert!(!p.is_null());
        Self(p)
    }

    fn address(&self) -> u64 {
        self.0 as u64
    }

    fn words(&self, count: usize) -> &mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.0.cast(), count) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe { VirtualFree(self.0, 0, MEM_RELEASE).unwrap() };
    }
}

fn hardware_device() -> Option<ID3D12Device13> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        index += 1;
        if unsafe { adapter.GetDesc1() }.is_ok_and(|desc| desc.Flags & 2 != 0) {
            continue;
        }
        let mut device: Option<ID3D12Device> = None;
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_ok()
            && let Some(device) = device.and_then(|d| d.cast::<ID3D12Device13>().ok())
        {
            return Some(device);
        }
    }
    None
}

fn buffer_desc(width: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_CROSS_ADAPTER,
        ..Default::default()
    }
}

fn placed(device: &ID3D12Device13, heap: &ID3D12Heap) -> ID3D12Resource {
    let mut resource = None;
    unsafe {
        device.CreatePlacedResource(heap, 0, &buffer_desc(PAGE), D3D12_RESOURCE_STATE_COMMON, None, &mut resource)
    }
    .unwrap();
    resource.unwrap()
}

fn committed(device: &ID3D12Device13, kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC { Flags: D3D12_RESOURCE_FLAG_NONE, ..buffer_desc(PAGE) };
    let mut resource = None;
    unsafe { device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut resource) }
        .unwrap();
    resource.unwrap()
}

/// Run `record` on a direct queue and wait for it.
fn submit(device: &ID3D12Device13, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
    let desc = D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() };
    let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&desc) }.unwrap();
    let allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }.unwrap();
    let list: ID3D12GraphicsCommandList =
        unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None::<&ID3D12PipelineState>) }.unwrap();
    record(&list);
    unsafe { list.Close() }.unwrap();
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
    let event = unsafe { CreateEventW(None, false, false, None) }.unwrap();
    unsafe {
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        queue.Signal(&fence, 1).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        assert_eq!(WaitForSingleObject(event, 10_000), WAIT_OBJECT_0, "GPU copy did not finish");
        let _ = windows::Win32::Foundation::CloseHandle(event);
    }
}

// Two GPU aliases of one private page, as XMem would hand them out: imports of
// either address must reach the same memory, and the CPU sees GPU writes.
#[test]
fn aliases_import_their_private_backing() {
    let Some(device) = hardware_device() else { return };
    let home = Region::private(PAGE as usize);
    let (first, second) = (Region::reserved(PAGE as usize), Region::reserved(PAGE as usize));
    let xmem = FakeXmem::install(&[(first.address(), home.address()), (second.address(), home.address())]);
    for (i, word) in home.words(16).iter_mut().enumerate() {
        *word = 0x5ec7_0000 + i as u32;
    }

    let pools = PagePools::default();
    let import = |guest: &Region| {
        let backing = acquire(guest.address(), PAGE).unwrap();
        assert_eq!(backing.address, home.address());
        let pin = GPU_APERTURE.pin(guest.address(), PAGE).unwrap();
        pools.import(&device, guest.address(), backing, PAGE, pin, "test", false).unwrap()
    };
    let heap_first = import(&first);
    // The same guest page with the same backing reuses its live import.
    assert!(Arc::ptr_eq(&heap_first, &import(&first)));
    let heap_second = import(&second);
    assert!(!Arc::ptr_eq(&heap_first, &heap_second));

    let via_first = placed(&device, &heap_first.native);
    let via_second = placed(&device, &heap_second.native);
    let upload = committed(&device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    let readback = committed(&device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut mapped = core::ptr::null_mut();
    unsafe { upload.Map(0, None, Some(&mut mapped)) }.unwrap();
    let gpu_words: [u32; 4] = [0x00c0_ffee, 0x0bad_f00d, 0x7e57_c0de, 0x0ddb_a115];
    unsafe { core::ptr::copy_nonoverlapping(gpu_words.as_ptr(), mapped.cast::<u32>().add(8), 4) };

    // The GPU reads the CPU's bytes through the first alias, writes through
    // it, and reads that write back through the second alias. A UAV barrier
    // orders unordered-access work, not copies, and the two aliases are
    // separate resources on one memory, so no barrier in one list orders the
    // copy through the second after the write through the first. Each step is
    // its own submission instead. One list with a UAV barrier passes under
    // vkd3d-proton but reads the old bytes on Windows' own runtime.
    submit(&device, |list| unsafe { list.CopyBufferRegion(&readback, 0, &via_first, 0, 32) });
    submit(&device, |list| unsafe { list.CopyBufferRegion(&via_first, 32, &upload, 32, 16) });
    submit(&device, |list| unsafe { list.CopyBufferRegion(&readback, 32, &via_second, 32, 16) });
    let mut out = core::ptr::null_mut();
    unsafe { readback.Map(0, None, Some(&mut out)) }.unwrap();
    let seen = unsafe { core::slice::from_raw_parts(out.cast::<u32>(), 12) };
    let expected: Vec<u32> = (0..8).map(|i| 0x5ec7_0000 + i).collect();
    assert_eq!(&seen[..8], &expected[..], "GPU did not read the backing through an alias");
    assert_eq!(&seen[8..12], &gpu_words, "a GPU write through one alias is not visible through the other");
    assert_eq!(&home.words(12)[8..12], &gpu_words, "the CPU does not see the GPU write");

    // Holds outlive every native heap and are released once each.
    assert_eq!(ACQUIRED.load(Ordering::Acquire), 3);
    assert_eq!(RELEASED.load(Ordering::Acquire), 1, "only the reused import's extra hold is gone");
    drop((via_first, via_second));
    drop(heap_first);
    assert_eq!(RELEASED.load(Ordering::Acquire), 2);
    drop(heap_second);
    assert_eq!(RELEASED.load(Ordering::Acquire), 3);
    drop(xmem);
}

// A guest page whose backing moved must not reuse the import of the old one.
#[test]
fn a_moved_backing_is_imported_again() {
    let Some(device) = hardware_device() else { return };
    let (old, new) = (Region::private(PAGE as usize), Region::private(PAGE as usize));
    let guest = Region::reserved(PAGE as usize);
    let xmem = FakeXmem::install(&[(guest.address(), old.address())]);
    let pools = PagePools::default();
    let import = || {
        let backing = acquire(guest.address(), PAGE).unwrap();
        let pin = GPU_APERTURE.pin(guest.address(), PAGE).unwrap();
        pools.import(&device, guest.address(), backing, PAGE, pin, "test", false).unwrap()
    };
    let before = import();
    *MAP.lock().unwrap() = vec![(guest.address(), new.address())];
    let after = import();
    assert!(!Arc::ptr_eq(&before, &after));
    drop((before, after));
    assert_eq!(RELEASED.load(Ordering::Acquire), ACQUIRED.load(Ordering::Acquire));
    drop(xmem);
}

#[test]
fn scattered_backing_runs_are_acquired_and_released_once() {
    let expected = vec![
        BackingRun { address: 0x7_0100_0000, bytes: PAGE },
        BackingRun { address: 0x7_0300_0000, bytes: 2 * PAGE },
    ];
    let _xmem = FakeXmem::install_runs(&[(0x7_0004_0000, expected.clone())]);
    let acquired = acquire_backing(0x7_0004_0000, 3 * PAGE).unwrap();
    let AcquiredBacking::Scattered { runs, hold } = acquired else {
        panic!("expected scattered backing");
    };
    assert_eq!(runs, expected);
    assert_eq!((ACQUIRED.load(Ordering::Acquire), RELEASED.load(Ordering::Acquire)), (1, 0));
    drop(hold);
    assert_eq!(RELEASED.load(Ordering::Acquire), 1);
}

#[test]
fn scattered_backing_retries_when_runs_exceed_inline_capacity() {
    let expected: Vec<BackingRun> = (0..20u64)
        .map(|i| BackingRun { address: 0x7_1000_0000 + i * (2 * PAGE), bytes: PAGE })
        .collect();
    let _xmem = FakeXmem::install_runs(&[(0x7_0005_0000, expected.clone())]);
    let AcquiredBacking::Scattered { runs, hold } = acquire_backing(0x7_0005_0000, 20 * PAGE).unwrap() else {
        panic!("expected scattered backing after retry");
    };
    assert_eq!(runs, expected);
    assert_eq!((ACQUIRED.load(Ordering::Acquire), RELEASED.load(Ordering::Acquire)), (1, 0));
    drop(hold);
    assert_eq!(RELEASED.load(Ordering::Acquire), 1);
}

#[test]
fn scattered_backing_invalid_runs_release_hold_and_fail() {
    let unaligned = vec![
        BackingRun { address: 0x7_0100_1000, bytes: PAGE },
        BackingRun { address: 0x7_0200_0000, bytes: PAGE },
    ];
    let wrong_total = vec![
        BackingRun { address: 0x7_0100_0000, bytes: PAGE },
        BackingRun { address: 0x7_0200_0000, bytes: 2 * PAGE },
    ];
    let _xmem = FakeXmem::install_runs(&[
        (0x7_0006_0000, unaligned),
        (0x7_0007_0000, wrong_total),
    ]);
    let err1 = acquire_backing(0x7_0006_0000, 2 * PAGE).err().expect("unaligned run must fail");
    let err2 = acquire_backing(0x7_0007_0000, 2 * PAGE).err().expect("mismatched run total must fail");
    assert_eq!(err1.code(), E_INVALIDARG);
    assert_eq!(err2.code(), E_INVALIDARG);
    assert_eq!((ACQUIRED.load(Ordering::Acquire), RELEASED.load(Ordering::Acquire)), (2, 2));
}

#[test]
fn scattered_backing_single_run_collapses_to_contiguous() {
    let single = vec![BackingRun { address: 0x7_0400_0000, bytes: 2 * PAGE }];
    let _xmem = FakeXmem::install_runs(&[(0x7_0008_0000, single)]);
    let AcquiredBacking::Contiguous(backing) = acquire_backing(0x7_0008_0000, 2 * PAGE).unwrap() else {
        panic!("single-run scattered response should collapse to contiguous");
    };
    assert_eq!(backing.address, 0x7_0400_0000);
    assert!(backing.hold.is_some());
    drop(backing);
    assert_eq!((ACQUIRED.load(Ordering::Acquire), RELEASED.load(Ordering::Acquire)), (1, 1));
}

#[test]
fn scattered_alias_imports_through_reserved_buffer_and_aliases_cpu_and_gpu() {
    let Some(device) = hardware_device() else { return };
    let native: ID3D12Device = device.cast().unwrap();
    let a = Region::private(PAGE as usize);
    let b = Region::private(PAGE as usize);
    let c = Region::private(PAGE as usize);
    let alias_abc = Region::reserved((3 * PAGE) as usize);
    let alias_bc = Region::reserved((2 * PAGE) as usize);

    a.words(4).copy_from_slice(&[0xaaaa_0001, 0xaaaa_0002, 0xaaaa_0003, 0xaaaa_0004]);
    b.words(4).copy_from_slice(&[0xbbbb_0001, 0xbbbb_0002, 0xbbbb_0003, 0xbbbb_0004]);
    c.words(4).copy_from_slice(&[0xcccc_0001, 0xcccc_0002, 0xcccc_0003, 0xcccc_0004]);

    let xmem = FakeXmem::install_runs(&[
        (
            alias_abc.address(),
            vec![
                BackingRun { address: c.address(), bytes: PAGE },
                BackingRun { address: a.address(), bytes: PAGE },
                BackingRun { address: b.address(), bytes: PAGE },
            ],
        ),
        (
            alias_bc.address(),
            vec![
                BackingRun { address: b.address(), bytes: PAGE },
                BackingRun { address: c.address(), bytes: PAGE },
            ],
        ),
    ]);
    let pools = PagePools::default();
    let init_transition = Mutex::new(None);
    let import = |guest: &Region, offset: u64, width: u64| {
        let size = (offset + width).next_multiple_of(PAGE);
        let AcquiredBacking::Scattered { runs, hold } = acquire_backing(guest.address(), size).unwrap() else {
            panic!("expected scattered backing");
        };
        let pin = GPU_APERTURE.pin(guest.address(), size).unwrap();
        let desc = D3D12_RESOURCE_DESC {
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..buffer_desc(width)
        };
        super::super::import::import_scattered_buffer(
            &native,
            &pools,
            &init_transition,
            offset,
            &desc,
            D3D12_RESOURCE_STATE_COMMON,
            runs,
            hold,
            pin,
            "test",
            false,
        )
    };

    // Sub-page offsets fail cleanly and still release the acquired hold.
    let offset_err = import(&alias_abc, 256, 2 * PAGE + 256)
        .err()
        .expect("non-zero offset must return E_NOTIMPL");
    assert_eq!(offset_err.code(), E_NOTIMPL);
    assert_eq!((ACQUIRED.load(Ordering::Acquire), RELEASED.load(Ordering::Acquire)), (1, 1));

    // Non-64KiB-multiple width spanning all 3 discrete backing runs `[c, a, b]`.
    let (res_abc, heap_abc) = import(&alias_abc, 0, 2 * PAGE + 1024).unwrap();
    let (res_bc, heap_bc) = import(&alias_bc, 0, 2 * PAGE).unwrap();
    assert!(!heap_abc.reusable_for(0));
    assert_eq!(
        heap_abc.buffer_alias(&native, 256, 256, 16, D3D12_RESOURCE_FLAG_NONE).err().unwrap().code(),
        E_NOTIMPL
    );

    let upload = committed(&device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    let readback = committed(&device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut mapped = core::ptr::null_mut();
    unsafe { upload.Map(0, None, Some(&mut mapped)) }.unwrap();
    let write_c: [u32; 4] = [0x1111_0001, 0x1111_0002, 0x1111_0003, 0x1111_0004];
    let write_a: [u32; 4] = [0x2222_0001, 0x2222_0002, 0x2222_0003, 0x2222_0004];
    let write_b: [u32; 4] = [0x3333_0001, 0x3333_0002, 0x3333_0003, 0x3333_0004];
    unsafe {
        core::ptr::copy_nonoverlapping(write_c.as_ptr(), mapped.cast::<u32>(), 4);
        core::ptr::copy_nonoverlapping(write_a.as_ptr(), mapped.cast::<u32>().add(4), 4);
        core::ptr::copy_nonoverlapping(write_b.as_ptr(), mapped.cast::<u32>().add(8), 4);
    }

    // Each step is its own submission: see `aliases_import_their_private_backing`.
    submit(&device, |list| unsafe {
        // Read initial CPU markers across all 3 tiles: tile 0=c, tile 1=a, tile 2=b.
        list.CopyBufferRegion(&readback, 0, &res_abc, 0, 16);
        list.CopyBufferRegion(&readback, 16, &res_abc, PAGE, 16);
        list.CopyBufferRegion(&readback, 32, &res_abc, 2 * PAGE, 16);
    });
    submit(&device, |list| unsafe {
        // Write into all 3 tiles (including tail tile 2 < width=2*PAGE+1024) via res_abc.
        list.CopyBufferRegion(&res_abc, 64, &upload, 0, 16);
        list.CopyBufferRegion(&res_abc, PAGE + 64, &upload, 16, 16);
        list.CopyBufferRegion(&res_abc, 2 * PAGE + 64, &upload, 32, 16);
    });
    submit(&device, |list| unsafe {
        // Read GPU writes back via the second alias `[b, c]`.
        list.CopyBufferRegion(&readback, 64, &res_bc, 64, 16);
        list.CopyBufferRegion(&readback, 80, &res_bc, PAGE + 64, 16);
    });

    let mut out = core::ptr::null_mut();
    unsafe { readback.Map(0, None, Some(&mut out)) }.unwrap();
    let seen = unsafe { core::slice::from_raw_parts(out.cast::<u32>(), 24) };
    assert_eq!(&seen[0..4], &[0xcccc_0001, 0xcccc_0002, 0xcccc_0003, 0xcccc_0004]);
    assert_eq!(&seen[4..8], &[0xaaaa_0001, 0xaaaa_0002, 0xaaaa_0003, 0xaaaa_0004]);
    assert_eq!(&seen[8..12], &[0xbbbb_0001, 0xbbbb_0002, 0xbbbb_0003, 0xbbbb_0004]);
    assert_eq!(&seen[16..20], &write_b);
    assert_eq!(&seen[20..24], &write_c);
    assert_eq!(&c.words(20)[16..20], &write_c);
    assert_eq!(&a.words(20)[16..20], &write_a);
    assert_eq!(&b.words(20)[16..20], &write_b);

    drop((res_abc, heap_abc));
    drop((res_bc, heap_bc));
    assert_eq!(RELEASED.load(Ordering::Acquire), ACQUIRED.load(Ordering::Acquire));

    // Releasing scattered regions and holds must not leave dangling VMAs that poison later submits.
    drop((a, b, c));
    submit(&device, |list| unsafe {
        list.CopyBufferRegion(&readback, 0, &upload, 0, 16);
    });
    drop(xmem);
}

#[test]
fn placed_resource_keeps_backing_hold_alive_after_imported_heap_drops() {
    let Some(device) = hardware_device() else { return };
    let home = Region::private(PAGE as usize);
    let guest = Region::reserved(PAGE as usize);
    let xmem = FakeXmem::install(&[(guest.address(), home.address())]);

    let pools = PagePools::default();
    let backing = acquire(guest.address(), PAGE).unwrap();
    let pin = GPU_APERTURE.pin(guest.address(), PAGE).unwrap();
    let heap_first = pools.import(&device, guest.address(), backing, PAGE, pin, "test", false).unwrap();
    let via_first = placed(&device, &heap_first.native);
    super::super::attach_import(&via_first, &heap_first).unwrap();

    drop(heap_first);
    assert_eq!(
        RELEASED.load(Ordering::Acquire),
        0,
        "placed resource must keep the BackingGuard hold alive through ID3D12Heap"
    );

    drop(via_first);
    assert_eq!(RELEASED.load(Ordering::Acquire), 1);
    drop(xmem);
}

#[test]
fn adjacent_placeholder_private_spans_import_check() {
    use windows::Win32::System::Memory::{
        MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE_PLACEHOLDER,
        VIRTUAL_FREE_TYPE, VirtualAlloc2,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    let Some(device) = hardware_device() else { return };
    let process = unsafe { GetCurrentProcess() };
    let base = unsafe {
        VirtualAlloc2(
            Some(process),
            None,
            (2 * PAGE) as usize,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS.0,
            None,
        )
    };
    assert!(!base.is_null());
    unsafe { VirtualFree(base, PAGE as usize, VIRTUAL_FREE_TYPE(MEM_RELEASE.0 | MEM_PRESERVE_PLACEHOLDER.0)) }.unwrap();
    let page0 = unsafe {
        VirtualAlloc2(
            Some(process),
            Some(base),
            PAGE as usize,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE.0,
            None,
        )
    };
    assert_eq!(page0, base);
    let page1_addr = unsafe { base.cast::<u8>().add(PAGE as usize) };
    let page1 = unsafe {
        VirtualAlloc2(
            Some(process),
            Some(page1_addr.cast()),
            PAGE as usize,
            MEM_RESERVE | MEM_COMMIT | MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE.0,
            None,
        )
    };
    assert_eq!(page1, page1_addr.cast());

    let p0_words = unsafe { core::slice::from_raw_parts_mut(page0.cast::<u32>(), 4) };
    let p1_words = unsafe { core::slice::from_raw_parts_mut(page1.cast::<u32>(), 4) };
    p0_words.copy_from_slice(&[0x1010_0001, 0x1010_0002, 0x1010_0003, 0x1010_0004]);
    p1_words.copy_from_slice(&[0x2020_0001, 0x2020_0002, 0x2020_0003, 0x2020_0004]);

    let heap = unsafe { device.OpenExistingHeapFromAddress1::<ID3D12Heap>(page0, (2 * PAGE) as usize) }
        .expect("adjacent placeholder PrivateSpans import as one heap");
    let mut res: Option<ID3D12Resource> = None;
    unsafe {
        device.CreatePlacedResource(
            &heap,
            0,
            &buffer_desc(2 * PAGE),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut res,
        )
    }
    .unwrap();
    let res = res.unwrap();
    let readback = committed(&device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    submit(&device, |list| unsafe {
        list.CopyBufferRegion(&readback, 0, &res, 0, 16);
        list.CopyBufferRegion(&readback, 16, &res, PAGE, 16);
    });
    let mut out = core::ptr::null_mut();
    unsafe { readback.Map(0, None, Some(&mut out)) }.unwrap();
    let seen = unsafe { core::slice::from_raw_parts(out.cast::<u32>(), 8) };
    assert_eq!(
        seen,
        &[
            0x1010_0001, 0x1010_0002, 0x1010_0003, 0x1010_0004,
            0x2020_0001, 0x2020_0002, 0x2020_0003, 0x2020_0004,
        ]
    );
    drop((res, readback, heap));

    let _ = unsafe { VirtualFree(page0, 0, MEM_RELEASE) };
    let _ = unsafe { VirtualFree(page1, 0, MEM_RELEASE) };
}

#[test]
fn retire_range_invalidates_imported_heap_and_prevents_cache_reuse() {
    let Some(device) = hardware_device() else { return };
    let home_a = Region::private(PAGE as usize);
    let home_b = Region::private(PAGE as usize);
    let guest = Region::reserved(PAGE as usize);
    let xmem = FakeXmem::install(&[(guest.address(), home_a.address())]);

    let pools = PagePools::default();
    let backing_a = acquire(guest.address(), PAGE).unwrap();
    let pin_a = GPU_APERTURE.pin(guest.address(), PAGE).unwrap();
    let heap_a = pools
        .import(&device, guest.address(), backing_a, PAGE, pin_a, "test", false)
        .unwrap();
    assert!(heap_a.is_live());

    unsafe { crate::commands::page_mapping::retire_from_xmem(guest.address(), PAGE) };
    assert!(!heap_a.is_live());
    assert!(!heap_a.reusable_for(home_a.address()));

    *MAP.lock().unwrap() = vec![(guest.address(), home_b.address())];
    let backing_b = acquire(guest.address(), PAGE).unwrap();
    let pin_b = GPU_APERTURE.pin(guest.address(), PAGE).unwrap();
    let heap_b = pools
        .import(&device, guest.address(), backing_b, PAGE, pin_b, "test", false)
        .unwrap();
    assert!(!Arc::ptr_eq(&heap_a, &heap_b));
    assert!(heap_b.is_live());

    drop((heap_a, heap_b));
    assert_eq!(RELEASED.load(Ordering::Acquire), ACQUIRED.load(Ordering::Acquire));
    drop(xmem);
}

#[test]
fn scattered_imports_reuse_cached_piece_heaps() {
    let Some(device) = hardware_device() else { return };
    let native = device.clone();
    let a = Region::private(PAGE as usize);
    let b = Region::private(PAGE as usize);
    let c = Region::private(PAGE as usize);
    let guest_ab = Region::reserved((2 * PAGE) as usize);
    let guest_ac = Region::reserved((2 * PAGE) as usize);

    a.words(4).copy_from_slice(&[0x1010_0001, 0x1010_0002, 0x1010_0003, 0x1010_0004]);
    let xmem = FakeXmem::install_runs(&[
        (
            guest_ab.address(),
            vec![
                BackingRun { address: a.address(), bytes: PAGE },
                BackingRun { address: b.address(), bytes: PAGE },
            ],
        ),
        (
            guest_ac.address(),
            vec![
                BackingRun { address: a.address(), bytes: PAGE },
                BackingRun { address: c.address(), bytes: PAGE },
            ],
        ),
    ]);

    let pools = PagePools::default();
    let init_transition = Mutex::new(None);
    let import = |guest: &Region| {
        let AcquiredBacking::Scattered { runs, hold } = acquire_backing(guest.address(), 2 * PAGE).unwrap() else {
            panic!("expected scattered backing");
        };
        let pin = GPU_APERTURE.pin(guest.address(), 2 * PAGE).unwrap();
        super::super::import::import_scattered_buffer(
            &native,
            &pools,
            &init_transition,
            0,
            &buffer_desc(2 * PAGE),
            D3D12_RESOURCE_STATE_COMMON,
            runs,
            hold,
            pin,
            "test",
            false,
        )
        .unwrap()
    };

    let (res_ab, heap_ab) = import(&guest_ab);
    let (res_ac, heap_ac) = import(&guest_ac);
    assert_eq!(heap_ab.native.as_raw(), heap_ac.native.as_raw());

    drop((res_ab, heap_ab));
    let readback = committed(&device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    submit(&device, |list| unsafe {
        list.CopyBufferRegion(&readback, 0, &res_ac, 0, 16);
    });
    let mut out = core::ptr::null_mut();
    unsafe { readback.Map(0, None, Some(&mut out)) }.unwrap();
    let seen = unsafe { core::slice::from_raw_parts(out.cast::<u32>(), 4) };
    assert_eq!(seen, &[0x1010_0001, 0x1010_0002, 0x1010_0003, 0x1010_0004]);

    drop((readback, res_ac, heap_ac));
    assert_eq!(RELEASED.load(Ordering::Acquire), ACQUIRED.load(Ordering::Acquire));
    drop(xmem);
}

#[test]
fn try_write_xmem_bytes_skips_unmapped_xmem_pages_without_failing() {
    let guest = Region::reserved((2 * PAGE) as usize);
    let backing = Region::private(PAGE as usize);
    let xmem = FakeXmem::install(&[(guest.address(), backing.address())]);
    unsafe extern "system" fn partial_acquire(
        address: u64,
        bytes: u64,
        out_backing: *mut u64,
        hold: *mut u64,
    ) -> i32 {
        let map = MAP.lock().unwrap();
        if bytes == PAGE
            && let Some(&(_, start)) = map.iter().find(|(g, _)| *g == address)
        {
            let cookie = ACQUIRED.fetch_add(1, Ordering::AcqRel) + 1;
            unsafe {
                out_backing.write(start);
                hold.write(cookie);
            }
            return 1;
        }
        unsafe { SetLastError(ERROR_INVALID_ADDRESS) };
        0
    }
    *TEST_EXPORTS.lock().unwrap() = Some(Exports::new(partial_acquire, fake_release));

    let mut payload = vec![0x11u8; (2 * PAGE) as usize];
    payload[PAGE as usize..].fill(0x22);
    assert!(matches!(try_write_xmem_bytes(guest.address(), &payload), Some(Ok(()))));
    assert!(backing.words((PAGE as usize) / 4).iter().all(|&w| w == 0x1111_1111));
    assert_eq!(RELEASED.load(Ordering::Acquire), ACQUIRED.load(Ordering::Acquire));
    drop(xmem);
}
