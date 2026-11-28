//! `TUXBOX_TRACE_GPU_FAULT=1`: say which draws and dispatches the GPU had
//! started and not finished when the device was lost.
//!
//! Each one is bracketed by two immediate writes into a ring in system memory:
//! one as the command processor reaches it, one once the work before it is done.
//! A fault stops the second from landing, and the memory outlives the device.
//! The driver may not report the loss before a title gives up, so the report is
//! also written when no marker has changed for a while.
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

/// Commands kept. A frame of the title records well under this many.
const SLOTS: usize = 1 << 20;
/// Unfinished commands named in the report, oldest first.
const REPORTED: usize = 48;
/// Seconds without a marker changing before the unfinished commands are named.
const QUIET: u32 = 8;
/// Reports of a quiet GPU, in case a long dispatch looks like a hang at first.
const QUIET_REPORTS: u32 = 4;

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("TUXBOX_TRACE_GPU_FAULT").as_deref() == Ok("1"))
}

/// A pipeline's name for the report, kept only while tracing.
pub(crate) fn name(label: &str) -> Option<Arc<str>> {
    enabled().then(|| Arc::from(label))
}

#[derive(Clone, Default)]
struct Command {
    id: u32,
    list: usize,
    kind: &'static str,
    pipeline: Option<Arc<str>>,
}

struct Ring {
    _buffer: ID3D12Resource,
    address: u64,
    /// The persistent mapping: `SLOTS` pairs of started and finished ids.
    words: usize,
    commands: Mutex<Vec<Command>>,
    next: AtomicU32,
}

static RING: OnceLock<Option<Ring>> = OnceLock::new();

impl Ring {
    /// The two marker words of the slot `id` uses.
    fn slot(&self, id: u32) -> (u32, u32) {
        let words = self.words as *const u32;
        let slot = id as usize % SLOTS;
        // SAFETY: the slot lies in the persistent mapping.
        unsafe { (words.add(2 * slot).read_volatile(), words.add(2 * slot + 1).read_volatile()) }
    }

    /// A value that changes when any marker lands.
    fn fingerprint(&self) -> u64 {
        (0..SLOTS as u32).fold(0u64, |sum, id| {
            let (started, finished) = self.slot(id);
            sum.wrapping_mul(31).wrapping_add(u64::from(started) << 32 | u64::from(finished))
        })
    }
}

fn create(device: &ID3D12Device) -> windows::core::Result<Ring> {
    let heap = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: (SLOTS * 8) as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: D3D12 only borrows the descriptions during creation.
    unsafe {
        device.CreateCommittedResource(
            &heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut buffer,
        )?;
    }
    let buffer = buffer.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_POINTER))?;
    let mut mapped = core::ptr::null_mut();
    // SAFETY: the mapping lasts as long as the ring, which is never dropped.
    unsafe { buffer.Map(0, None, Some(&mut mapped))? };
    // SAFETY: the whole buffer is mapped and no list names it yet.
    unsafe { core::ptr::write_bytes(mapped.cast::<u8>(), 0, SLOTS * 8) };
    Ok(Ring {
        address: unsafe { buffer.GetGPUVirtualAddress() },
        _buffer: buffer,
        words: mapped.addr(),
        commands: Mutex::new(vec![Command::default(); SLOTS]),
        next: AtomicU32::new(1),
    })
}

fn watch(device: ID3D12Device) {
    struct Watched(ID3D12Device);
    // SAFETY: D3D12 devices are free-threaded.
    unsafe impl Send for Watched {}
    let watched = Watched(device);
    let spawned = std::thread::Builder::new().name("TuxBox GPU fault trace".into()).spawn(move || {
        let watched = watched;
        let (mut last, mut quiet, mut reports) = (0, 0, 0);
        loop {
            std::thread::sleep(Duration::from_secs(1));
            // SAFETY: the thread holds a reference on the device.
            if let Err(error) = unsafe { watched.0.GetDeviceRemovedReason() } {
                report(&format!("device lost ({:#010x})", error.code().0));
                return;
            }
            let Some(Some(ring)) = RING.get() else { continue };
            let now = ring.fingerprint();
            quiet = if now == last { quiet + 1 } else { 0 };
            last = now;
            if quiet == QUIET && reports < QUIET_REPORTS && report(&format!("no marker changed for {QUIET}s")) {
                reports += 1;
            }
        }
    });
    if spawned.is_err() {
        crate::diagnostics::log("GPU fault trace: no watching thread");
    }
}

fn ring(list: &ID3D12GraphicsCommandList) -> Option<&'static Ring> {
    RING.get_or_init(|| {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: the list is live while it records.
        unsafe { list.GetDevice(&mut device) }.ok()?;
        let device = device?;
        match create(&device) {
            Ok(ring) => {
                crate::diagnostics::log("GPU fault trace: marking every draw and dispatch");
                watch(device);
                Some(ring)
            }
            Err(error) => {
                crate::diagnostics::log(&format!("GPU fault trace: no marker buffer: {error:?}"));
                None
            }
        }
    })
    .as_ref()
}

fn write(list: &ID3D12GraphicsCommandList2, address: u64, value: u32, mode: D3D12_WRITEBUFFERIMMEDIATE_MODE) {
    let parameter = D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: address, Value: value };
    // SAFETY: one parameter and one mode; the address lies in the ring.
    unsafe { list.WriteBufferImmediate(1, &parameter, Some(&mode)) };
}

/// Record `command` between its two markers.
pub(crate) fn around(
    list: &ID3D12GraphicsCommandList,
    owner: usize,
    kind: &'static str,
    pipeline: Option<&Arc<str>>,
    command: impl FnOnce(),
) {
    let marked = enabled().then(|| ring(list).zip(list.cast::<ID3D12GraphicsCommandList2>().ok())).flatten();
    let Some((ring, markers)) = marked else { return command() };
    let id = ring.next.fetch_add(1, Ordering::Relaxed);
    let slot = id as usize % SLOTS;
    ring.commands.lock().unwrap_or_else(|e| e.into_inner())[slot] =
        Command { id, list: owner, kind, pipeline: pipeline.cloned() };
    let address = ring.address + (slot * 8) as u64;
    write(&markers, address, id, D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_IN);
    command();
    write(&markers, address + 4, id, D3D12_WRITEBUFFERIMMEDIATE_MODE_MARKER_OUT);
}

/// The ids whose slot shows a start and no finish, oldest first. `slot` reads
/// the two marker words of a slot.
pub(crate) fn unfinished(ids: impl Iterator<Item = u32>, slot: impl Fn(u32) -> (u32, u32)) -> Vec<u32> {
    let mut found: Vec<u32> = ids
        .filter(|&id| {
            let (started, finished) = slot(id);
            id != 0 && started == id && finished != id
        })
        .collect();
    found.sort_unstable();
    found
}

/// Name the unfinished commands. Returns whether there were any.
fn report(why: &str) -> bool {
    let Some(Some(ring)) = RING.get() else { return false };
    let commands = ring.commands.lock().unwrap_or_else(|e| e.into_inner());
    let found = unfinished(commands.iter().map(|command| command.id), |id| ring.slot(id));
    if found.is_empty() {
        return false;
    }
    crate::diagnostics::log(&format!(
        "GPU fault trace: {why}; {} commands started and not finished, of {} recorded",
        found.len(),
        ring.next.load(Ordering::Relaxed) - 1
    ));
    for id in found.iter().take(REPORTED) {
        let command = &commands[*id as usize % SLOTS];
        crate::diagnostics::log(&format!(
            "GPU fault trace: #{id} list {:#x} {} {}",
            command.list,
            command.kind,
            command.pipeline.as_deref().unwrap_or("unnamed pipeline")
        ));
    }
    true
}

#[cfg(test)]
#[path = "../tests/diagnostics/gpu_trace.rs"]
mod tests;
