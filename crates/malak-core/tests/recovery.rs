//! Inspect, apply, and withdraw recovery plans against real Markdown boards.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;
#[path = "recovery/scanning.rs"]
mod scanning;

use malak_core::{GetReply, Limits, Outcome, Owner, Position, RecoveryReport, SendRequest};
use malak_layers::{
    Claim, Claimed, Error, ReadLimit, ReadResult, Record, Store, StoreInfo, StorePlace, Trim,
    Trimmed,
};

use malak_journal::{Checkpoint, StateFolder};
use malak_store_markdown::MarkdownStore;
use malak_testkit::unix_socket::SocketDir;
use std::fs;
use std::path::{Path, PathBuf};

struct Fixture {
    _root: SocketDir,
    state: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        let store = root.path().join("store");
        Self {
            _root: root,
            state,
            store,
        }
    }

    fn damaged() -> (Self, PathBuf) {
        let (fixture, path, mut bytes) = Self::populated();
        let starts = markers(&bytes);
        bytes.drain(starts[1]..starts[2]);
        fs::write(&path, bytes).unwrap();
        fixture.stop_for_damage(2);
        (fixture, path)
    }

    fn populated() -> (Self, PathBuf, Vec<u8>) {
        let fixture = Self::new();
        let board = fixture.create();
        let lead = board.lead();
        for id in ["one", "two", "three"] {
            lead.send(SendRequest::note("lead", id, id.as_bytes()))
                .unwrap();
        }
        drop(lead);
        stop(board);
        let (path, bytes) = fs::read_dir(&fixture.store)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                if path.extension().is_none_or(|extension| extension != "md") {
                    return None;
                }
                let bytes = fs::read(&path).unwrap();
                (markers(&bytes).len() == 3).then_some((path, bytes))
            })
            .next()
            .unwrap();
        (fixture, path, bytes)
    }

    fn create(&self) -> Owner {
        Owner::create_durable(self.store(), Limits::default(), &self.state).unwrap()
    }

    fn restart(&self) -> Owner {
        Owner::restart_durable(self.store(), Limits::default(), &self.state).unwrap()
    }

    fn stop_for_damage(&self, position: u64) {
        let restored = self.restart();
        let error = restored.lead().get("lead", position).unwrap_err();
        assert_eq!(error.outcome(), Outcome::Unavailable);
        stop(restored);
    }

    fn leave_older_state_beside(&self, channel: &str) -> PathBuf {
        stop(self.create());
        let older = self._root.path().join("older");
        common::copy_journal_prefix(&self.state, &older, usize::MAX);
        let current = self.restart();
        if channel != "lead" {
            current.create_channel(channel).unwrap();
        }
        current
            .lead()
            .send(SendRequest::note(channel, "new-run", b"new run"))
            .unwrap();
        stop(current);
        self.remove_witness();
        older
    }

    fn remove_witness(&self) {
        let store = self.store();
        let Claimed::Taken(owner) = store.claim(Claim::Take).unwrap() else {
            panic!("expected an exclusive store claim");
        };
        assert_eq!(
            store
                .trim(owner.generation, "_witness", Trim::Keep(1..1))
                .unwrap(),
            Trimmed::Done
        );
    }

    fn store(&self) -> MarkdownStore {
        MarkdownStore::new(&self.store, false)
    }

    fn head(&self) -> Checkpoint {
        StateFolder::acquire_existing(&self.state)
            .unwrap()
            .inspect()
            .unwrap()
            .loaded()
            .head
    }

    fn look(&self, factor: u64) -> Result<RecoveryReport, malak_core::Error> {
        Owner::look_recovery(self.store(), Limits::default(), &self.state, None, factor)
    }

    fn altered_first_record(&self) -> Vec<u8> {
        let store = self.store();
        let Claimed::Repair { channels, .. } = store.claim(Claim::Repair).unwrap() else {
            panic!("expected a repair claim");
        };
        let name = channels
            .iter()
            .find(|name| name.starts_with("lead/"))
            .unwrap();
        let ReadResult::Repair { candidates, .. } = store
            .read(
                name,
                1.try_into().unwrap(),
                ReadLimit {
                    records: 1,
                    before: None,
                },
            )
            .unwrap()
        else {
            panic!("expected a repair scan");
        };
        let mut record = candidates.into_iter().next().unwrap().record;
        let body = malak_record::unverified_body(&record).unwrap();
        let first_byte = body.as_ptr() as usize - record.data.as_ptr() as usize;
        let mut altered = record.data.to_vec();
        altered[first_byte] ^= 1;
        record.data = altered.into();
        drop(store);

        let staged = self._root.path().join("staged");
        let staging = MarkdownStore::new(&staged, false);
        let Claimed::Taken(owner) = staging.claim(Claim::Take).unwrap() else {
            panic!("expected a staging claim");
        };
        staging.append(owner.generation, name, &[record]).unwrap();
        let path = fs::read_dir(&staged)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|suffix| suffix == "md"))
            .unwrap();
        fs::read(path).unwrap()
    }

    fn channel_containing(&self, body: &[u8]) -> (PathBuf, Vec<u8>) {
        fs::read_dir(&self.store)
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                if path.extension().is_none_or(|suffix| suffix != "md") {
                    return None;
                }
                let bytes = fs::read(&path).unwrap();
                bytes
                    .windows(body.len())
                    .any(|window| window == body)
                    .then_some((path, bytes))
            })
            .next()
            .unwrap()
    }
}

fn stop(board: Owner) {
    board.stop(board.status(None, None).run).unwrap();
}

fn markers(bytes: &[u8]) -> Vec<usize> {
    let marker = b"<!-- malak 1 ";
    bytes
        .windows(marker.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == marker).then_some(index))
        .collect()
}

#[test]
fn recovery_look_reports_a_missing_middle_record_without_changing_state() {
    let (fixture, channel) = Fixture::damaged();
    let before = fixture.head();
    let bytes = fs::read(&channel).unwrap();

    let report = fixture.look(64).unwrap().to_string();

    assert!(report.contains("Keep [1..2)"));
    assert!(report.contains("Give up [2..3) (missing)"));
    assert!(report.contains("Keep [3..4)"));
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(channel).unwrap(), bytes);
}

#[test]
fn recovery_look_keeps_records_after_an_unparseable_stretch() {
    let (fixture, file, original) = Fixture::populated();
    let starts = markers(&original);
    let damaged = [
        &original[..starts[1]],
        b"unparseable second record\n",
        &original[starts[2]..],
    ]
    .concat();
    fs::write(&file, &damaged).unwrap();
    fixture.stop_for_damage(2);
    let before = fixture.head();

    let report = fixture.look(64).unwrap().to_string();

    assert!(report.contains("Keep [1..2)"));
    assert!(report.contains("Give up [2..3) (missing)"));
    assert!(report.contains("Keep [3..4)"));
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(file).unwrap(), damaged);
}

#[test]
fn recovery_reports_the_missing_end_and_preserves_the_head() {
    let (fixture, file, original) = Fixture::populated();
    let damaged = &original[..markers(&original)[2]];
    fs::write(&file, damaged).unwrap();
    fixture.stop_for_damage(3);
    let before = fixture.head();

    let report = fixture.look(64).unwrap().to_string();

    assert!(report.contains("Keep [1..3)"));
    assert!(report.contains("Give up [3..4) (missing)"));
    assert!(report.contains("Head 4"));
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(file).unwrap(), damaged);
}

#[test]
fn recovery_reports_a_changed_record_by_its_failed_check() {
    let (fixture, file, original) = Fixture::populated();
    let altered = fixture.altered_first_record();
    let damaged = [&altered, &original[markers(&original)[1]..]].concat();
    fs::write(&file, &damaged).unwrap();
    fixture.stop_for_damage(1);
    let before = fixture.head();

    let report = fixture.look(64).unwrap().to_string();

    assert!(report.contains("Give up [1..2) (record check failed)"));
    assert!(report.contains("Keep [2..4)"));
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(file).unwrap(), damaged);
}

#[test]
fn recovery_keeps_no_record_from_an_earlier_channel_creation() {
    let fixture = Fixture::new();
    let board = fixture.create();
    let (old, _) = board.create_channel("jobs").unwrap();
    board
        .lead()
        .send(SendRequest::note("jobs", "old-job", b"old creation body"))
        .unwrap();
    let (_, previous) = fixture.channel_containing(b"old creation body");
    board.remove_channel("jobs", old).unwrap();
    let (current, _) = board.create_channel("jobs").unwrap();
    assert_ne!(current, old);
    for (id, body) in [
        ("first", "replaceable body"),
        ("second", "current creation body"),
    ] {
        board
            .lead()
            .send(SendRequest::note("jobs", id, body.as_bytes()))
            .unwrap();
    }
    let (file, original) = fixture.channel_containing(b"current creation body");
    stop(board);
    let kept = &original[markers(&original)[1]..];
    fs::write(&file, [&previous, kept].concat()).unwrap();
    let reopened = fixture.restart();
    assert_eq!(
        reopened.lead().get("jobs", 1).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    stop(reopened);

    let report = fixture
        .apply(&fixture.approved(), b"original", false)
        .unwrap();

    assert!(report.to_string().contains("finished"));
    assert_eq!(fs::read(file).unwrap(), kept);
    let reopened = fixture.restart();
    assert!(matches!(
        reopened.lead().get("jobs", 1).unwrap(),
        GetReply::Void { .. }
    ));
    let GetReply::Message(message) = reopened.lead().get("jobs", 2).unwrap() else {
        panic!("expected a current record");
    };
    assert_eq!(message.receipt.creation, current);
    assert_eq!(message.body.as_ref(), b"current creation body");
}

#[test]
fn a_recovery_plan_binds_the_work_factor() {
    let (fixture, _) = Fixture::damaged();

    let ordinary = fixture.look(64).unwrap().plan_number();
    let raised = fixture.look(128).unwrap().plan_number();

    assert_eq!(fixture.look(64).unwrap().plan_number(), ordinary);
    assert_ne!(ordinary, raised);
    assert!(raised.starts_with("128-"));
}

#[test]
fn recovery_cannot_look_while_the_board_is_open() {
    let fixture = Fixture::new();
    let _board = Owner::create_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();

    let error = fixture.look(64).err().unwrap();

    assert_eq!(error.outcome(), Outcome::Unavailable);
}

#[test]
fn recovery_refuses_a_store_with_only_foreign_records() {
    let (fixture, file, _) = Fixture::populated();
    let (_foreign, _, foreign_bytes) = Fixture::populated();
    fs::write(&file, &foreign_bytes).unwrap();
    fixture.remove_witness();
    let before = fixture.head();

    let error = fixture.look(64).err().unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(error.to_string().contains("no store record verifies"));
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(file).unwrap(), foreign_bytes);
}

#[test]
fn recovery_refuses_older_state_with_new_records_in_unstopped_channels() {
    let fixture = Fixture::new();
    let older = fixture.leave_older_state_beside("lead");

    assert_read_only_rollback_refusal(&fixture, &older);
}

#[test]
fn recovery_refuses_older_state_with_records_in_an_unknown_creation() {
    let fixture = Fixture::new();
    let older = fixture.leave_older_state_beside("new");

    assert_read_only_rollback_refusal(&fixture, &older);
}

fn assert_read_only_rollback_refusal(fixture: &Fixture, older: &Path) {
    let content = |directory: &Path, extension: Option<&str>| {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                extension.is_none_or(|suffix| path.extension().unwrap_or_default() == suffix)
            })
            .map(|path| {
                (
                    path.file_name().unwrap().to_owned(),
                    fs::read(path).unwrap(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = content(older, None);
    let store = content(&fixture.store, Some("md"));

    let error = Owner::look_recovery(fixture.store(), Limits::default(), older, None, 64)
        .err()
        .unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(error.to_string().contains("state folder is older"));
    assert_eq!(content(older, None), before);
    assert_eq!(content(&fixture.store, Some("md")), store);
}

struct Attested {
    store: MarkdownStore,
    place: &'static [u8],
    interrupt_settle: bool,
    pause: Option<RewriteBoundary>,
}

struct RewriteBoundary {
    checkpoint: PathBuf,
    after: bool,
}

impl Attested {
    fn pause_rewrite(&self, after: bool) {
        if let Some(pause) = &self.pause
            && pause.after == after
        {
            malak_testkit::process::pause_at_checkpoint(&pause.checkpoint);
        }
    }
}

impl Store for Attested {
    fn describe(&self) -> StoreInfo {
        self.store.describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        if self.interrupt_settle && matches!(mode, Claim::Settle { .. }) {
            return Err(Error::unavailable("injected interruption after rewrite"));
        }
        match self.store.claim(mode)? {
            Claimed::Repair {
                ownership,
                channels,
                ..
            } => Ok(Claimed::Repair {
                ownership,
                channels,
                place: StorePlace::Established(self.place.to_vec()),
            }),
            result => Ok(result),
        }
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        self.store.append(generation, channel, records)
    }

    fn read(&self, channel: &str, from: Position, bound: ReadLimit) -> Result<ReadResult, Error> {
        self.store.read(channel, from, bound)
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        let rewriting = matches!(&trim, Trim::Repair { .. });
        if rewriting {
            self.pause_rewrite(false);
        }
        let result = self.store.trim(generation, channel, trim)?;
        if rewriting {
            self.pause_rewrite(true);
        }
        Ok(result)
    }
}

impl Fixture {
    fn attested(&self, place: &'static [u8], interrupt_settle: bool) -> Attested {
        Attested {
            store: self.store(),
            place,
            interrupt_settle,
            pause: None,
        }
    }

    fn approved(&self) -> String {
        Owner::look_recovery(
            self.attested(b"original", false),
            Limits::default(),
            &self.state,
            None,
            128,
        )
        .unwrap()
        .plan_number()
    }

    fn apply(
        &self,
        plan: &str,
        place: &'static [u8],
        interrupt_settle: bool,
    ) -> Result<RecoveryReport, Error> {
        Owner::apply_recovery(
            self.attested(place, interrupt_settle),
            Limits::default(),
            &self.state,
            None,
            plan,
        )
    }
}

#[test]
fn recovery_apply_leaves_the_kept_records_and_a_void() {
    let (fixture, _) = Fixture::damaged();
    let number = fixture.approved();

    fixture.apply(&number, b"original", false).unwrap();

    let restarted =
        Owner::restart_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();
    let lead = restarted.lead();
    assert!(matches!(
        lead.get("lead", 2).unwrap(),
        GetReply::Void { .. }
    ));
    let read = lead.read(&["lead"], 10, std::time::Duration::ZERO).unwrap();
    let positions: Vec<_> = read
        .messages
        .iter()
        .map(|message| message.receipt.position.get())
        .collect();
    assert_eq!(positions, [1, 3]);
    assert_eq!(read.channels[0].voids, 1);
}

#[test]
fn a_changed_copy_with_the_same_check_does_not_hide_the_current_record() {
    let (fixture, file, original) = Fixture::populated();
    let starts = markers(&original);
    let first = &original[..starts[1]];
    let third = &original[starts[2]..];
    let altered = fixture.altered_first_record();
    let damaged = [b"stray byte\n", &altered[..], first, first, third].concat();
    fs::write(&file, &damaged).unwrap();
    fixture.stop_for_damage(2);

    let plan = fixture.approved();
    fixture.apply(&plan, b"original", false).unwrap();

    assert_eq!(fs::read(file).unwrap(), [first, third].concat());
    assert_original_set_aside(&fixture, &damaged);
    let board = fixture.restart();
    let read = board
        .lead()
        .read(&["lead"], 8, std::time::Duration::ZERO)
        .unwrap();
    assert_eq!(read.channels[0].voids, 1);
    assert_eq!(
        read.messages
            .iter()
            .map(|message| message.body.as_ref())
            .collect::<Vec<_>>(),
        [b"one".as_slice(), b"three".as_slice()]
    );
}

fn assert_original_set_aside(fixture: &Fixture, damaged: &[u8]) {
    let saved = fs::read_dir(&fixture.store)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".repair-")
        })
        .unwrap();
    assert_eq!(fs::read(saved).unwrap(), damaged);
}

#[test]
fn putting_the_original_record_back_cannot_reverse_a_void() {
    let (fixture, file, original) = Fixture::populated();
    let starts = markers(&original);
    let mut damaged = original.clone();
    damaged.drain(starts[1]..starts[2]);
    fs::write(&file, damaged).unwrap();
    fixture.stop_for_damage(2);
    fixture
        .apply(&fixture.approved(), b"original", false)
        .unwrap();
    fs::write(&file, original).unwrap();

    let board = fixture.restart();
    let lead = board.lead();
    let read = lead.read(&["lead"], 8, std::time::Duration::ZERO).unwrap();

    assert!(matches!(
        lead.get("lead", 2).unwrap(),
        GetReply::Void { .. }
    ));
    assert_eq!(read.channels[0].voids, 1);
    assert_eq!(
        read.messages
            .iter()
            .map(|message| message.receipt.position.get())
            .collect::<Vec<_>>(),
        [1, 3]
    );
    let next = lead
        .send(SendRequest::note("lead", "four", b"four"))
        .unwrap();
    assert_eq!(next.position.get(), 4);
}

#[test]
fn an_accepted_repair_continues_after_setting_aside_the_only_verifying_record() {
    let (fixture, file, original) = Fixture::populated();
    let board = fixture.restart();
    let creation = board.status(None, Some(&"lead".parse().unwrap())).channels[0].creation;
    board.trim("lead", creation, 3).unwrap();
    stop(board);
    fs::write(&file, b"").unwrap();
    fixture.stop_for_damage(3);
    fixture.remove_witness();
    let excluded = &original[..markers(&original)[1]];
    fs::write(&file, excluded).unwrap();
    let plan = fixture.approved();

    kill_repair(&fixture, &plan, true);
    assert!(fs::read(&file).unwrap().is_empty());
    let report = fixture.apply(&plan, b"original", false).unwrap();

    assert!(report.to_string().contains("finished"));
    let board = fixture.restart();
    assert!(matches!(
        board.lead().get("lead", 3).unwrap(),
        GetReply::Void { .. }
    ));
}

#[test]
fn resending_a_voided_receipt_stores_a_new_position() {
    let (fixture, _) = Fixture::damaged();
    fixture
        .apply(&fixture.approved(), b"original", false)
        .unwrap();
    let restarted =
        Owner::restart_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();

    let receipt = restarted
        .lead()
        .send(SendRequest::note("lead", "two", b"two"))
        .unwrap();

    assert!(!receipt.duplicate);
    assert_eq!(receipt.position.get(), 4);
}

#[test]
fn applying_a_plan_without_a_host_attestation_saves_nothing() {
    let (fixture, channel) = Fixture::damaged();
    let before = fixture.head();
    let bytes = fs::read(&channel).unwrap();
    let number = fixture.look(64).unwrap().plan_number();

    let error = Owner::apply_recovery(
        fixture.store(),
        Limits::default(),
        &fixture.state,
        None,
        &number,
    )
    .err()
    .unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(channel).unwrap(), bytes);
}

#[test]
fn a_changed_store_place_leaves_begun_repair_work_pending() {
    let (fixture, channel) = Fixture::damaged();
    let number = fixture.approved();
    assert!(fixture.apply(&number, b"original", true).is_err());
    let before = fixture.head();
    let bytes = fs::read(&channel).unwrap();

    let error = fixture
        .apply(&number, b"restored image", false)
        .err()
        .unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(channel).unwrap(), bytes);
    assert!(
        fixture
            .look(64)
            .unwrap()
            .to_string()
            .contains("rewrite begun")
    );
}

#[test]
fn applying_again_after_the_rewrite_finishes_the_accepted_plan() {
    let (fixture, _) = Fixture::damaged();
    let number = fixture.approved();
    assert!(fixture.apply(&number, b"original", true).is_err());

    let done = fixture.apply(&number, b"original", false).unwrap();

    assert!(done.to_string().contains("finished"));
    let restarted =
        Owner::restart_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();
    assert!(matches!(
        restarted.lead().get("lead", 2).unwrap(),
        GetReply::Void { .. }
    ));
}

#[test]
fn withdrawal_requires_no_store_and_leaves_the_channel_stopped() {
    let (fixture, _) = Fixture::damaged();
    let number = fixture.approved();
    assert!(fixture.apply(&number, b"original", true).is_err());
    let moved = fixture.store.with_extension("moved");
    fs::rename(&fixture.store, &moved).unwrap();

    let report =
        Owner::withdraw_recovery(Limits::default(), &fixture.state, None, &number).unwrap();

    assert!(report.to_string().contains("withdrawn"));
    assert!(!fixture.store.exists());
    fs::rename(moved, &fixture.store).unwrap();
    let restarted =
        Owner::restart_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();
    assert_eq!(
        restarted.lead().get("lead", 2).unwrap_err().outcome(),
        Outcome::Unavailable
    );
}

#[test]
fn applying_an_unaccepted_plan_after_channel_bytes_change_is_refused() {
    let (fixture, channel) = Fixture::damaged();
    let number = fixture.approved();
    let mut changed = fs::read(&channel).unwrap();
    changed.extend_from_slice(b"new stray bytes");
    fs::write(&channel, &changed).unwrap();
    let before = fixture.head();

    let error = fixture.apply(&number, b"original", false).err().unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(channel).unwrap(), changed);
}

#[test]
fn startup_finishes_an_accepted_plan_before_serving_the_channel() {
    let (fixture, _) = Fixture::damaged();
    let number = fixture.approved();
    assert!(fixture.apply(&number, b"original", true).is_err());

    let restarted = Owner::restart_durable(
        fixture.attested(b"original", false),
        Limits::default(),
        &fixture.state,
    )
    .unwrap();

    assert!(matches!(
        restarted.lead().get("lead", 2).unwrap(),
        GetReply::Void { .. }
    ));
}

#[test]
fn startup_keeps_an_unattested_pending_channel_stopped() {
    let (fixture, _) = Fixture::damaged();
    let number = fixture.approved();
    assert!(fixture.apply(&number, b"original", true).is_err());

    let restarted =
        Owner::restart_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();

    assert_eq!(
        restarted.lead().get("lead", 2).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    stop(restarted);
    assert_eq!(fixture.look(64).unwrap().plan_number(), number);
}

const RECOVERY_CHILD: &str = "MALAK_RECOVERY_CHECKPOINT_CHILD";

#[test]
fn a_kill_at_either_rewrite_boundary_keeps_the_accepted_plan() {
    for after in [false, true] {
        let (fixture, _) = Fixture::damaged();
        let number = fixture.approved();
        kill_repair(&fixture, &number, after);

        assert_eq!(fixture.look(64).unwrap().plan_number(), number);
        let report = fixture.apply(&number, b"original", false).unwrap();
        assert!(report.to_string().contains("finished"));
        let reopened =
            Owner::restart_durable(fixture.store(), Limits::default(), &fixture.state).unwrap();
        assert!(matches!(
            reopened.lead().get("lead", 2).unwrap(),
            GetReply::Void { .. }
        ));
    }
}

fn kill_repair(fixture: &Fixture, plan: &str, after: bool) {
    let root = fixture._root.path();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "recovery_checkpoint_child"])
        .env(RECOVERY_CHILD, root)
        .env("MALAK_RECOVERY_PLAN", plan)
        .env("MALAK_RECOVERY_AFTER_REWRITE", after.to_string());
    malak_testkit::process::kill_after_checkpoint(&mut child, &root.join("checkpoint"));
}

#[test]
fn recovery_checkpoint_child() {
    let Some(root) = std::env::var_os(RECOVERY_CHILD) else {
        return;
    };
    let root = Path::new(&root);
    let store = Attested {
        store: MarkdownStore::new(root.join("store"), false),
        place: b"original",
        interrupt_settle: false,
        pause: Some(RewriteBoundary {
            checkpoint: root.join("checkpoint"),
            after: std::env::var("MALAK_RECOVERY_AFTER_REWRITE").unwrap() == "true",
        }),
    };
    let plan = std::env::var("MALAK_RECOVERY_PLAN").unwrap();
    let result = Owner::apply_recovery(store, Limits::default(), root.join("state"), None, &plan);
    panic!(
        "apply returned before its rewrite checkpoint: {:?}",
        result.err()
    );
}
