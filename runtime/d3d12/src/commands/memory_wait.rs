//! Translate waits on host-visible 64-bit fence words across command list segments.
//! Supports all eight D3D12 comparison functions, mid-list segment splitting with
//! bound state replay, one persistent monotonic fence per queue, safe pre-satisfied
//! wait elision, and cross-queue completion wakeups.
use super::*;
use std::ops::Range;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize};
use std::sync::{Condvar, LazyLock, Once, mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Result;

/// Exact D3D12 comparison predicate for `Wait64BitValueX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Comparison {
    Never,
    Less,
    Equal,
    LessEqual,
    Greater,
    NotEqual,
    GreaterEqual,
    Always,
}

impl Comparison {
    pub(super) fn from_raw(raw: u32) -> Result<Self> {
        match D3D12_COMPARISON_FUNC(raw as i32) {
            D3D12_COMPARISON_FUNC_NEVER => Ok(Self::Never),
            D3D12_COMPARISON_FUNC_LESS => Ok(Self::Less),
            D3D12_COMPARISON_FUNC_EQUAL => Ok(Self::Equal),
            D3D12_COMPARISON_FUNC_LESS_EQUAL => Ok(Self::LessEqual),
            D3D12_COMPARISON_FUNC_GREATER => Ok(Self::Greater),
            D3D12_COMPARISON_FUNC_NOT_EQUAL => Ok(Self::NotEqual),
            D3D12_COMPARISON_FUNC_GREATER_EQUAL => Ok(Self::GreaterEqual),
            D3D12_COMPARISON_FUNC_ALWAYS => Ok(Self::Always),
            _ => Err(E_INVALIDARG.into()),
        }
    }

    pub(super) fn matches(self, current: u64, target: u64) -> bool {
        match self {
            Self::Never => false,
            Self::Less => current < target,
            Self::Equal => current == target,
            Self::LessEqual => current <= target,
            Self::Greater => current > target,
            Self::NotEqual => current != target,
            Self::GreaterEqual => current >= target,
            Self::Always => true,
        }
    }

    /// True when every possible `u64` value already satisfies the predicate.
    pub(super) fn is_always_satisfied(self, target: u64) -> bool {
        matches!(self, Self::Always)
            || (matches!(self, Self::GreaterEqual) && target == 0)
            || (matches!(self, Self::LessEqual) && target == u64::MAX)
    }

    /// Monotone upward predicates remain satisfied once reached by a counter.
    /// Non-monotone predicates (`EQUAL`, `LESS`, `LESS_EQUAL`, `NOT_EQUAL`,
    /// `NEVER`) are never elided at submission time.
    pub(super) fn is_monotone(self) -> bool {
        matches!(self, Self::Greater | Self::GreaterEqual)
    }

    pub(super) fn symbol(self) -> &'static str {
        match self {
            Self::Never => "NEVER",
            Self::Less => "<",
            Self::Equal => "==",
            Self::LessEqual => "<=",
            Self::Greater => ">",
            Self::NotEqual => "!=",
            Self::GreaterEqual => ">=",
            Self::Always => "ALWAYS",
        }
    }
}

/// Guest address ranges written by recorded commands, used to prevent unsafe
/// submission-time elision when an earlier list or segment modifies a waited word.
#[derive(Default)]
pub(super) struct WrittenRanges {
    ranges: Vec<Range<u64>>,
    all: bool,
}

impl WrittenRanges {
    const MAX_RANGES: usize = 256;

    pub(super) fn record(&mut self, address: u64, bytes: u64) {
        if self.all || bytes == 0 {
            return;
        }
        let end = address.saturating_add(bytes);
        let range = address..end;
        if let Some(existing) = self
            .ranges
            .iter_mut()
            .find(|r| r.start <= range.end && range.start <= r.end)
        {
            existing.start = existing.start.min(range.start);
            existing.end = existing.end.max(range.end);
            return;
        }
        if self.ranges.len() >= Self::MAX_RANGES {
            self.all = true;
            self.ranges.clear();
            return;
        }
        self.ranges.push(range);
    }

    pub(super) fn extend_from(&mut self, other: &Self) {
        if self.all {
            return;
        }
        if other.all {
            self.all = true;
            self.ranges.clear();
            return;
        }
        for r in &other.ranges {
            self.record(r.start, r.end - r.start);
        }
    }

    pub(super) fn overlaps_word(&self, address: u64) -> bool {
        if self.all {
            return true;
        }
        let end = address.saturating_add(8);
        self.ranges
            .iter()
            .any(|r| r.start < end && address < r.end)
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.all && self.ranges.is_empty()
    }

    pub(super) fn record_unknown(&mut self) {
        self.all = true;
        self.ranges.clear();
    }

    pub(super) fn clear(&mut self) {
        self.ranges.clear();
        self.all = false;
    }
}

struct InFlightWrite {
    device: usize,
    fence_key: usize,
    fence: ID3D12Fence,
    value: u64,
    writes: WrittenRanges,
}

// SAFETY: D3D12 fences are free-threaded.
unsafe impl Send for InFlightWrite {}

const MAX_IN_FLIGHT_WRITES: usize = 256;

static IN_FLIGHT_WRITES: Mutex<Vec<InFlightWrite>> = Mutex::new(Vec::new());
static IN_FLIGHT_COUNT: AtomicUsize = AtomicUsize::new(0);

fn prune_in_flight(entries: &mut Vec<InFlightWrite>) {
    entries.retain(|entry| {
        let done = unsafe { entry.fence.GetCompletedValue() };
        done < entry.value && done != u64::MAX
    });
    IN_FLIGHT_COUNT.store(entries.len(), Ordering::Release);
}

/// Track writes belonging to a submitted batch until its progress fence retires.
/// Prevents a later submission from eliding a monotone wait while an earlier
/// GPU write to the same address is still in flight.
pub(super) fn record_in_flight_writes(
    device: usize,
    fence: ID3D12Fence,
    value: u64,
    writes: WrittenRanges,
) {
    if writes.is_empty() {
        return;
    }
    let fence_key = fence.as_raw().addr();
    let mut entries = IN_FLIGHT_WRITES.lock().unwrap_or_else(|e| e.into_inner());
    prune_in_flight(&mut entries);
    if entries.len() >= MAX_IN_FLIGHT_WRITES
        && let Some(existing) = entries
            .iter_mut()
            .find(|e| e.device == device && e.fence_key == fence_key)
    {
        existing.value = existing.value.max(value);
        existing.writes.extend_from(&writes);
        IN_FLIGHT_COUNT.store(entries.len(), Ordering::Release);
        return;
    }
    entries.push(InFlightWrite {
        device,
        fence_key,
        fence,
        value,
        writes,
    });
    IN_FLIGHT_COUNT.store(entries.len(), Ordering::Release);
}

fn has_in_flight_write(device: usize, address: u64) -> bool {
    if IN_FLIGHT_COUNT.load(Ordering::Acquire) == 0 {
        return false;
    }
    let mut entries = IN_FLIGHT_WRITES.lock().unwrap_or_else(|e| e.into_inner());
    prune_in_flight(&mut entries);
    entries
        .iter()
        .any(|entry| entry.device == device && entry.writes.overlaps_word(address))
}

struct Word {
    resource: ID3D12Resource,
    mapped: usize,
}

impl Drop for Word {
    fn drop(&mut self) {
        unsafe { self.resource.Unmap(0, Some(&D3D12_RANGE::default())) };
    }
}

impl Word {
    fn map(list: &List, address: u64) -> Result<Self> {
        if address == 0 || !address.is_multiple_of(8) {
            return Err(E_INVALIDARG.into());
        }
        let (buffer, gpu) =
            unsafe { device::command_buffer_address(parent(&list.parent), address, 8)? };
        let resource = buffer.ok_or(E_NOTIMPL)?;
        let offset = gpu
            .checked_sub(unsafe { resource.GetGPUVirtualAddress() })
            .ok_or(E_INVALIDARG)? as usize;
        let range = D3D12_RANGE {
            Begin: offset,
            End: offset.checked_add(8).ok_or(E_INVALIDARG)?,
        };
        let mut base = ptr::null_mut();
        unsafe { resource.Map(0, Some(&range), Some(&mut base))? };
        if base.is_null() {
            unsafe { resource.Unmap(0, Some(&D3D12_RANGE::default())) };
            return Err(E_FAIL.into());
        }
        Ok(Self {
            resource,
            mapped: base.addr() + offset,
        })
    }

    fn value(&self) -> u64 {
        // X64 aligned 64-bit load. The COM resource and its CPU mapping stay alive
        // until all queued waits release their segment.
        unsafe { AtomicU64::from_ptr(self.mapped as *mut u64).load(Ordering::Acquire) }
    }

    fn matches(&self, comparison: Comparison, target: u64) -> bool {
        comparison.matches(self.value(), target)
    }
}

/// How long the queue is held for a guest memory word that nothing is going to
/// write, before it is released.
const STALL: Duration = Duration::from_secs(5);

/// How often a wait held past `STALL` looks again for the list that writes it.
const WRITER_CHECK: Duration = Duration::from_millis(100);

pub(super) struct Segment {
    prefix: Option<(ID3D12CommandList, ID3D12CommandAllocator)>,
    word: Word,
    address: u64,
    comparison: Comparison,
    target: u64,
    prior_write_in_list: bool,
}

impl Segment {
    pub(super) fn reached(&self) -> bool {
        self.word.matches(self.comparison, self.target)
    }

    /// Safe submission-time fast-path check: only elide when no prefix list runs
    /// before the wait, the predicate is monotone upward, no earlier list,
    /// segment, or still-in-flight GPU submission wrote to `[address..address+8)`,
    /// and the memory word already satisfies the predicate right now.
    pub(super) fn can_elide(&self, device: usize, batch_writes: &WrittenRanges) -> bool {
        if self.prefix.is_some() || !self.comparison.is_monotone() {
            return false;
        }
        let prior_write = self.prior_write_in_list
            || batch_writes.overlaps_word(self.address)
            || has_in_flight_write(device, self.address);
        can_elide_wait(false, self.comparison, prior_write, self.reached())
    }
}

fn can_elide_wait(
    has_prefix: bool,
    comparison: Comparison,
    prior_write_overlaps: bool,
    already_reached: bool,
) -> bool {
    !has_prefix && comparison.is_monotone() && !prior_write_overlaps && already_reached
}

/// One fence label operation a list recorded.
#[derive(Clone, Copy)]
pub(super) struct Label {
    /// `None` for a write. For a wait, `(holds_queue, comparison)`.
    wait: Option<(bool, Comparison)>,
    address: u64,
    value: u64,
}

impl Label {
    pub(super) fn write(address: u64, value: u64) -> Self {
        Self {
            wait: None,
            address,
            value,
        }
    }

    fn wait(holds: bool, comparison: Comparison, address: u64, value: u64) -> Self {
        Self {
            wait: Some((holds, comparison)),
            address,
            value,
        }
    }
}

/// What happened to a closed list.
#[derive(Clone, Copy)]
pub(super) enum Fate {
    /// Submitted to a queue of this command list type.
    Submitted(i32),
    /// Rejected at Close with this status, so it never runs.
    Dropped(i32),
}

struct Seen {
    order: u64,
    list: usize,
    fate: Fate,
    labels: Vec<Label>,
}

/// The most recent lists that wrote or waited on a label, oldest first.
static HISTORY: std::sync::Mutex<std::collections::VecDeque<Seen>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(super) fn note_list(list: usize, fate: Fate, labels: &[Label]) {
    if labels.is_empty() {
        return;
    }
    static ORDER: AtomicU64 = AtomicU64::new(0);
    let mut history = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    if history.len() >= 4096 {
        history.pop_front();
    }
    history.push_back(Seen {
        order: ORDER.fetch_add(1, Ordering::Relaxed),
        list,
        fate,
        labels: labels.to_vec(),
    });
}

fn fate(seen: &Seen) -> String {
    match seen.fate {
        Fate::Submitted(kind) => format!("submitted to a type {kind} queue"),
        Fate::Dropped(status) => format!("rejected at Close ({status:#010x})"),
    }
}

/// Say which lists should have satisfied a stalled wait, and which waits stand
/// before it: the one waiting and the one writing.
fn explain(address: u64, comparison: Comparison, target: u64) {
    let history = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    let mut found = 0;
    for seen in history.iter() {
        for (at, label) in seen.labels.iter().enumerate() {
            let matches = label.address == address
                && match label.wait {
                    None => comparison.matches(label.value, target),
                    Some((_, cmp)) => cmp == comparison && label.value == target,
                };
            if !matches || found >= 6 {
                continue;
            }
            found += 1;
            let earlier: Vec<_> = seen.labels[..at]
                .iter()
                .filter_map(|l| match l.wait {
                    Some((true, cmp)) => Some(format!("{:#x}{}{:#x}", l.address, cmp.symbol(), l.value)),
                    _ => None,
                })
                .collect();
            crate::diagnostics::log(&format!(
                "stalled label {address:#x} {} {target:#x}: {} by list {:#x} (order {}), {}, after waits [{}]",
                comparison.symbol(),
                if label.wait.is_some() { "waited" } else { "written" },
                seen.list,
                seen.order,
                fate(seen),
                earlier.join(" "),
            ));
        }
    }
    if !history.iter().any(|seen| {
        seen.labels
            .iter()
            .any(|l| l.wait.is_none() && l.address == address && comparison.matches(l.value, target))
    }) {
        crate::diagnostics::log(&format!(
            "stalled label {address:#x} {} {target:#x}: no submitted or rejected list writes it",
            comparison.symbol(),
        ));
    }
}

/// Guest addresses the title has waited on, so writes to them can be logged.
static WAITED: std::sync::Mutex<Vec<(u64, u64)>> = std::sync::Mutex::new(Vec::new());

/// Log the first waits on each address, then each power of two.
fn trace_wait(
    address: u64,
    comparison: Comparison,
    target: u64,
    value: u64,
    list_type: i32,
    mid_list: bool,
) {
    let mut waited = WAITED.lock().unwrap_or_else(|e| e.into_inner());
    let count = if let Some((_, count)) = waited.iter_mut().find(|(known, _)| *known == address) {
        *count += 1;
        *count
    } else if waited.len() < 256 {
        waited.push((address, 1));
        1
    } else {
        return;
    };
    drop(waited);
    if count <= 4 || count.is_power_of_two() {
        crate::diagnostics::log(&format!(
            "memory wait trace #{count} address={address:#x} op={} target={target:#x} value={value:#x} list_type={list_type} mid_list={mid_list}",
            comparison.symbol()
        ));
    }
}

/// Report a guest command that writes a waited-on address.
pub(super) fn trace_write(source: &str, address: u64, bytes: u64, value: u64) {
    let hit = WAITED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(waited, _)| {
            address < waited.saturating_add(8) && *waited < address.saturating_add(bytes)
        });
    if hit {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 32 || count.is_power_of_two() {
            crate::diagnostics::log(&format!(
                "memory wait write #{count} {source} address={address:#x} bytes={bytes:#x} value={value:#x}"
            ));
        }
    }
}

static FENCES_CREATED: AtomicU64 = AtomicU64::new(0);
static WAITS_SUBMITTED: AtomicU64 = AtomicU64::new(0);
static WAITS_ELIDED: AtomicU64 = AtomicU64::new(0);
static MID_LIST_SPLITS: AtomicU64 = AtomicU64::new(0);

pub(super) fn note_elided() {
    WAITS_ELIDED.fetch_add(1, Ordering::Relaxed);
}

pub(super) unsafe extern "system" fn wait_value64(
    list: &List,
    address: u64,
    comparison_raw: u32,
    target: u64,
    flags: u32,
) {
    let mut state = recording(list);
    if !state.ensure_open() {
        return;
    }
    let result = (|| -> Result<()> {
        if flags > 1 {
            return Err(E_NOTIMPL.into());
        }
        let comparison = Comparison::from_raw(comparison_raw)?;
        let word = Word::map(list, address)?;
        if comparison.is_always_satisfied(target) {
            return Ok(());
        }
        let list_type = state.list_type.0;
        let mid_list = state.active.has_commands;
        trace_wait(address, comparison, target, word.value(), list_type, mid_list);
        if mid_list {
            if state.can_split_mid_list() {
                state.segments.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
                let prior_write_in_list = state.written_ranges.overlaps_word(address);
                let closed = state.split_active()?;
                MID_LIST_SPLITS.fetch_add(1, Ordering::Relaxed);
                state
                    .labels
                    .push(Label::wait(true, comparison, address, target));
                state.segments.push(Arc::new(Segment {
                    prefix: Some(closed),
                    word,
                    address,
                    comparison,
                    target,
                    prior_write_in_list,
                }));
                return Ok(());
            }
            state
                .labels
                .push(Label::wait(false, comparison, address, target));
            if state.list_type != D3D12_COMMAND_LIST_TYPE_COPY {
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                            pResource: std::mem::ManuallyDrop::new(None),
                        }),
                    },
                };
                // SAFETY: the active list is open, and a null resource names every UAV.
                unsafe { state.native().ResourceBarrier(&[barrier]) };
            }
            let reason = state.split_fallback_reason();
            static FIRST: Once = Once::new();
            crate::diagnostics::log_once(&FIRST, || {
                format!(
                    "GPU memory wait {address:#x} {} {target:#x} replaced by a barrier ({reason})",
                    comparison.symbol()
                )
            });
            return Ok(());
        }
        state.segments.try_reserve(1).map_err(|_| E_OUTOFMEMORY)?;
        let prior_write_in_list = state.written_ranges.overlaps_word(address);
        state
            .labels
            .push(Label::wait(true, comparison, address, target));
        state.segments.push(Arc::new(Segment {
            prefix: None,
            word,
            address,
            comparison,
            target,
            prior_write_in_list,
        }));
        Ok(())
    })();
    if let Err(error) = result {
        let op = Comparison::from_raw(comparison_raw)
            .map(Comparison::symbol)
            .unwrap_or("INVALID");
        crate::diagnostics::recording_error(format_args!(
            "64-bit GPU memory wait {address:#x} {op} {target:#x} ({:?})",
            error.code()
        ));
        state.error = error.code();
    }
}

/// Shared wakeup notifier for cross-queue memory waits.
struct Notifier {
    epoch: Mutex<u64>,
    ready: Condvar,
}

impl Notifier {
    const fn new() -> Self {
        Self {
            epoch: Mutex::new(0),
            ready: Condvar::new(),
        }
    }

    fn epoch(&self) -> u64 {
        *self.epoch.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify_all(&self) {
        let mut epoch = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        *epoch = epoch.wrapping_add(1);
        self.ready.notify_all();
    }

    fn wait_timeout(&self, seen: u64, timeout: Duration) {
        let guard = self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        if *guard != seen {
            return;
        }
        let _ = self.ready.wait_timeout(guard, timeout);
    }
}

static NOTIFIER: Notifier = Notifier::new();
static ACTIVE_WAITERS: AtomicUsize = AtomicUsize::new(0);

// D3D12 can retain SetEventOnCompletion registrations across queue lifetimes.
// Keep one process-lifetime event handle valid forever and bridge it into NOTIFIER.
static WAKE_EVENT: LazyLock<Result<OwnedHandle>> = LazyLock::new(|| {
    let handle = unsafe { CreateEventW(None, false, false, None)? };
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
});

fn wake_event() -> Option<HANDLE> {
    static BRIDGE: Once = Once::new();
    let handle = WAKE_EVENT.as_ref().ok().map(|e| HANDLE(e.as_raw_handle()))?;
    BRIDGE.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("TuxBox wait wakeup".into())
            .spawn(move || {
                let Some(event) = WAKE_EVENT.as_ref().ok().map(|e| HANDLE(e.as_raw_handle())) else {
                    return;
                };
                loop {
                    if unsafe { WaitForSingleObject(event, INFINITE) } == WAIT_FAILED {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    notify_waiters();
                }
            });
    });
    Some(handle)
}

/// Wake any memory-wait worker sleeping for cross-queue or fence progress.
/// Zero-lock fast path when no queue is actively waiting.
pub(crate) fn notify_waiters() {
    if ACTIVE_WAITERS.load(Ordering::SeqCst) != 0 {
        NOTIFIER.notify_all();
    }
}

/// Waits the queue workers are polling right now: queue type, label, comparison, target.
static ACTIVE: std::sync::Mutex<Vec<(i32, u64, Comparison, u64)>> =
    std::sync::Mutex::new(Vec::new());

fn active() -> std::sync::MutexGuard<'static, Vec<(i32, u64, Comparison, u64)>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

struct ActiveWaitGuard {
    held: (i32, u64, Comparison, u64),
}

impl ActiveWaitGuard {
    fn enter(held: (i32, u64, Comparison, u64)) -> Self {
        ACTIVE_WAITERS.fetch_add(1, Ordering::SeqCst);
        active().push(held);
        Self { held }
    }
}

impl Drop for ActiveWaitGuard {
    fn drop(&mut self) {
        let mut waiting = active();
        if let Some(at) = waiting.iter().position(|&w| w == self.held) {
            waiting.swap_remove(at);
        }
        drop(waiting);
        ACTIVE_WAITERS.fetch_sub(1, Ordering::SeqCst);
    }
}

mod fence_poll;
mod recorded_writes;
pub(super) use recorded_writes::{LabelWrites, note as note_recorded_write};

/// `queue` was given the lists of `writes` in a batch that has run once
/// `fence` reaches `value`. Until then they hold the other queues' waits.
pub(super) fn label_writes_submitted(
    writes: Vec<Arc<LabelWrites>>,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    value: u64,
) {
    struct Progress(ID3D12Fence, u64);
    // SAFETY: D3D12 fences are free-threaded.
    unsafe impl Send for Progress {}
    unsafe impl Sync for Progress {}
    impl Progress {
        fn done(&self) -> bool {
            // SAFETY: the fence is live while this holds it.
            let completed = unsafe { self.0.GetCompletedValue() };
            // A removed device completes nothing more.
            completed >= self.1 || completed == u64::MAX
        }
    }
    let progress = Progress(fence.clone(), value);
    recorded_writes::submitted(writes, queue.as_raw().addr(), move || progress.done());
}

// Poll removal while waiting for a notification; a healthy slow prefix
// retains its original ordering and has no new deadline.
const PREFIX_POLL_MS: u32 = 100;

struct WorkerEvent(Option<OwnedHandle>);

impl WorkerEvent {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self(Some(unsafe { OwnedHandle::from_raw_handle(handle.0) })))
    }

    fn handle(&self) -> HANDLE {
        self.0
            .as_ref()
            .map_or(HANDLE::default(), |h| HANDLE(h.as_raw_handle()))
    }

    fn wait_for_fence(
        &mut self,
        fence: &ID3D12Fence,
        value: u64,
        device: &ID3D12Device,
    ) -> Result<()> {
        let completed = unsafe { fence.GetCompletedValue() };
        if completed == u64::MAX {
            return Err(unsafe { device.GetDeviceRemovedReason() }
                .err()
                .unwrap_or_else(|| E_FAIL.into()));
        }
        if completed >= value {
            return unsafe { device.GetDeviceRemovedReason() };
        }
        let event = self.handle();
        if event.is_invalid() {
            return Err(E_FAIL.into());
        }
        unsafe { fence.SetEventOnCompletion(value, event)? };
        let result = fence_poll::poll(
            value,
            || unsafe { fence.GetCompletedValue() },
            || unsafe { device.GetDeviceRemovedReason() },
            || match unsafe { WaitForSingleObject(event, PREFIX_POLL_MS) } {
                WAIT_OBJECT_0 | WAIT_TIMEOUT => Ok(()),
                _ => Err(E_FAIL.into()),
            },
            || E_FAIL.into(),
        );
        if result.is_err() {
            // A driver may still retain the registered event when the wait
            // fails. Preserve its handle until process exit, just as on the
            // old WAIT_FAILED path. The worker stops after this error.
            // MAX is removal, not evidence that the registered value retired.
            let observed = unsafe { fence.GetCompletedValue() };
            if observed == u64::MAX || observed < value {
                std::mem::forget(self.0.take());
            }
        }
        result
    }
}

struct Job {
    kind: i32,
    fence: ID3D12Fence,
    signal_val: u64,
    release_val: u64,
    segment: Arc<Segment>,
    device: ID3D12Device,
    /// The queue the wait holds.
    queue: usize,
    /// Unreleased label waits of the batch this one belongs to.
    waits: Option<crate::device::placement::held::Waits>,
}

impl Job {
    /// Whether a write that satisfies the wait is still to run: recorded in a
    /// list no queue has taken yet, or in a batch another queue has not run.
    fn writer_is_coming(&self) -> bool {
        let segment = &self.segment;
        recorded_writes::coming(segment.address, segment.comparison, segment.target, self.queue)
    }

    fn release_wait(&mut self) {
        if let Some(waits) = self.waits.take() {
            waits.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.release_wait();
    }
}

/// RAII guard for batch waits not yet handed off to a queued worker `Job`.
pub(super) struct PendingWaits {
    waits: Option<crate::device::placement::held::Waits>,
    remaining: usize,
}

impl PendingWaits {
    pub(super) fn new(
        waits: Option<crate::device::placement::held::Waits>,
        remaining: usize,
    ) -> Self {
        Self { waits, remaining }
    }

    fn take_one(&mut self) -> Option<crate::device::placement::held::Waits> {
        debug_assert!(self.remaining > 0);
        self.remaining = self.remaining.saturating_sub(1);
        self.waits.clone()
    }
}

impl Drop for PendingWaits {
    fn drop(&mut self) {
        if self.remaining != 0
            && let Some(waits) = &self.waits
        {
            waits.fetch_sub(self.remaining, Ordering::AcqRel);
        }
    }
}

pub(super) struct Worker {
    sender: mpsc::Sender<Job>,
    fence: ID3D12Fence,
    next_pair: AtomicU64,
}

fn next_monotonic_pair(counter: &AtomicU64) -> Result<(u64, u64)> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current >= u64::MAX - 2 {
            return Err(E_FAIL.into());
        }
        match counter.compare_exchange_weak(
            current,
            current + 2,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok((current + 1, current + 2)),
            Err(observed) => current = observed,
        }
    }
}

impl Worker {
    fn new(device: &ID3D12Device, error: Arc<AtomicI32>) -> Result<Self> {
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE)? };
        FENCES_CREATED.fetch_add(1, Ordering::Relaxed);
        let mut fence_event = WorkerEvent::new()?;
        let (sender, jobs) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("TuxBox GPU memory waits".into())
            .spawn(move || {
                for mut job in jobs {
                    let result = (|| -> Result<()> {
                        fence_event.wait_for_fence(&job.fence, job.signal_val, &job.device)?;
                        if !job.segment.reached() {
                            let started = Instant::now();
                            let held = (
                                job.kind,
                                job.segment.address,
                                job.segment.comparison,
                                job.segment.target,
                            );
                            let _guard = ActiveWaitGuard::enter(held);
                            // When to look for the list that writes the value: once
                            // the wait is `STALL` old, then every `WRITER_CHECK`
                            // for as long as there is one.
                            let mut look_at = STALL;
                            let mut held_reported = false;
                            while !job.segment.reached() {
                                unsafe {
                                    job.device.GetDeviceRemovedReason()?;
                                }
                                let elapsed = started.elapsed();
                                if elapsed >= look_at {
                                    if !job.writer_is_coming() {
                                        // Nothing wrote the value, for example because
                                        // the work that should have was dropped. A queue
                                        // held forever trips the title's GPU watchdog.
                                        report_stall(
                                            job.segment.address,
                                            job.segment.comparison,
                                            job.segment.target,
                                            job.segment.word.value(),
                                        );
                                        break;
                                    }
                                    // The list that writes the value is recorded, or
                                    // another queue is running it: the title is only
                                    // slower than `STALL`, and what waits here needs
                                    // that work.
                                    if !held_reported {
                                        held_reported = true;
                                        report_held(&job.segment);
                                    }
                                    look_at = elapsed + WRITER_CHECK;
                                }
                                wait_for_progress(
                                    &job.segment,
                                    elapsed,
                                    look_at.saturating_sub(elapsed),
                                );
                            }
                        }
                        unsafe {
                            job.fence.Signal(job.release_val)?;
                        }
                        notify_waiters();
                        job.release_wait();
                        static FIRST: Once = Once::new();
                        crate::diagnostics::log_once(&FIRST, || {
                            format!(
                                "GPU memory wait completed {} {:#x}",
                                job.segment.comparison.symbol(),
                                job.segment.target
                            )
                        });
                        Ok(())
                    })();
                    if let Err(cause) = result {
                        // Drop of `job` and any remaining queued `Job`s in `jobs`
                        // releases every outstanding `placement::held::Waits` count.
                        let _ = error.compare_exchange(
                            S_OK.0,
                            cause.code().0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        // Fail the device rather than releasing work behind a broken gate.
                        if let Ok(device) = job.device.cast::<ID3D12Device5>() {
                            unsafe { device.RemoveDevice() };
                        }
                        break;
                    }
                }
            })
            .map_err(|_| E_OUTOFMEMORY)?;
        Ok(Self {
            sender,
            fence,
            next_pair: AtomicU64::new(0),
        })
    }

    fn next_gate(&self) -> Result<(u64, u64)> {
        next_monotonic_pair(&self.next_pair)
    }
}

/// Brief active poll window for sub-millisecond cross-queue handoffs before
/// parking on the completion notifier.
const SPIN: Duration = Duration::from_micros(750);

/// Maximum bounded sleep per iteration when no D3D12 completion event fires
/// (for example, a direct CPU write or an unsatisfied wait approaching timeout).
const WAIT_STEP: Duration = Duration::from_millis(1);

fn wait_for_progress(segment: &Segment, elapsed: Duration, remaining: Duration) {
    if elapsed < SPIN {
        for _ in 0..16 {
            std::hint::spin_loop();
            if segment.reached() {
                return;
            }
        }
        std::thread::yield_now();
        return;
    }
    let epoch = NOTIFIER.epoch();
    if let Some(event) = wake_event() {
        super::queue_trace::arm_progress_wake(event);
    }
    if segment.reached() {
        return;
    }
    NOTIFIER.wait_timeout(epoch, remaining.min(WAIT_STEP));
}

/// Report a wait held past `STALL` for a write that is still to run: the
/// first eight, then each power of two.
fn report_held(segment: &Segment) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 8 || count.is_power_of_two() {
        crate::diagnostics::log(&format!(
            "GPU memory wait held past {}s for a write that is still to run (#{count}): address={:#x} op={} target={:#x} value={:#x}",
            STALL.as_secs(),
            segment.address,
            segment.comparison.symbol(),
            segment.target,
            segment.word.value(),
        ));
    }
}

/// Report a released wait: the first eight, then each power of two.
fn report_stall(address: u64, comparison: Comparison, target: u64, value: u64) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let count = COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 8 || count.is_power_of_two() {
        crate::diagnostics::log(&format!(
            "GPU memory wait released after {}s without reaching its target (#{count}): address={address:#x} op={} target={target:#x} value={value:#x}",
            STALL.as_secs(),
            comparison.symbol(),
        ));
    }
    if count == 1 {
        // The first stall shows the whole knot: every held queue and, for
        // each, the lists that should release it.
        super::queue_trace::report();
        let held = active().clone();
        for (kind, address, cmp, target) in held {
            crate::diagnostics::log(&format!(
                "held queue of type {kind}: waiting for {address:#x} {} {target:#x}",
                cmp.symbol()
            ));
            explain(address, cmp, target);
        }
    } else if count <= 8 {
        explain(address, comparison, target);
    }
}

pub(super) type Splits = Vec<(usize, Arc<Segment>)>;

pub(super) unsafe fn submit(
    queue: &Queue,
    host: &[*mut c_void],
    splits: Splits,
    mut pending: PendingWaits,
) -> Result<()> {
    let run = |lists: &[*mut c_void]| {
        if !lists.is_empty() {
            unsafe {
                (queue.native.vtable().ExecuteCommandLists)(
                    queue.native.as_raw(),
                    lists.len() as u32,
                    lists.as_ptr(),
                )
            };
        }
    };
    if splits.is_empty() {
        run(host);
        return Ok(());
    }
    let mut device: Option<ID3D12Device> = None;
    unsafe {
        queue.native.GetDevice(&mut device)?;
    }
    let device = device.ok_or(E_FAIL)?;
    let worker = match queue.state.wait_worker.get() {
        Some(worker) => worker,
        None => {
            let new = Worker::new(&device, queue.state.placement.error.clone())?;
            queue.state.wait_worker.get_or_init(|| new)
        }
    };
    let mut start = 0;
    for (index, segment) in splits {
        run(&host[start..index]);
        start = index;
        if let Some((prefix, _allocator)) = &segment.prefix {
            run(&[prefix.as_raw()]);
        }
        let (signal_val, release_val) = worker.next_gate()?;
        WAITS_SUBMITTED.fetch_add(1, Ordering::Relaxed);
        // `signal_val` retires the prefix before the CPU reads the guest word.
        // `release_val` holds the suffix on the GPU without blocking the submission thread.
        unsafe {
            queue.native.Signal(&worker.fence, signal_val)?;
        }
        let job = Job {
            kind: queue.state.kind.0,
            fence: worker.fence.clone(),
            signal_val,
            release_val,
            segment,
            device: device.clone(),
            queue: queue.native.as_raw().addr(),
            waits: pending.take_one(),
        };
        worker.sender.send(job).map_err(|_| E_FAIL)?;
        unsafe {
            queue.native.Wait(&worker.fence, release_val)?;
            super::queue_trace::waited(
                &queue.native,
                "hold for a title fence label",
                &worker.fence,
                release_val,
            );
        }
    }
    run(&host[start..]);
    Ok(())
}

#[cfg(test)]
#[path = "../tests/commands/memory_wait.rs"]
mod tests;
