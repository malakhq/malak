//! Store contract checks on real SQLite databases.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_layers::{
    Claim, Claimed, Durability, Ownership, Position, ReadLimit, ReadResult, Record, Store, Trim,
};
use malak_store_sqlite::SqliteStore;
use malak_testkit::store as kit;
use std::fs;
use std::path::PathBuf;
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
};

static NEXT: AtomicUsize = AtomicUsize::new(0);
const CHANNEL: &str = "lead/_00000000000000000000000000000001";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        // Cargo's temporary directory may sit below a symlink, such as macOS's /var.
        let parent = fs::canonicalize(env!("CARGO_TARGET_TMPDIR")).unwrap();
        Self(parent.join(format!(
            "sqlite-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::new(&self.0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.0.exists() {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

macro_rules! store_check {
    ($name:ident) => {
        #[test]
        fn $name() {
            let fixture = Fixture::new();
            kit::$name(&fixture.store());
        }
    };
}

store_check!(claim_is_exclusive);
store_check!(store_id_survives_release);
store_check!(stale_writer_cannot_append);
store_check!(stale_writer_cannot_trim);
store_check!(append_round_trips_exact_records);
store_check!(append_rejects_a_nonincreasing_position);
store_check!(read_respects_the_exclusive_end);
store_check!(trim_keeps_only_the_requested_range);

#[test]
fn two_concurrent_database_claims_cannot_both_succeed() {
    let fixture = Fixture::new();
    let start = Barrier::new(2);
    let claimed = Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let claim = || {
            let store = fixture.store();
            start.wait();
            let result = store.claim(Claim::Take).is_ok();
            claimed.wait();
            result
        };
        let first = scope.spawn(claim);
        let second = scope.spawn(claim);
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert_ne!(outcomes[0], outcomes[1]);
}

#[test]
fn a_new_database_connection_keeps_the_store_id() {
    let fixture = Fixture::new();
    let first = take(&fixture.store());

    let second = take(&fixture.store());

    assert_eq!(first.store_id, second.store_id);
    assert!(first.generation < second.generation);
}

#[test]
fn a_front_trim_remembers_the_channel_end_when_no_rows_remain() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let generation = take(&store).generation;
    store.append(generation, CHANNEL, &[record(5)]).unwrap();

    store
        .trim(generation, CHANNEL, Trim::Keep(6..Position::MAX + 1))
        .unwrap();

    assert!(store.append(generation, CHANNEL, &[record(5)]).is_err());
    store.append(generation, CHANNEL, &[record(6)]).unwrap();
}

#[test]
fn a_tail_cut_allows_the_cut_position_to_be_written_again() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let generation = take(&store).generation;
    store
        .append(generation, CHANNEL, &[record(4), record(5)])
        .unwrap();

    store.trim(generation, CHANNEL, Trim::Keep(1..5)).unwrap();

    store.append(generation, CHANNEL, &[record(5)]).unwrap();
}

#[test]
fn the_final_position_round_trips_without_overflowing() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let generation = take(&store).generation;
    let entry = record(Position::MAX);
    store
        .append(generation, CHANNEL, std::slice::from_ref(&entry))
        .unwrap();

    store
        .trim(
            generation,
            CHANNEL,
            Trim::Keep(Position::MAX..Position::MAX + 1),
        )
        .unwrap();

    assert_eq!(read(&store, CHANNEL), [entry]);
}

#[test]
fn unusual_channel_names_are_exact_database_keys() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let generation = take(&store).generation;
    let first = "../x'); DELETE FROM records; --";
    let second = "../X'); DELETE FROM records; --";

    store.append(generation, first, &[record(1)]).unwrap();
    store.append(generation, second, &[record(2)]).unwrap();

    assert_eq!(read(&store, first), [record(1)]);
    assert_eq!(read(&store, second), [record(2)]);
}

#[test]
fn a_database_symlink_is_refused() {
    let fixture = Fixture::new();
    take(&fixture.store());
    let database = fixture.0.join("records.sqlite3");
    fs::rename(&database, fixture.0.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink("elsewhere", &database).unwrap();

    assert!(fixture.store().claim(Claim::Take).is_err());
}

#[test]
fn sqlite_describes_synced_durability() {
    let fixture = Fixture::new();
    assert_eq!(fixture.store().describe().durability, Durability::Synced);
}

fn take(store: &SqliteStore) -> Ownership {
    let Claimed::Taken(ownership) = store.claim(Claim::Take).unwrap() else {
        panic!("expected ordinary claim");
    };
    ownership
}

fn record(position: u64) -> Record {
    Record {
        position: position.try_into().unwrap(),
        data: position.to_be_bytes().to_vec().into(),
    }
}

fn read(store: &SqliteStore, channel: &str) -> Vec<Record> {
    let limit = ReadLimit {
        records: 10,
        before: None,
    };
    let ReadResult::Records {
        records,
        unparseable: false,
    } = store.read(channel, Position::FIRST, limit).unwrap()
    else {
        panic!("expected whole records");
    };
    records
}
