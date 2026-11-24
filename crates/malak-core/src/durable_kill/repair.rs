//! Kill and resume accepted repairs on several real Markdown channels.

use super::{
    TestStore, claim_records as claim, kill, read_records as read, start, start_with_store,
};
use crate::{CreationId, GetReply, Limits, Outcome, Owner, SendRequest};
use malak_layers::{Record, Store, Trim, Trimmed};
use malak_record::store_name;
use malak_store_markdown::MarkdownStore;
use malak_testkit::unix_socket::SocketDir;
use std::path::Path;

const CHANNELS: [&str; 2] = ["alpha", "beta"];
const STEPS: &[&str] = &[
    "input-before",
    "input-after",
    "begun",
    "rewrite-before",
    "rewrite-after",
    "settled",
    "read-back",
    "finished",
];

pub(super) fn apply_child(root: &Path, synced: bool, owner_confirmed: bool) -> ! {
    let plan = std::fs::read_to_string(root.join("plan")).unwrap();
    let result = if owner_confirmed {
        apply_with_confirmation(root, synced, &plan)
    } else {
        apply(root, synced, &plan)
    };
    panic!("apply exited before its checkpoint: {:?}", result.err());
}

fn repair_store(root: &Path, synced: bool) -> TestStore {
    TestStore {
        store: MarkdownStore::new(root.join("store"), synced),
        cut: false,
    }
}

pub(super) fn pending_send() -> SendRequest<'static> {
    SendRequest::note("alpha", "unreported", b"unreported")
}

fn look(
    root: &Path,
    synced: bool,
    work_factor: u64,
) -> Result<crate::RecoveryReport, crate::Error> {
    Owner::look_recovery(
        repair_store(root, synced),
        Limits::default(),
        root.join("state"),
        Some("key"),
        work_factor,
    )
}

fn apply(root: &Path, synced: bool, plan: &str) -> Result<crate::RecoveryReport, crate::Error> {
    Owner::apply_recovery(
        repair_store(root, synced),
        Limits::default(),
        root.join("state"),
        Some("key"),
        plan,
    )
}

fn apply_with_confirmation(
    root: &Path,
    synced: bool,
    plan: &str,
) -> Result<crate::RecoveryReport, crate::Error> {
    Owner::apply_recovery_with_owner_confirmation(
        MarkdownStore::new(root.join("store"), synced),
        Limits::default(),
        root.join("state"),
        Some("key"),
        plan,
    )
}

struct Board {
    root: SocketDir,
    creations: [CreationId; 2],
    original: [Vec<Record>; 2],
    number: String,
    synced: bool,
}

impl Board {
    fn damaged(synced: bool, kept: [u64; 2]) -> Self {
        Self::prepare(synced, kept, false)
    }

    fn with_unreported_head() -> Self {
        Self::prepare(true, [1, 1], true)
    }

    fn prepare(synced: bool, kept: [u64; 2], has_tail: bool) -> Self {
        let root = SocketDir::new().unwrap();
        let (board, entrances) = start(root.path(), synced, false);
        let creations = CHANNELS.map(|channel| {
            let (creation, _) = board.create_channel(channel).unwrap();
            for id in ["first", "second", "third"] {
                board
                    .lead()
                    .send(SendRequest::note(channel, id, id.as_bytes()))
                    .unwrap();
            }
            creation
        });
        board.stop(board.status(None, None).run).unwrap();
        drop((board, entrances));
        if has_tail {
            kill(root.path(), synced, "repair-tail", "head-before");
        }
        let original = {
            let (store, generation) = claim(root.path(), synced);
            CHANNELS
                .into_iter()
                .zip(creations)
                .zip(kept)
                .map(|((channel, creation), kept)| {
                    let name = store_name(&channel.parse().unwrap(), creation);
                    let records = read(&store, &name);
                    assert_eq!(
                        store
                            .trim(generation, &name, Trim::Keep(1..kept + 1))
                            .unwrap(),
                        Trimmed::Done
                    );
                    if has_tail && channel == "alpha" {
                        assert_eq!(records.len(), 4);
                        let pending = records[3].clone();
                        let mut moved = pending.clone();
                        moved.position = 6.try_into().unwrap();
                        store.append(generation, &name, &[pending, moved]).unwrap();
                    } else {
                        assert_eq!(records.len(), 3);
                    }
                    records
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap()
        };
        let (opened, entrances) = start(root.path(), synced, false);
        for name in CHANNELS {
            assert_eq!(
                opened.lead().get(name, 2).unwrap_err().outcome(),
                Outcome::Unavailable
            );
        }
        opened.stop(opened.status(None, None).run).unwrap();
        drop((opened, entrances));

        let number = look(root.path(), synced, 64).unwrap().plan_number();
        std::fs::write(root.path().join("plan"), &number).unwrap();
        Self {
            root,
            creations,
            original,
            number,
            synced,
        }
    }

    fn kill(&self, checkpoint: &str) {
        kill(self.root.path(), self.synced, "repair", checkpoint);
    }

    fn apply(&self) -> crate::RecoveryReport {
        apply(self.root.path(), self.synced, &self.number).unwrap()
    }

    fn journal_head(&self) -> malak_journal::Checkpoint {
        malak_journal::StateFolder::acquire_existing(self.root.path().join("state"))
            .unwrap()
            .inspect()
            .unwrap()
            .loaded()
            .head
    }

    fn restart(&self) -> (Owner, crate::LocalEntrances) {
        start(self.root.path(), self.synced, false)
    }

    fn restore_tail(&self, index: usize) {
        let (store, generation) = claim(self.root.path(), self.synced);
        let name = self.store_name(index);
        store
            .append(generation, &name, &self.original[index][1..])
            .unwrap();
    }

    fn store_name(&self, index: usize) -> String {
        store_name(&CHANNELS[index].parse().unwrap(), self.creations[index])
    }
}

#[test]
fn a_finished_repair_adopts_exactly_the_approved_head_records() {
    let fixture = Board::with_unreported_head();
    let preview = look(fixture.root.path(), fixture.synced, 64)
        .unwrap()
        .to_string();
    assert!(preview.contains("Adopt [4..5)"), "{preview}");

    fixture.apply();

    let (store, _) = claim(fixture.root.path(), fixture.synced);
    let physical = read(&store, &fixture.store_name(0));
    assert_eq!(
        physical
            .iter()
            .map(|item| item.position.get())
            .collect::<Vec<_>>(),
        [1, 4]
    );
    drop(store);
    let (board, _entrances) = fixture.restart();
    let lead = board.lead();
    let receipt = lead.send(pending_send()).unwrap();
    assert!(receipt.duplicate);
    assert_eq!(receipt.position.get(), 4);
    let GetReply::Message(message) = lead.get("alpha", 4).unwrap() else {
        panic!("expected adopted head message");
    };
    assert_eq!(message.body.as_ref(), b"unreported");
    assert_eq!(
        lead.send(SendRequest::note("alpha", "next", b"next"))
            .unwrap()
            .position
            .get(),
        5
    );
}

#[test]
fn withdrawing_a_repair_preserves_adoptability_only_before_begin() {
    for begun in [false, true] {
        let fixture = Board::with_unreported_head();
        let root = fixture.root.path();
        let checkpoint = if begun {
            format!("repair-{}-rewrite-before", fixture.creations[0])
        } else {
            "repair-accepted".to_owned()
        };
        fixture.kill(&checkpoint);

        let withdrawn = Owner::withdraw_recovery(
            Limits::default(),
            root.join("state"),
            Some("key"),
            &fixture.number,
        )
        .unwrap();

        assert!(withdrawn.to_string().contains("withdrawn"));
        let upcoming = look(root, fixture.synced, 64).unwrap().to_string();
        assert_eq!(upcoming.contains("Adopt [4..5)"), !begun, "{upcoming}");
        let (store, _) = claim(root, fixture.synced);
        let entries = read(&store, &fixture.store_name(0));
        assert_eq!(
            entries
                .iter()
                .map(|item| item.position.get())
                .collect::<Vec<_>>(),
            [1, 4, 6]
        );
    }
}

#[test]
fn a_killed_repair_resumes_with_the_raised_bound_it_needed_to_be_accepted() {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    let mut fixture = Board::damaged(true, [1, 1]);
    let root = fixture.root.path();
    let channel = fs::read_dir(root.join("store"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|file| {
            file.extension().is_some_and(|ext| ext == "md")
                && fs::read(file)
                    .unwrap()
                    .windows(7)
                    .any(|body| body == b"\nfirst\n")
        })
        .unwrap();
    let padding = vec![b'x'; 8_192];
    let mut view_length = padding.len();
    // Every false candidate's view ends at the same footer and includes all
    // later starts. Their combined lengths exceed 64 times the channel size.
    let prefixes = (100..240)
        .rev()
        .map(|position| {
            let prefix = format!("<!-- malak 1 {position} 0 {view_length}  -->\n");
            view_length += prefix.len();
            prefix
        })
        .collect::<Vec<_>>();
    let mut file = OpenOptions::new().append(true).open(channel).unwrap();
    for prefix in prefixes.iter().rev() {
        file.write_all(prefix.as_bytes()).unwrap();
    }
    file.write_all(&padding).unwrap();
    file.write_all(b"\n\n").unwrap();
    drop(file);
    let rejected = look(root, fixture.synced, 64).err().unwrap();
    assert!(rejected.to_string().contains("work bound exceeded"));
    fixture.number = look(root, fixture.synced, 256).unwrap().plan_number();
    fs::write(root.join("plan"), &fixture.number).unwrap();

    fixture.kill("repair-accepted");
    let report = fixture.apply().to_string();

    assert_eq!(
        report
            .lines()
            .filter(|line| line.ends_with("finished"))
            .count(),
        2
    );
}

#[test]
fn every_accepted_repair_checkpoint_resumes_all_channels() {
    let checkpoints = STEPS
        .iter()
        .flat_map(|step| [(Some(0), *step), (Some(1), *step)])
        .chain([(None, "accepted")]);
    for (index, step) in checkpoints {
        for synced in [false, true] {
            let fixture = Board::damaged(synced, [1, 1]);
            let checkpoint = index.map_or_else(
                || "repair-accepted".to_owned(),
                |index| format!("repair-{}-{step}", fixture.creations[index]),
            );

            fixture.kill(&checkpoint);
            if synced {
                let result = apply(fixture.root.path(), synced, &fixture.number);
                if index == Some(1) && step == "finished" {
                    // The original plan has already finished every channel.
                    assert_eq!(result.err().unwrap().outcome(), Outcome::Refused);
                } else {
                    let report = result.unwrap().to_string();
                    assert_eq!(
                        report
                            .lines()
                            .filter(|line| line.ends_with("finished"))
                            .count(),
                        2
                    );
                }
            }
            let (board, _entrances) = fixture.restart();

            for channel in CHANNELS {
                let lead = board.lead();
                let GetReply::Message(kept) = lead.get(channel, 1).unwrap() else {
                    panic!("lost retained record for {checkpoint}");
                };
                assert_eq!(kept.body.as_ref(), b"first", "{checkpoint}");
                for void in [2, 3] {
                    assert!(
                        matches!(lead.get(channel, void).unwrap(), GetReply::Void { .. }),
                        "{checkpoint} {channel} {void}"
                    );
                }
            }
            assert!(board.status(None, None).pending_repair.is_none());
        }
    }
}

#[test]
fn a_record_restored_to_a_pending_channel_withdraws_only_that_rewrite() {
    let fixture = Board::damaged(true, [1, 1]);
    fixture.kill("repair-accepted");
    fixture.restore_tail(0);

    let report = fixture.apply().to_string();

    assert!(
        report
            .lines()
            .any(|line| line.starts_with("alpha ") && line.ends_with("withdrawn"))
    );
    assert!(
        report
            .lines()
            .any(|line| line.starts_with("beta ") && line.ends_with("finished"))
    );
    let (store, _) = claim(fixture.root.path(), fixture.synced);
    let restored = read(&store, &fixture.store_name(0));
    assert_eq!(restored.len(), fixture.original[0].len());
    for (after, before) in restored.iter().zip(&fixture.original[0]) {
        assert_eq!(after.position, before.position);
        assert_eq!(after.data, before.data);
    }
    drop(store);
    let (opened, _entrances) = fixture.restart();
    assert_eq!(
        opened.lead().get("alpha", 2).unwrap_err().outcome(),
        Outcome::Unavailable
    );
}

#[test]
fn a_begun_local_repair_requires_confirmation_and_the_original_folder() {
    use std::fs;

    let board = Board::damaged(true, [1, 1]);
    let root = board.root.path();
    let folder = root.join("store");
    let state = root.join("state");
    let stock = || MarkdownStore::new(&folder, true);
    let looked = Owner::look_recovery(stock(), Limits::default(), &state, Some("key"), 64).unwrap();
    let number = looked.plan_number();
    fs::write(root.join("plan"), &number).unwrap();
    let step = format!("repair-{}-begun", board.creations[0]);
    kill(root, true, "repair-owner-confirmed", &step);

    let before = board.journal_head();
    let error = Owner::apply_recovery(stock(), Limits::default(), &state, Some("key"), &number)
        .err()
        .unwrap();
    assert!(error.to_string().contains("--owner-vouches-same-volume"));
    assert_eq!(board.journal_head(), before);

    let (owner, entrances) = start_with_store(root, stock());
    assert_eq!(
        owner.status(None, None).pending_repair.as_deref(),
        Some(number.as_str())
    );
    assert_eq!(
        owner.lead().get("alpha", 1).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    owner.stop(owner.status(None, None).run).unwrap();
    drop((owner, entrances));

    let original = folder.with_extension("original");
    fs::rename(&folder, &original).unwrap();
    fs::create_dir(&folder).unwrap();
    let mut saved = Vec::new();
    for entry in fs::read_dir(&original).unwrap() {
        let entry = entry.unwrap();
        let path = folder.join(entry.file_name());
        let bytes = fs::read(entry.path()).unwrap();
        fs::write(&path, &bytes).unwrap();
        if path.extension().is_some_and(|extension| extension == "md") {
            saved.push((path, bytes));
        }
    }
    let before = board.journal_head();

    let error = apply_with_confirmation(root, true, &number).err().unwrap();
    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(
        error
            .to_string()
            .contains("filesystem ID or folder number has changed")
    );
    assert_eq!(board.journal_head(), before);
    for (path, contents) in saved {
        assert_eq!(fs::read(path).unwrap(), contents);
    }

    fs::rename(&folder, folder.with_extension("copied")).unwrap();
    fs::rename(&original, &folder).unwrap();
    let repaired = apply_with_confirmation(root, true, &number).unwrap();
    assert!(repaired.to_string().contains("finished"));
}

#[test]
fn a_different_store_id_refuses_a_begun_repair_with_an_empty_keep_list() {
    use std::fs;

    let board = Board::damaged(true, [0, 1]);
    board.kill(&format!("repair-{}-begun", board.creations[0]));
    let checkpoint = board.journal_head();
    let root = board.root.path();
    let previous = root.join("original-store");
    let current = root.join("store");
    fs::rename(&current, &previous).unwrap();
    drop(claim(root, true));
    let mut records = Vec::new();
    for entry in fs::read_dir(&previous).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            let bytes = fs::read(path).unwrap();
            let path = current.join(entry.file_name());
            fs::write(&path, &bytes).unwrap();
            records.push((path, bytes));
        }
    }

    let error = apply(root, true, &board.number).err().unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(
        error
            .to_string()
            .contains("saved board names a different store"),
        "{error}"
    );
    assert_eq!(board.journal_head(), checkpoint);
    for (path, original) in records {
        assert_eq!(fs::read(path).unwrap(), original);
    }
    assert!(fs::read_dir(&current).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".repair-")
    }));
}
