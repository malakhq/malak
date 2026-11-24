//! Ordinary store behavior on real Markdown files.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_layers::{
    Claim, Claimed, Durability, KeptRecord, Ownership, Position, ReadLimit, ReadResult, Record,
    Store, StorePlace, Trim, Trimmed,
};
use malak_store_markdown::MarkdownStore;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
const CHANNEL: &str = "lead/_0123456789abcdef0123456789abcdef";

struct Fixture {
    path: PathBuf,
    store: MarkdownStore,
}

impl Fixture {
    fn new(sync: bool) -> Self {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "md-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        let store = MarkdownStore::new(&path, sync);
        Self { path, store }
    }

    fn channel(&self) -> PathBuf {
        fs::read_dir(&self.path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| entry.extension().is_some_and(|ext| ext == "md"))
            .unwrap()
    }

    fn take(&self) -> Ownership {
        take(&self.store)
    }

    fn release(&self, generation: u64) {
        self.store.claim(Claim::Release { generation }).unwrap();
    }

    fn append(&self, generation: u64, position: u64, data: &'static [u8]) {
        self.store
            .append(generation, CHANNEL, &[record(position, data)])
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.path.exists() {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }
}

fn record(position: u64, data: &'static [u8]) -> Record {
    Record {
        position: position.try_into().unwrap(),
        data: data.into(),
    }
}

fn take(store: &dyn Store) -> Ownership {
    let Claimed::Taken(ownership) = store.claim(Claim::Take).unwrap() else {
        panic!("expected an ordinary claim");
    };
    ownership
}

fn read(store: &MarkdownStore) -> (Vec<Record>, bool) {
    let result = store
        .read(
            CHANNEL,
            Position::FIRST,
            ReadLimit {
                records: 16,
                before: None,
            },
        )
        .unwrap();
    let ReadResult::Records {
        records,
        unparseable,
    } = result
    else {
        panic!("expected an ordinary read");
    };
    (records, unparseable)
}

macro_rules! ordinary {
    ($($test:ident),* $(,)?) => {$(
        #[test]
        fn $test() {
            let fixture = Fixture::new(false);
            malak_testkit::store::$test(&fixture.store)
        }
    )*};
}

ordinary!(
    claim_is_exclusive,
    store_id_survives_release,
    stale_writer_cannot_append,
    stale_writer_cannot_trim,
    append_round_trips_exact_records,
    append_rejects_a_nonincreasing_position,
    read_respects_the_exclusive_end,
    trim_keeps_only_the_requested_range,
);

#[test]
fn restart_and_synced_modes_describe_their_durability() {
    assert_eq!(
        Fixture::new(false).store.describe().durability,
        Durability::Restart
    );
    assert_eq!(
        Fixture::new(true).store.describe().durability,
        Durability::Synced
    );
}

#[test]
fn a_second_instance_cannot_claim_an_owned_directory() {
    let fixture = Fixture::new(false);
    let first = fixture.take();
    let other = MarkdownStore::new(&fixture.path, false);

    assert!(other.claim(Claim::Take).is_err());
    fixture.release(first.generation);
    assert_eq!(take(&other).store_id, first.store_id);
}

#[test]
fn a_successor_reports_existing_records_and_preserves_the_store_id() {
    let fixture = Fixture::new(true);
    let first = fixture.take();
    fixture.append(first.generation, 1, b"persisted");
    fixture.release(first.generation);

    let other = MarkdownStore::new(&fixture.path, false);
    let successor = take(&other);

    assert!(successor.has_records);
    assert_eq!(successor.store_id, first.store_id);
    assert_eq!(read(&other), (vec![record(1, b"persisted")], false));
}

#[test]
fn a_torn_writer_slot_preserves_the_previous_identity() {
    let fixture = Fixture::new(false);
    let first = fixture.take();
    fixture.release(first.generation);
    OpenOptions::new()
        .append(true)
        .open(fixture.path.join(".owner"))
        .unwrap()
        .write_all(b"partial next slot")
        .unwrap();

    let successor = fixture.take();

    assert_eq!(successor.store_id, first.store_id);
    assert_eq!(successor.generation, first.generation + 1);
}

#[test]
fn take_preserves_a_torn_channel_until_an_explicit_cut() {
    let fixture = Fixture::new(false);
    let first = fixture.take();
    fixture.append(first.generation, 1, b"whole");
    fixture.release(first.generation);
    let file = fixture.channel();
    OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"<!-- malak 1 2")
        .unwrap();
    let original = fs::read(&file).unwrap();

    let current = fixture.take();
    assert_eq!(read(&fixture.store), (vec![record(1, b"whole")], true));
    assert_eq!(fs::read(&file).unwrap(), original);
    fixture
        .store
        .trim(current.generation, CHANNEL, Trim::Keep(1..2))
        .unwrap();
    fixture
        .store
        .claim(Claim::Settle {
            generation: current.generation,
            channel: CHANNEL,
        })
        .unwrap();
    fixture.append(current.generation, 2, b"after cut");

    assert_eq!(
        read(&fixture.store),
        (vec![record(1, b"whole"), record(2, b"after cut")], false)
    );
}

#[test]
fn a_tail_cut_cannot_discard_an_unparseable_retained_record() {
    let fixture = Fixture::new(false);
    let owner = fixture.take();
    fixture.append(owner.generation, 1, b"whole");
    let file = fixture.channel();
    OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"bad")
        .unwrap();
    let bytes = fs::read(&file).unwrap();

    assert!(
        fixture
            .store
            .trim(owner.generation, CHANNEL, Trim::Keep(1..3))
            .is_err()
    );
    assert_eq!(fs::read(file).unwrap(), bytes);
}

#[test]
fn append_refuses_an_externally_changed_file_length() {
    let fixture = Fixture::new(false);
    let owner = fixture.take();
    fixture.append(owner.generation, 1, b"whole");
    let file = fixture.channel();
    OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let bytes = fs::read(&file).unwrap();

    assert!(
        fixture
            .store
            .append(owner.generation, CHANNEL, &[record(2, b"new")])
            .is_err()
    );
    assert_eq!(fs::read(file).unwrap(), bytes);
}

#[test]
fn a_user_cannot_supply_a_second_record_marker_in_the_body() {
    let fixture = Fixture::new(false);
    let owner = fixture.take();
    let payload = b"before <!-- malak 1 after";

    fixture.append(owner.generation, 1, payload);

    let bytes = fs::read(fixture.channel()).unwrap();
    assert_eq!(
        bytes
            .windows(13)
            .filter(|part| *part == b"<!-- malak 1 ")
            .count(),
        1
    );
    assert_eq!(read(&fixture.store), (vec![record(1, payload)], false));
}

#[test]
fn a_sealed_record_has_a_readable_body_with_a_safe_fence() {
    let fixture = Fixture::new(false);
    let owner = fixture.take();
    let key = malak_record::RecordKey::new(&[17; 32]);
    let stamp = malak_record::Stamp {
        run: [19; 16],
        attempt: 0,
    };
    let body = b"one ```` code run";
    let signed = key
        .seal(CHANNEL, Position::FIRST, stamp, b"fields", body)
        .unwrap();

    fixture
        .store
        .append(owner.generation, CHANNEL, &[signed])
        .unwrap();

    let text = fs::read_to_string(fixture.channel()).unwrap();
    assert!(text.contains("`````text\none ```` code run\n`````"));
}

#[test]
fn maximum_channel_names_fit_a_single_file_and_can_be_trimmed() {
    let fixture = Fixture::new(false);
    let owner = fixture.take();
    let name = format!("{}/_0123456789abcdef0123456789abcdef", "a".repeat(128));
    fixture
        .store
        .append(
            owner.generation,
            &name,
            &[record(1, b"old"), record(2, b"new")],
        )
        .unwrap();

    fixture
        .store
        .trim(owner.generation, &name, Trim::Keep(2..3))
        .unwrap();
}

#[test]
fn ordinary_and_repair_claims_report_the_same_directory_identity() {
    let fixture = Fixture::new(false);
    let ordinary = fixture.take();
    fixture.release(ordinary.generation);

    let Claimed::Repair {
        ownership, place, ..
    } = fixture.store.claim(Claim::Repair).unwrap()
    else {
        panic!("expected repair claim");
    };

    assert_eq!(place, StorePlace::OwnerConfirmation);
    assert_eq!(ownership.location, ordinary.location);
    let location = ownership.location.unwrap();
    assert!(!location.filesystem_id.is_empty());
    assert_eq!(
        location.folder_number,
        fs::metadata(&fixture.path).unwrap().ino()
    );
}

fn repair(fixture: &Fixture) -> Ownership {
    let Claimed::Repair {
        ownership,
        channels,
        ..
    } = fixture.store.claim(Claim::Repair).unwrap()
    else {
        panic!("expected repair claim");
    };
    assert_eq!(channels, [CHANNEL]);
    ownership
}

fn candidate(fixture: &Fixture) -> (KeptRecord, [u8; 32]) {
    let ReadResult::Repair {
        candidates,
        digest,
        exact,
        ..
    } = fixture
        .store
        .read(
            CHANNEL,
            Position::FIRST,
            ReadLimit {
                records: 0,
                before: Some(Position::FIRST),
            },
        )
        .unwrap()
    else {
        panic!("expected an independent candidate scan");
    };
    assert!(!exact);
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    (
        KeptRecord {
            position: candidate.record.position,
            digest: Sha256::digest(&candidate.encoded).into(),
        },
        digest,
    )
}

fn a_channel_with_stray_prefix() -> Fixture {
    let fixture = Fixture::new(false);
    let owner = fixture.take();
    fixture.append(owner.generation, 1, b"message");
    fixture.release(owner.generation);
    let file = fixture.channel();
    let mut bytes = b"stray byte without a line break".to_vec();
    bytes.extend_from_slice(&fs::read(&file).unwrap());
    fs::write(file, bytes).unwrap();
    fixture
}

#[test]
fn repair_read_finds_a_marker_after_stray_bytes_regardless_of_read_bounds() {
    let fixture = a_channel_with_stray_prefix();
    let owner = repair(&fixture);

    let (record, digest) = candidate(&fixture);

    assert_eq!(record.position, Position::FIRST);
    assert_eq!(
        digest,
        <[u8; 32]>::from(Sha256::digest(fs::read(fixture.channel()).unwrap()))
    );
    fixture.release(owner.generation);
}

#[test]
fn a_repair_preserves_the_original_bytes_and_can_be_repeated() {
    let fixture = a_channel_with_stray_prefix();
    let owner = repair(&fixture);
    let original = fs::read(fixture.channel()).unwrap();
    let (record, digest) = candidate(&fixture);
    let trim = Trim::Repair {
        plan: [3; 32],
        input: digest,
        keep: &[record],
    };

    assert_eq!(
        fixture
            .store
            .trim(owner.generation, CHANNEL, trim.clone())
            .unwrap(),
        Trimmed::Done
    );
    assert_eq!(
        fixture.store.trim(owner.generation, CHANNEL, trim).unwrap(),
        Trimmed::AlreadyRewritten
    );

    let kept = fs::read(fixture.channel()).unwrap();
    assert_eq!(record.digest, <[u8; 32]>::from(Sha256::digest(&kept)));
    let backup = fs::read_dir(&fixture.path)
        .unwrap()
        .map(|entry| entry.unwrap())
        .find(|entry| entry.file_name().to_str().unwrap().starts_with(".repair-"))
        .unwrap();
    assert_eq!(fs::read(backup.path()).unwrap(), original);
    fixture.release(owner.generation);
}

#[test]
fn repair_refuses_a_changed_channel_without_creating_a_backup() {
    let fixture = a_channel_with_stray_prefix();
    let owner = repair(&fixture);
    let (record, digest) = candidate(&fixture);
    let file = fixture.channel();
    let mut changed = fs::read(&file).unwrap();
    changed.push(b'!');
    fs::write(&file, &changed).unwrap();

    let result = fixture
        .store
        .trim(
            owner.generation,
            CHANNEL,
            Trim::Repair {
                plan: [3; 32],
                input: digest,
                keep: &[record],
            },
        )
        .unwrap();

    assert_eq!(result, Trimmed::ChangedInput);
    assert_eq!(fs::read(file).unwrap(), changed);
    assert!(!fs::read_dir(&fixture.path).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_str()
            .unwrap()
            .starts_with(".repair-")
    }));
    fixture.release(owner.generation);
}
