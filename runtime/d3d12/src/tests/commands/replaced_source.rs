use super::{Source, source_state};

const GUEST: u64 = 0x1_7798_6300;
const FIRST: u64 = 0x2_eadc_0000;
const LATER: u64 = 0x2_d40d_0000;

#[test]
fn an_unchanged_source_runs() {
    assert_eq!(source_state(GUEST, FIRST, FIRST, || false), Some(Source::Current));
}

#[test]
fn a_replaced_source_in_title_memory_keeps_its_recorded_storage() {
    assert_eq!(source_state(GUEST, FIRST, LATER, || true), Some(Source::Replaced));
    // Released altogether: the address resolves to itself again.
    assert_eq!(source_state(GUEST, FIRST, GUEST, || true), Some(Source::Replaced));
}

#[test]
fn a_source_replaced_in_a_registered_pool_is_refused() {
    assert_eq!(source_state(GUEST, FIRST, LATER, || false), None);
}

#[test]
fn a_raw_title_address_recorded_before_it_had_storage_is_refused() {
    assert_eq!(source_state(GUEST, GUEST, LATER, || true), None);
}
