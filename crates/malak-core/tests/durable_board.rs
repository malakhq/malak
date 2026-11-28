//! Real disk evidence for durable Direct board startup and writes.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_core::{Action, Durability, GetReply, Grant, Limits, Owner, SendRequest};
use malak_journal::StateFolder;

mod common;
#[path = "durable_board/store_isolation.rs"]
mod store_isolation;
use malak_layers::{
    Claim, Claimed, ReadLimit, ReadResult, Record, Store, StoreInfo, Trim, Trimmed,
};
use malak_store_markdown::MarkdownStore;
use minicbor::Decoder;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn inbox_names_resolve_the_current_enrolment_after_a_durable_restart() {
    let fixture = Fixture::new();
    let board = fixture.board(false);
    let old = board.enrol("reviewer", Grant::default()).unwrap();
    board
        .lead()
        .send(SendRequest::note("to/reviewer", "one", b"old"))
        .unwrap();
    board.revoke(old.id()).unwrap();
    let current = board.enrol("reviewer", Grant::default()).unwrap();
    board
        .lead()
        .send(SendRequest::note("to/reviewer", "one", b"current"))
        .unwrap();
    let old_inbox = old.id().inbox();
    let current_inbox = current.id().inbox();
    drop(old);
    drop(current);
    drop(board);

    let board = fixture.restart();
    let GetReply::Message(message) = board.lead().get("to/reviewer", 1).unwrap() else {
        panic!("missing current inbox");
    };
    assert_eq!(message.channel, current_inbox);
    assert_eq!(&message.body[..], b"current");
    let GetReply::Message(message) = board.lead().get(old_inbox.as_str(), 1).unwrap() else {
        panic!("missing old history");
    };
    assert_eq!(&message.body[..], b"old");
}

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    storage: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "board-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        let state = root.join("state");
        let storage = root.join("records");
        Self {
            root,
            state,
            storage,
        }
    }

    fn board(&self, sync: bool) -> Owner {
        Owner::create_durable(
            MarkdownStore::new(&self.storage, sync),
            Limits::default(),
            &self.state,
        )
        .unwrap()
    }

    fn with_probe(&self, effect: Effect) -> Owner {
        let store = DurableProbe {
            inner: MarkdownStore::new(&self.storage, false),
            effect,
        };
        Owner::create_durable(store, Limits::default(), &self.state).unwrap()
    }

    fn restart(&self) -> Owner {
        Owner::restart_durable(
            MarkdownStore::new(&self.storage, false),
            Limits::default(),
            &self.state,
        )
        .unwrap()
    }

    fn interrupted_start(&self, last_saved: usize) -> PathBuf {
        drop(self.board(false));
        let path = self.root.join("interrupted");
        common::copy_journal_prefix(&self.state, &path, last_saved);
        path
    }

    fn resume_at(&self, state: &Path) -> Result<Owner, malak_core::Error> {
        Owner::create_durable(
            MarkdownStore::new(&self.storage, false),
            Limits::default(),
            state,
        )
    }

    fn compact(&self) {
        let inspected = StateFolder::acquire_existing(&self.state)
            .unwrap()
            .inspect()
            .unwrap();
        let archive = inspected.archive_payload().unwrap();
        inspected.activate().unwrap().compact(&archive).unwrap();
    }

    fn record_file(&self, marker: &[u8]) -> PathBuf {
        fs::read_dir(&self.storage)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                fs::read(path)
                    .unwrap_or_default()
                    .windows(marker.len())
                    .any(|part| part == marker)
            })
            .unwrap()
    }

    fn tags(&self) -> Vec<u8> {
        let loaded = StateFolder::acquire(&self.state)
            .unwrap()
            .inspect()
            .unwrap();
        loaded.entries().map(|(_, bytes)| tag(bytes)).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn tag(bytes: &[u8]) -> u8 {
    let mut d = Decoder::new(bytes);
    assert!(d.array().unwrap().unwrap() >= 2);
    assert_eq!(d.u8().unwrap(), 1);
    d.u8().unwrap()
}

#[test]
fn stop_releases_stable_state_while_old_handles_are_held() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let held = owner.lead();
    let run = owner.status(None, None).run;

    owner.stop(run).unwrap();

    assert!(held.send(SendRequest::note("lead", "late", b"")).is_err());
    let restarted = fixture.restart();
    assert_ne!(restarted.status(None, None).run, run);
}

#[test]
fn a_synced_receipt_is_in_the_record_and_its_journal() {
    let fixture = Fixture::new();
    let owner = fixture.board(true);
    let lead = owner.lead();
    let receipt = lead
        .send(SendRequest::note("lead", "one", b"stored"))
        .unwrap();

    let GetReply::Message(found) = lead.get("lead", receipt.position.get()).unwrap() else {
        panic!("expected a stored message");
    };
    assert_eq!(found.receipt, receipt);
    assert_eq!(receipt.durability, Durability::Synced);

    drop(lead);
    drop(owner);
    assert_eq!(fixture.tags(), [0, 1, 2, 3, 8, 29]);
}

#[test]
fn an_ack_is_saved_before_returning() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let lead = owner.lead();
    let receipt = lead
        .send(SendRequest::note("lead", "one", b"stored"))
        .unwrap();
    lead.read(&["lead"], 1, std::time::Duration::ZERO).unwrap();

    lead.ack("lead", receipt.creation, receipt.position.get())
        .unwrap();

    drop(lead);
    drop(owner);
    assert_eq!(fixture.tags().last(), Some(&15));
}

#[test]
fn the_same_durable_receipt_is_available_after_trimming() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let lead = owner.lead();
    let request = SendRequest::note("lead", "first", b"stored");
    let first = lead.send(request).unwrap();
    owner.trim("lead", first.creation, 2).unwrap();

    let retry = lead.send(request).unwrap();

    let mut expected = first;
    expected.duplicate = true;
    assert_eq!(retry, expected);
    drop(lead);
    drop(owner);
    assert_eq!(fixture.tags(), [0, 1, 2, 3, 8, 29, 10, 11]);
}

#[test]
fn retry_after_restart_and_durability_change_uses_the_trimmed_receipt() {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let request = SendRequest::note("lead", "first", b"stored");
    let original = board.lead().send(request).unwrap();
    assert_eq!(original.durability, Durability::Synced);
    assert_eq!(board.trim("lead", original.creation, 2).unwrap(), 2);
    board.stop(board.status(None, None).run).unwrap();
    drop(board);
    let reopened = fixture.restart();
    assert_eq!(
        reopened.status(None, None).store.durability,
        Durability::Restart
    );

    let retry = reopened.lead().send(request).unwrap();

    let mut expected = original;
    expected.duplicate = true;
    assert_eq!(retry, expected);
}

#[test]
fn an_enrolment_and_its_inbox_are_one_journal_entry() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let mut grant = Grant::default();
    grant.allow("lead".parse().unwrap(), [Action::Send]);

    let handle = owner.enrol("participant", grant).unwrap();

    assert!(matches!(
        owner.lead().get(handle.id().inbox().as_str(), 1).unwrap(),
        GetReply::NeverWritten { .. }
    ));
    drop(handle);
    drop(owner);
    assert_eq!(fixture.tags(), [0, 1, 2, 3, 4]);
}

#[test]
fn an_interrupted_first_start_keeps_the_saved_keys() {
    let fixture = Fixture::new();
    let interrupted = fixture.interrupted_start(1);

    drop(fixture.resume_at(&interrupted).unwrap());

    let original = StateFolder::acquire(&fixture.state)
        .unwrap()
        .inspect()
        .unwrap();
    let resumed = StateFolder::acquire(&interrupted)
        .unwrap()
        .inspect()
        .unwrap();
    assert!(original.entries().next().unwrap() == resumed.entries().next().unwrap());
    assert_eq!(
        resumed
            .entries()
            .map(|(_, bytes)| tag(bytes))
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[test]
fn an_interrupted_first_start_keeps_the_saved_lead_inbox() {
    let fixture = Fixture::new();
    let interrupted = fixture.interrupted_start(2);
    let original = StateFolder::acquire(&fixture.state)
        .unwrap()
        .inspect()
        .unwrap();
    let mut lead = Decoder::new(original.entries().nth(1).unwrap().1);
    lead.array().unwrap();
    lead.u8().unwrap();
    lead.u8().unwrap();
    let expected = malak_core::CreationId::from_bytes(lead.bytes().unwrap().try_into().unwrap());

    let owner = fixture.resume_at(&interrupted).unwrap();

    assert!(matches!(owner.lead().get("lead", 1).unwrap(),
        GetReply::NeverWritten { creation } if creation == expected));
}

#[test]
fn a_saved_initialization_marker_is_not_written_again() {
    let fixture = Fixture::new();
    let interrupted = fixture.interrupted_start(3);

    drop(fixture.resume_at(&interrupted).unwrap());

    let loaded = StateFolder::acquire(&interrupted)
        .unwrap()
        .inspect()
        .unwrap();
    assert_eq!(
        loaded
            .entries()
            .map(|(_, bytes)| tag(bytes))
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[test]
fn a_store_mismatch_leaves_interrupted_initialization_untouched() {
    let fixture = Fixture::new();
    let interrupted = fixture.interrupted_start(1);

    let result = Owner::create_durable(
        MarkdownStore::new(fixture.root.join("other-store"), false),
        Limits::default(),
        &interrupted,
    );

    assert_eq!(
        result.err().unwrap().outcome(),
        malak_core::Outcome::Refused
    );
    let loaded = StateFolder::acquire(&interrupted)
        .unwrap()
        .inspect()
        .unwrap();
    assert_eq!(loaded.loaded().head.sequence, 1);
}

#[test]
fn retrying_after_restart_returns_the_original_receipt() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let send = SendRequest::note("lead", "id-1", b"the original");
    let original = owner.lead().send(send).unwrap();
    drop(owner);
    let restored = fixture.restart();

    let retry = restored.lead().send(send).unwrap();

    let mut expected = original;
    expected.duplicate = true;
    assert_eq!(retry, expected);
}

#[test]
fn an_ack_survives_restart() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let lead = owner.lead();
    let first = lead
        .send(SendRequest::note("lead", "one", b"read before restart"))
        .unwrap();
    let later = lead
        .send(SendRequest::note("lead", "two", b"due after restart"))
        .unwrap();
    lead.read(&["lead"], 1, std::time::Duration::ZERO).unwrap();
    lead.ack("lead", first.creation, first.position.get())
        .unwrap();
    drop(lead);
    drop(owner);
    let restored = fixture.restart();

    let reply = restored
        .lead()
        .read(&["lead"], 2, std::time::Duration::ZERO)
        .unwrap();

    assert_eq!(reply.messages.len(), 1);
    assert_eq!(reply.messages[0].receipt, later);
}

#[test]
fn a_revoked_participant_stays_revoked_after_restart() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let participant = owner.enrol("worker", Grant::default()).unwrap();
    let id = participant.id().clone();
    owner.revoke(&id).unwrap();
    drop(participant);
    drop(owner);
    let restored = fixture.restart();

    let outcome = restored.handle(&id);

    assert_eq!(
        outcome.err().unwrap().outcome(),
        malak_core::Outcome::Refused
    );
}

#[test]
fn a_new_write_keeps_the_current_creation_and_advances_its_position() {
    let fixture = Fixture::new();
    let first = fixture
        .board(false)
        .lead()
        .send(SendRequest::note("lead", "one", b"first"))
        .unwrap();
    let restarted = fixture.restart();

    let next = restarted
        .lead()
        .send(SendRequest::note("lead", "two", b"second"))
        .unwrap();

    assert_eq!(next.creation, first.creation);
    assert_eq!(next.position.get(), first.position.get() + 1);
}

#[test]
fn a_missing_record_stops_only_its_channel_after_restart() {
    let fixture = Fixture::new();
    let owner = fixture.board(false);
    let lead = owner.lead();
    let missing = lead
        .send(SendRequest::note("lead", "lost", b"gone"))
        .unwrap();
    let kept = lead
        .send(SendRequest::note("notes", "kept", b"still here"))
        .unwrap();
    drop(lead);
    drop(owner);
    let store = MarkdownStore::new(&fixture.storage, false);
    let Claimed::Taken(ownership) = store.claim(Claim::Take).unwrap() else {
        panic!("expected an exclusive claim");
    };
    store
        .trim(
            ownership.generation,
            &malak_record::store_name(&"lead".parse().unwrap(), missing.creation),
            Trim::Keep(1..1),
        )
        .unwrap();
    store
        .claim(Claim::Release {
            generation: ownership.generation,
        })
        .unwrap();
    let restored = fixture.restart();

    assert_eq!(
        restored
            .lead()
            .get("lead", missing.position.get())
            .unwrap_err()
            .outcome(),
        malak_core::Outcome::Unavailable
    );
    assert!(
        matches!(restored.lead().get("notes", kept.position.get()).unwrap(),
        GetReply::Message(message) if message.receipt == kept)
    );
}

#[test]
fn an_existing_state_folder_is_preserved_when_a_new_board_is_refused() {
    let fixture = Fixture::new();
    drop(fixture.board(false));
    let entries = fixture.tags();

    let result = Owner::create_durable(
        MarkdownStore::new(&fixture.storage, false),
        Limits::default(),
        &fixture.state,
    );

    assert_eq!(
        result.err().unwrap().outcome(),
        malak_core::Outcome::Refused
    );
    assert_eq!(fixture.tags(), entries);
}

#[test]
fn a_saved_trim_finishes_after_restart() {
    let fixture = Fixture::new();
    let owner = fixture.with_probe(Effect::RefuseTrim);
    let lead = owner.lead();
    let removed = lead.send(SendRequest::note("lead", "old", b"old")).unwrap();
    let kept = lead.send(SendRequest::note("lead", "new", b"new")).unwrap();
    assert!(
        owner
            .trim("lead", removed.creation, kept.position.get())
            .is_err()
    );
    drop(lead);
    drop(owner);
    let restarted = fixture.restart();

    let answer = restarted
        .lead()
        .get("lead", removed.position.get())
        .unwrap();

    assert!(matches!(answer, GetReply::Trimmed { before, .. } if before == kept.position.get()));
    assert!(
        matches!(restarted.lead().get("lead", kept.position.get()).unwrap(),
        GetReply::Message(message) if message.receipt == kept)
    );
    drop(restarted);
    assert!(fixture.tags().contains(&11));
}

#[test]
fn restart_finishes_a_saved_removal_and_keeps_its_successor() {
    let fixture = Fixture::new();
    let owner = fixture.with_probe(Effect::RefuseTrim);
    let lead = owner.lead();
    let removed = lead
        .send(SendRequest::note("tasks", "old", b"marked-for-removal"))
        .unwrap();
    assert!(owner.remove_channel("tasks", removed.creation).is_err());
    let kept = lead
        .send(SendRequest::note("tasks", "new", b"kept"))
        .unwrap();
    assert_ne!(removed.creation, kept.creation);
    let old = fixture.record_file(b"marked-for-removal");
    drop(lead);
    drop(owner);

    let restarted = fixture.restart();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while fs::metadata(&old).unwrap().len() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "pending removal did not trim the old creation"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    assert!(matches!(restarted.lead().get("tasks", 1).unwrap(),
        GetReply::Message(message) if message.receipt == kept && message.body.as_ref() == b"kept"));
}

#[test]
fn a_saved_cut_releases_the_position_after_restart() {
    let fixture = Fixture::new();
    let owner = fixture.with_probe(Effect::RefuseWriteAndCut);
    let message = SendRequest::note("lead", "new", b"new");
    assert!(owner.lead().send(message).is_err());
    drop(owner);
    let restarted = fixture.restart();

    let receipt = restarted.lead().send(message).unwrap();

    assert_eq!(receipt.position.get(), 1);
    drop(restarted);
    assert!(fixture.tags().contains(&13));
}

#[test]
fn an_empty_interrupted_append_can_be_retried_after_restart() {
    let fixture = Fixture::new();
    let owner = fixture.with_probe(Effect::RefuseWriteAndCut);
    let message = SendRequest::note("lead", "id-1", b"body");
    assert!(owner.lead().send(message).is_err());
    drop(owner);
    let path = fixture.root.join("after-reserve");
    common::copy_journal_prefix(&fixture.state, &path, 5);
    let restarted = Owner::restart_durable(
        MarkdownStore::new(&fixture.storage, false),
        Limits::default(),
        &path,
    )
    .unwrap();

    let receipt = restarted.lead().send(message).unwrap();

    assert_eq!(receipt.position.get(), 1);
}

#[test]
fn dropping_a_durable_board_releases_its_store_before_returning() {
    let fixture = Fixture::new();
    let board = fixture.with_probe(Effect::DelayRelease);

    drop(board);

    let store = MarkdownStore::new(&fixture.storage, false);
    assert!(matches!(
        store.claim(Claim::Take).unwrap(),
        Claimed::Taken(_)
    ));
}

enum Effect {
    CheckReservation(PathBuf),
    RefuseTrim,
    RefuseWriteAndCut,
    DelayRelease,
    SuppressWitness,
}

struct DurableProbe {
    inner: MarkdownStore,
    effect: Effect,
}

impl Store for DurableProbe {
    fn describe(&self) -> StoreInfo {
        self.inner.describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, malak_layers::Error> {
        if matches!(mode, Claim::Release { .. }) && matches!(self.effect, Effect::DelayRelease) {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        self.inner.claim(mode)
    }

    fn append(
        &self,
        generation: u64,
        channel: &str,
        records: &[Record],
    ) -> Result<(), malak_layers::Error> {
        if channel == "_witness" {
            if matches!(self.effect, Effect::SuppressWitness) {
                return Err(malak_layers::Error::unavailable("injected witness failure"));
            }
            return self.inner.append(generation, channel, records);
        }
        match &self.effect {
            Effect::CheckReservation(state) => check_reservation(state),
            Effect::RefuseWriteAndCut => {
                return Err(malak_layers::Error::unavailable("injected append failure"));
            }
            Effect::RefuseTrim | Effect::DelayRelease | Effect::SuppressWitness => (),
        }
        self.inner.append(generation, channel, records)
    }

    fn read(
        &self,
        channel: &str,
        from: malak_layers::Position,
        limit: ReadLimit,
    ) -> Result<ReadResult, malak_layers::Error> {
        self.inner.read(channel, from, limit)
    }

    fn trim(
        &self,
        generation: u64,
        channel: &str,
        trim: Trim<'_>,
    ) -> Result<Trimmed, malak_layers::Error> {
        if channel != "_witness"
            && matches!(self.effect, Effect::RefuseTrim | Effect::RefuseWriteAndCut)
        {
            return Err(malak_layers::Error::unavailable("injected trim failure"));
        }
        self.inner.trim(generation, channel, trim)
    }
}

fn check_reservation(state: &Path) {
    let path = state.join("entries-00000000000000000001");
    let mut replay = malak_journal::Replay::new(malak_journal::Checkpoint::EMPTY);
    let mut last = None;
    replay
        .file(1, &fs::read(path).unwrap(), |_, data| {
            let tag = tag(data);
            if !matches!(tag, 19 | 20) {
                last = Some(tag);
            }
        })
        .unwrap();
    assert_eq!(last, Some(8));
}

#[test]
fn the_journal_reservation_precedes_the_store_append() {
    let fixture = Fixture::new();
    let owner = fixture.with_probe(Effect::CheckReservation(fixture.state.clone()));

    owner
        .lead()
        .send(SendRequest::note("lead", "one", b"stored"))
        .unwrap();
}

#[test]
fn a_compacted_board_replays_its_receipts_and_later_entries() {
    let fixture = Fixture::new();
    let message = SendRequest::note("lead", "archived", b"archived");
    let original = fixture.board(false).lead().send(message).unwrap();
    fixture.compact();
    let restarted = fixture.restart();
    let later = restarted
        .lead()
        .send(SendRequest::note("lead", "later", b"later"))
        .unwrap();
    drop(restarted);

    let restarted = fixture.restart();

    let retry = restarted.lead().send(message).unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.position, original.position);
    assert_eq!(retry.creation, original.creation);
    assert!(
        matches!(restarted.lead().get("lead", later.position.get()).unwrap(),
        GetReply::Message(message) if message.receipt == later)
    );
}

#[test]
fn a_second_compaction_replays_an_existing_archive() {
    let fixture = Fixture::new();
    drop(fixture.board(false));
    fixture.compact();
    let request = SendRequest::note("lead", "second", b"second");
    let original = fixture.restart().lead().send(request).unwrap();
    fixture.compact();

    let receipt = fixture.restart().lead().send(request).unwrap();

    assert!(receipt.duplicate);
    assert_eq!(receipt.position, original.position);
    assert_eq!(receipt.creation, original.creation);
}

#[test]
fn restart_adopts_consecutive_verifying_records_without_saved_reservations() {
    let fixture = Fixture::new();
    let owner = fixture.with_probe(Effect::SuppressWitness);
    let requests = [
        SendRequest::note("lead", "one", b"one"),
        SendRequest::note("lead", "two", b"two"),
    ];
    let saved = requests.map(|request| owner.lead().send(request).unwrap());
    drop(owner);
    let old = fixture.root.join("before-writes");
    common::copy_journal_prefix(&fixture.state, &old, 4);
    let restored = Owner::restart_durable(
        MarkdownStore::new(&fixture.storage, false),
        Limits::default(),
        &old,
    )
    .unwrap();

    let replayed = requests.map(|request| restored.lead().send(request).unwrap());

    assert!(replayed.iter().all(|receipt| receipt.duplicate));
    assert_eq!(
        replayed.map(|receipt| receipt.position),
        saved.map(|receipt| receipt.position)
    );
    assert_eq!(
        replayed.map(|receipt| receipt.creation),
        saved.map(|receipt| receipt.creation)
    );
}

#[test]
fn restart_cuts_an_unparseable_tail_without_stopping_the_channel() {
    use std::io::Write;

    let fixture = Fixture::new();
    let original = fixture
        .board(false)
        .lead()
        .send(SendRequest::note("lead", "whole", b"whole"))
        .unwrap();
    let file = fixture.record_file(b"whole");
    fs::OpenOptions::new()
        .append(true)
        .open(file)
        .unwrap()
        .write_all(b"torn tail")
        .unwrap();

    let restarted = fixture.restart();

    assert!(matches!(restarted.lead().get("lead", 1).unwrap(),
        GetReply::Message(message) if message.receipt == original));
    assert_eq!(
        restarted
            .lead()
            .send(SendRequest::note("lead", "next", b"next"))
            .unwrap()
            .position
            .get(),
        2
    );
}

#[test]
fn the_trace_aware_head_uses_the_canonical_digest_without_trace() {
    use malak_core::HeaderValue;
    let fixture = Fixture::new();
    let owner = fixture.board(true);
    let request = SendRequest {
        headers: &[("trace", HeaderValue::Text("aBcD"))],
        ..SendRequest::note("lead", "one", b"stored")
    };
    owner.lead().send(request).unwrap();
    drop(owner);

    let journal = StateFolder::acquire(&fixture.state)
        .unwrap()
        .inspect()
        .unwrap();
    let (_, payload) = journal
        .entries()
        .find(|(_, bytes)| tag(bytes) == 29)
        .unwrap();
    let mut input = Decoder::new(payload);
    assert_eq!(input.array().unwrap(), Some(14));
    for _ in 0..11 {
        input.skip().unwrap();
    }
    // SHA-256("malak/retry/v3\0" || 82 02 80 || SHA-256("stored")).
    let expected = [
        0xca, 0x4f, 0x43, 0x95, 0xf8, 0x81, 0x95, 0xd8, 0x6a, 0x40, 0x30, 0x63, 0x44, 0x8c, 0x79,
        0xc7, 0xb6, 0x00, 0xc9, 0xa6, 0x54, 0x31, 0xfc, 0x50, 0x7b, 0x92, 0x50, 0xab, 0xb4, 0xc0,
        0xbb, 0x81,
    ];
    assert_eq!(input.bytes().unwrap(), expected);
    assert_eq!(input.u8().unwrap(), 2);
    assert_eq!(input.str().unwrap(), "aBcD");
    assert_eq!(input.position(), payload.len());
}

#[path = "durable_board/implicit_trace.rs"]
mod implicit_trace;
