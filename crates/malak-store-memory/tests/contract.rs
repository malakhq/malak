//! Shared store contract checks for the memory implementation.

use malak_layers::{Claim, Durability, Outcome, Store};
use malak_store_memory::MemoryStore;
use malak_testkit::store;

#[test]
fn declares_memory_durability() {
    assert_eq!(
        MemoryStore::default().describe().durability,
        Durability::Memory
    );
}

#[test]
fn memory_does_not_support_repair() {
    let error = MemoryStore::default().claim(Claim::Repair).unwrap_err();
    assert_eq!(error.outcome(), Outcome::NotSupported);
}

#[test]
fn claim_is_exclusive() {
    store::claim_is_exclusive(&MemoryStore::default());
}

#[test]
fn store_id_survives_release() {
    store::store_id_survives_release(&MemoryStore::default());
}

#[test]
fn stale_writer_cannot_append() {
    store::stale_writer_cannot_append(&MemoryStore::default());
}

#[test]
fn stale_writer_cannot_trim() {
    store::stale_writer_cannot_trim(&MemoryStore::default());
}

#[test]
fn append_round_trips_exact_records() {
    store::append_round_trips_exact_records(&MemoryStore::default());
}

#[test]
fn append_rejects_a_nonincreasing_position() {
    store::append_rejects_a_nonincreasing_position(&MemoryStore::default());
}

#[test]
fn read_respects_the_exclusive_end() {
    store::read_respects_the_exclusive_end(&MemoryStore::default());
}

#[test]
fn trim_keeps_only_the_requested_range() {
    store::trim_keeps_only_the_requested_range(&MemoryStore::default());
}
