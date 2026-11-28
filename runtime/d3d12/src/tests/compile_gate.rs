use super::*;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

/// The most holders seen at once while `jobs` threads pass through `gate`.
fn peak(gate: &Gate, jobs: usize) -> usize {
    let (live, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                let _slot = gate.acquire(|_| {});
                most.fetch_max(live.fetch_add(1, SeqCst) + 1, SeqCst);
                std::thread::sleep(Duration::from_millis(30));
                live.fetch_sub(1, SeqCst);
            });
        }
    });
    most.load(SeqCst)
}

#[test]
fn no_more_than_the_slot_count_run_together() {
    assert_eq!(peak(&Gate::new(2), 8), 2);
    assert_eq!(peak(&Gate::new(1), 4), 1);
}

#[test]
fn zero_slots_means_unlimited() {
    assert!(peak(&Gate::new(0), 6) > 2);
}

#[test]
fn only_a_caller_that_must_wait_is_told() {
    let gate = Gate::new(1);
    let told = AtomicUsize::new(0);
    let first = gate.acquire(|_| panic!("a free slot needs no wait"));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _second = gate.acquire(|slots| {
                told.store(slots, SeqCst);
            });
        });
        while told.load(SeqCst) == 0 {
            std::thread::yield_now();
        }
        drop(first);
    });
    assert_eq!(told.load(SeqCst), 1);
}

#[test]
fn a_panicking_compile_frees_its_slot() {
    let gate = Gate::new(1);
    let crashed = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _slot = gate.acquire(|_| {});
                panic!("driver compile failed");
            })
            .join()
    });
    assert!(crashed.is_err());
    let _again = gate.acquire(|_| panic!("the slot should be free"));
}

#[test]
fn small_shaders_never_take_a_slot() {
    assert!(large_compile(LARGE_BYTES - 1, |_| panic!("no wait")).is_none());
    assert!(large_compile(LARGE_BYTES, |_| {}).is_some());
}

#[test]
fn slot_count_comes_from_the_environment_value() {
    assert_eq!(parse_slots(None), None);
    assert_eq!(parse_slots(Some("2")), Some(2));
    assert_eq!(parse_slots(Some(" 0 ")), Some(0));
    assert_eq!(parse_slots(Some("many")), None);
    assert_eq!(parse_slots(Some("-1")), None);
}


#[test]
fn memory_budget_leaves_room_and_never_disables_progress() {
    for (available, slots) in [(0, 1), (2, 1), (4, 1), (6, 2), (8, 3), (10, 4), (32, 4)] {
        assert_eq!(memory_slots(Some(available * GIB)), slots);
    }
    assert_eq!(memory_slots(None), DEFAULT_SLOTS);
}

#[test]
fn memory_pressure_serializes_large_compiles() {
    let low = Gate::with_limit(Limit::Adaptive(|| Some(3 * GIB)));
    let medium = Gate::with_limit(Limit::Adaptive(|| Some(6 * GIB)));
    let high = Gate::with_limit(Limit::Adaptive(|| Some(16 * GIB)));
    assert_eq!(peak(&low, 6), 1);
    assert_eq!(peak(&medium, 6), 2);
    assert_eq!(peak(&high, 6), 4);
}

#[test]
fn adaptive_gate_rechecks_memory_after_a_slot_is_freed() {
    use std::sync::atomic::AtomicU64;
    static AVAILABLE: AtomicU64 = AtomicU64::new(16 * GIB);
    let gate = Gate::with_limit(Limit::Adaptive(|| Some(AVAILABLE.load(SeqCst))));
    let first = gate.acquire(|_| panic!("first must proceed"));
    let second = gate.acquire(|_| panic!("second must proceed"));
    AVAILABLE.store(3 * GIB, SeqCst);
    let waited = AtomicUsize::new(0);
    let (entered, received) = std::sync::mpsc::channel();
    let (early, eventually) = std::thread::scope(|scope| {
        scope.spawn(|| {
            let _slot = gate.acquire(|slots| { waited.store(slots, SeqCst); });
            entered.send(()).unwrap();
        });
        while waited.load(SeqCst) == 0 { std::thread::yield_now(); }
        drop(first);
        let early = received.recv_timeout(Duration::from_millis(100)).is_ok();
        drop(second);
        (early, early || received.recv_timeout(Duration::from_secs(2)).is_ok())
    });
    assert_eq!(waited.load(SeqCst), 1);
    assert!(!early && eventually);
}


#[cfg(windows)]
#[test]
fn host_memory_query_is_available() {
    let bytes = available_memory().expect("GlobalMemoryStatusEx failed");
    let slots = memory_slots(Some(bytes));
    println!("host memory available={} MiB, adaptive slots={slots}", bytes >> 20);
    assert!((1..=DEFAULT_SLOTS).contains(&slots));
}

#[test]
fn growing_budget_admits_all_newly_available_slots() {
    use std::sync::atomic::AtomicU64;
    static AVAILABLE: AtomicU64 = AtomicU64::new(6 * GIB);
    let gate = Gate::with_limit(Limit::Adaptive(|| Some(AVAILABLE.load(SeqCst))));
    let first = gate.acquire(|_| panic!("first must proceed"));
    let second = gate.acquire(|_| panic!("second must proceed"));
    let waiting = AtomicUsize::new(0);
    let (entered, received) = std::sync::mpsc::channel();
    let (release, released) = (Mutex::new(false), Condvar::new());
    let admitted = std::thread::scope(|scope| {
        for _ in 0..3 {
            let entered = entered.clone();
            let waiting = &waiting;
            let gate = &gate;
            let release = &release;
            let released = &released;
            scope.spawn(move || {
                let _slot = gate.acquire(|_| { waiting.fetch_add(1, SeqCst); });
                entered.send(()).unwrap();
                let _guard = released.wait_while(release.lock().unwrap(), |go| !*go).unwrap();
            });
        }
        while waiting.load(SeqCst) != 3 { std::thread::yield_now(); }
        AVAILABLE.store(16 * GIB, SeqCst);
        drop(first);
        let mut count = 0;
        for _ in 0..3 {
            if received.recv_timeout(Duration::from_secs(2)).is_ok() { count += 1; }
            else { break; }
        }
        *release.lock().unwrap() = true;
        released.notify_all();
        drop(second);
        count
    });
    assert_eq!(admitted, 3);
}
