use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

// Each test owns its addresses and queues: the record is the process's.
const WAITING: usize = 0x51;
const OTHER: usize = 0x52;

fn waits(label: u64, target: u64) -> bool {
    coming(label, Comparison::GreaterEqual, target, WAITING)
}

#[test]
fn a_recorded_write_keeps_a_wait_until_its_list_is_gone() {
    const LABEL: u64 = 0x7001_0000;
    assert!(!waits(LABEL, 5), "nothing is recorded yet");
    let list = Arc::<LabelWrites>::default();
    note(&list, LABEL, 5);
    assert!(waits(LABEL, 5), "the list that writes 5 is still to be submitted");
    assert!(waits(LABEL, 4));
    assert!(!waits(LABEL, 6), "5 does not satisfy a wait for 6");
    assert!(!waits(LABEL + 8, 5), "another label");
    assert!(!coming(LABEL, Comparison::Less, 5, WAITING), "the comparison is the wait's");
    // The list is reset, rejected or destroyed.
    drop(list);
    assert!(!waits(LABEL, 5), "a write nobody will submit holds no queue");
}

#[test]
fn a_submitted_write_keeps_a_wait_until_its_batch_has_run() {
    const LABEL: u64 = 0x7002_0000;
    let list = Arc::<LabelWrites>::default();
    note(&list, LABEL, 5);
    let ran = Arc::new(AtomicBool::new(false));
    let done = ran.clone();
    // The title resets the list as soon as it has submitted it.
    submitted(vec![list], OTHER, move || done.load(Ordering::Relaxed));
    assert!(waits(LABEL, 5), "another queue has not run the batch that writes 5");
    ran.store(true, Ordering::Relaxed);
    assert!(!waits(LABEL, 5), "the batch has run: what it wrote is what the label holds");
}

#[test]
fn a_wait_does_not_hold_for_what_its_own_queue_was_given() {
    const LABEL: u64 = 0x7003_0000;
    let list = Arc::<LabelWrites>::default();
    note(&list, LABEL, 5);
    // A batch that cannot run before the wait ends: it is behind it.
    submitted(vec![list], WAITING, || false);
    assert!(!waits(LABEL, 5), "the waiting queue holds for a write behind its own wait");
    assert!(coming(LABEL, Comparison::GreaterEqual, 5, OTHER), "another queue holds for it");
}

#[test]
fn writes_of_other_lists_stay() {
    const LABEL: u64 = 0x7004_0000;
    let (first, second) = (Arc::<LabelWrites>::default(), Arc::<LabelWrites>::default());
    note(&first, LABEL, 7);
    note(&second, LABEL, 9);
    drop(first);
    assert!(waits(LABEL, 9));
    assert!(coming(LABEL, Comparison::Equal, 9, WAITING));
    assert!(!coming(LABEL, Comparison::Equal, 7, WAITING), "the list that wrote 7 is gone");
}

#[test]
fn what_has_run_or_is_gone_is_dropped() {
    const LABEL: u64 = 0x7005_0000;
    let kept = Arc::<LabelWrites>::default();
    note(&kept, LABEL, 1);
    // A title records, submits and resets lists for as long as it runs.
    for frame in 0..10_000 {
        let list = Arc::<LabelWrites>::default();
        note(&list, LABEL + 8, frame);
        if frame % 2 == 0 {
            submitted(vec![list], OTHER, || true);
        }
    }
    let (writes, running) = {
        let still = STILL_TO_RUN.lock().unwrap();
        (still.writes.len(), still.running.len())
    };
    assert!(writes < 1_000, "{writes} writes are kept after 10,000 lists came and went");
    assert!(running < 100, "{running} lists are kept after their batches ran");
    assert!(coming(LABEL, Comparison::Equal, 1, WAITING), "a list still to be submitted lost its write");
    assert!(!waits(LABEL + 8, 0));
}
