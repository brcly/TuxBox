//! Label writes that are still to run: recorded in a list no queue has taken
//! yet, or given to a queue in a batch that has not run.
//!
//! A title can submit the list that waits on a label well before the list
//! that writes it: ProjectScarlett submits its compute lists first, and the
//! lists that write their labels with the rest of the frame. When a frame
//! takes longer than a queue is held for a label, releasing those waits runs
//! the compute work before the work it waits for. A write that is still to
//! run is coming, so a wait holds for it.
use super::Comparison;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// The label writes of one recording, alive while they can still run: until
/// the list is reset, rejected or destroyed, or the batch it went in has run.
#[derive(Default)]
pub(in crate::commands) struct LabelWrites {
    /// The batch the list was submitted in, once it was.
    batch: OnceLock<Arc<Batch>>,
}

struct Batch {
    /// The queue that runs it.
    queue: usize,
    /// Whether it has run.
    done: Box<dyn Fn() -> bool + Send + Sync>,
}

struct Write {
    list: Weak<LabelWrites>,
    address: u64,
    value: u64,
}

struct Writes {
    writes: Vec<Write>,
    /// The length at which the writes of lists that are gone are dropped next.
    prune_at: usize,
    /// The lists of batches that have not run. A title can reset a list as
    /// soon as it has submitted it, so the recording no longer keeps these.
    running: Vec<Arc<LabelWrites>>,
}

impl Writes {
    /// Let go of the lists whose batch has run.
    fn retire(&mut self) {
        self.running
            .retain(|list| list.batch.get().is_some_and(|batch| !(batch.done)()));
    }
}

const PRUNE: usize = 64;

static STILL_TO_RUN: Mutex<Writes> = Mutex::new(Writes {
    writes: Vec::new(),
    prune_at: PRUNE,
    running: Vec::new(),
});

/// Note a label write `list` has recorded.
pub(in crate::commands) fn note(list: &Arc<LabelWrites>, address: u64, value: u64) {
    let mut still = STILL_TO_RUN.lock().unwrap_or_else(|e| e.into_inner());
    if still.writes.len() >= still.prune_at {
        still.writes.retain(|write| write.list.strong_count() != 0);
        // Twice what is left, so that recording a write stays cheap.
        still.prune_at = (2 * still.writes.len()).max(PRUNE);
    }
    still.writes.push(Write {
        list: Arc::downgrade(list),
        address,
        value,
    });
}

/// `queue` was given `lists` in one batch. `done` tells when that batch has run.
pub(in crate::commands) fn submitted(
    lists: Vec<Arc<LabelWrites>>,
    queue: usize,
    done: impl Fn() -> bool + Send + Sync + 'static,
) {
    if lists.is_empty() {
        return;
    }
    let batch = Arc::new(Batch {
        queue,
        done: Box::new(done),
    });
    let mut still = STILL_TO_RUN.lock().unwrap_or_else(|e| e.into_inner());
    still.retire();
    for list in lists {
        // Set once: a recording hands its writes over when it is submitted.
        let _ = list.batch.set(batch.clone());
        still.running.push(list);
    }
}

/// Whether a write that satisfies a wait held on `queue` is still to run.
pub(super) fn coming(address: u64, comparison: Comparison, target: u64, queue: usize) -> bool {
    let mut still = STILL_TO_RUN.lock().unwrap_or_else(|e| e.into_inner());
    still.retire();
    still.writes.iter().any(|write| {
        write.address == address
            && comparison.matches(write.value, target)
            && write.list.upgrade().is_some_and(|list| match list.batch.get() {
                // Recorded, and no queue has it yet.
                None => true,
                // A queue runs what it is given in order. What the waiting
                // queue got before the wait has run, and what it got after
                // cannot run until the wait ends: neither is worth holding for.
                Some(batch) => batch.queue != queue && !(batch.done)(),
            })
    })
}

#[cfg(test)]
#[path = "../../tests/commands/recorded_writes.rs"]
mod tests;
