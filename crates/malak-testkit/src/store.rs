//! Checks for store claims, exact record round trips, append order, and trimming.
//!
//! Run each check from its own test against a fresh, empty store.

use malak_layers::{
    Claim, Claimed, Ownership, Position, ReadLimit, ReadResult, Record, Store, Trim,
};

const CHANNEL: &str = "work/deploy/_0123456789abcdef0123456789abcdef";

fn take(store: &dyn Store) -> Ownership {
    let Claimed::Taken(owner) = store.claim(Claim::Take).unwrap() else {
        panic!("ordinary claim did not return ownership");
    };
    owner
}

fn record(position: u64, data: &'static [u8]) -> Record {
    Record {
        position: position.try_into().unwrap(),
        data: data.into(),
    }
}

fn read(store: &dyn Store, from: u64, before: Option<Position>, records: usize) -> Vec<Record> {
    let limit = ReadLimit { records, before };
    let result = store
        .read(CHANNEL, from.try_into().unwrap(), limit)
        .unwrap();
    let ReadResult::Records {
        records,
        unparseable: false,
    } = result
    else {
        panic!("store did not return a whole ordinary read");
    };
    records
}

fn populate_three(store: &dyn Store) -> (u64, [Record; 3]) {
    let owner = take(store);
    let records = [
        record(1, b"first"),
        record(2, b"second"),
        record(3, b"third"),
    ];
    store.append(owner.generation, CHANNEL, &records).unwrap();
    (owner.generation, records)
}

fn take_successor(store: &dyn Store) -> (Ownership, Ownership) {
    let owner = take(store);
    store
        .claim(Claim::Release {
            generation: owner.generation,
        })
        .unwrap();
    (owner, take(store))
}

/// Assert that a second claimant cannot take a store with an active writer.
pub fn claim_is_exclusive(store: &dyn Store) {
    let owner = take(store);
    assert!(store.claim(Claim::Take).is_err());
    store
        .claim(Claim::Release {
            generation: owner.generation,
        })
        .unwrap();
}

/// Assert that the store's ID outlives a writer's claim.
pub fn store_id_survives_release(store: &dyn Store) {
    let (owner, successor) = take_successor(store);
    assert_eq!(owner.store_id, successor.store_id);
    assert!(successor.generation > owner.generation);
}

/// Assert that a superseded generation can no longer append.
pub fn stale_writer_cannot_append(store: &dyn Store) {
    let (owner, _) = take_successor(store);
    assert!(
        store
            .append(owner.generation, CHANNEL, &[record(1, b"stale")])
            .is_err()
    );
}

/// Assert that a superseded generation cannot trim a channel.
pub fn stale_writer_cannot_trim(store: &dyn Store) {
    let (owner, _) = take_successor(store);
    assert!(
        store
            .trim(owner.generation, CHANNEL, Trim::Keep(1..1))
            .is_err()
    );
}

/// Assert that exact bytes round-trip and positions may have gaps.
pub fn append_round_trips_exact_records(store: &dyn Store) {
    let owner = take(store);
    let records = [record(2, b"\x00\xff"), record(4, b"")];
    store.append(owner.generation, CHANNEL, &records).unwrap();
    assert_eq!(read(store, 1, None, 2), records);
}

/// Assert that appending a position already held cannot replace its record.
pub fn append_rejects_a_nonincreasing_position(store: &dyn Store) {
    let owner = take(store);
    let original = [record(2, b"first")];
    store.append(owner.generation, CHANNEL, &original).unwrap();
    assert!(
        store
            .append(owner.generation, CHANNEL, &[record(2, b"second")])
            .is_err()
    );
    assert_eq!(read(store, 1, None, 2), original);
}

/// Assert that a read cannot pass its exclusive end.
pub fn read_respects_the_exclusive_end(store: &dyn Store) {
    let (_, records) = populate_three(store);
    assert_eq!(read(store, 2, Some(records[2].position), 4), records[1..2]);
}

/// Assert that a trim can keep one interior range of a channel.
pub fn trim_keeps_only_the_requested_range(store: &dyn Store) {
    let (generation, records) = populate_three(store);
    store.trim(generation, CHANNEL, Trim::Keep(2..3)).unwrap();
    assert_eq!(read(store, 1, None, 4), records[1..2]);
}
