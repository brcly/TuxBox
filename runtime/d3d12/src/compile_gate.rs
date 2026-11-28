//! Bound how many large translated compute shaders the driver compiles at once.
//!
//! The title creates pipelines on many threads. Translated compute programs of
//! several hundred KiB take the driver minutes and on the order of a gigabyte
//! each. With sixteen in flight ProjectScarlett filled its memory limit and
//! swap, and none of the larger ones finished in nine minutes. Smaller shaders
//! are not limited.

use std::sync::{Condvar, LazyLock, Mutex};

/// Translated DXIL of at least this size takes a slot. ProjectScarlett's
/// compiles that ran for more than 30 seconds were all 700 KiB or larger, and
/// 95% of its compute translations are below 450 KiB.
const LARGE_BYTES: usize = 512 << 10;
const DEFAULT_SLOTS: usize = 4;

/// Leave room for the running title, and conservatively budget 2 GiB for
/// each large driver compile (measured allocations have been around 1 GiB).
const GIB: u64 = 1 << 30;
const RESERVED_BYTES: u64 = 2 * GIB;
const BYTES_PER_COMPILE: u64 = 2 * GIB;

/// `TUXBOX_LARGE_COMPILES` overrides the memory-based policy. `0` is unlimited.
static LARGE: LazyLock<Gate> = LazyLock::new(|| {
    let value = std::env::var("TUXBOX_LARGE_COMPILES").ok();
    Gate::with_limit(parse_slots(value.as_deref())
        .map_or(Limit::Adaptive(available_memory), Limit::Fixed))
});

fn parse_slots(value: Option<&str>) -> Option<usize> {
    value.and_then(|text| text.trim().parse().ok())
}

#[cfg(windows)]
fn available_memory() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: the initialized structure is writable for the duration of the call.
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some(status.ullAvailPhys.min(status.ullAvailPageFile))
}

#[cfg(not(windows))]
fn available_memory() -> Option<u64> { None }

fn memory_slots(available: Option<u64>) -> usize {
    available.map_or(DEFAULT_SLOTS, |bytes| {
        // Always allow one compile to make progress, even under pressure.
        (bytes.saturating_sub(RESERVED_BYTES) / BYTES_PER_COMPILE)
            .clamp(1, DEFAULT_SLOTS as u64) as usize
    })
}

enum Limit {
    Fixed(usize),
    Adaptive(fn() -> Option<u64>),
}

impl Limit {
    fn slots(&self) -> usize {
        match self {
            Self::Fixed(slots) => *slots,
            Self::Adaptive(available) => memory_slots(available()),
        }
    }
}

pub(crate) struct Gate {
    limit: Limit,
    running: Mutex<usize>,
    freed: Condvar,
}

/// One running large compile. Dropping it frees the slot.
pub(crate) struct Slot<'a>(&'a Gate);

impl Gate {
    #[cfg(test)]
    fn new(slots: usize) -> Self {
        Self::with_limit(Limit::Fixed(slots))
    }

    fn with_limit(limit: Limit) -> Self {
        Self { limit, running: Mutex::new(0), freed: Condvar::new() }
    }

    /// Block until a slot is free. `waiting` is called once, with the slot
    /// count, if the caller has to wait.
    fn acquire(&self, waiting: impl FnOnce(usize)) -> Slot<'_> {
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let mut slots = self.limit.slots();
        if slots != 0 && *running >= slots {
            waiting(slots);
            while slots != 0 && *running >= slots {
                running = self.freed.wait(running).unwrap_or_else(|e| e.into_inner());
                // Re-evaluate only after a running compile completes. The
                // minimum of one ensures that memory pressure cannot deadlock.
                slots = self.limit.slots();
            }
        }
        *running += 1;
        Slot(self)
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        // An adaptive budget can grow by more than one slot when memory
        // becomes available. Wake every waiter so the new capacity can be
        // used; the mutex and admission predicate still enforce the limit.
        match &self.0.limit {
            Limit::Adaptive(_) => self.0.freed.notify_all(),
            Limit::Fixed(_) => self.0.freed.notify_one(),
        }
    }
}

pub(crate) fn is_large(bytes: usize) -> bool {
    bytes >= LARGE_BYTES
}

/// Hold the returned value across the pipeline creation call. Small shaders get
/// `None` and never wait.
pub(crate) fn large_compile(bytes: usize, waiting: impl FnOnce(usize)) -> Option<Slot<'static>> {
    is_large(bytes).then(|| LARGE.acquire(waiting))
}

#[cfg(test)]
#[path = "tests/compile_gate.rs"]
mod tests;
