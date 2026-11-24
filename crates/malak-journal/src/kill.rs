//! Process-kill fixtures for truncation, sync and snapshot publication.

use crate::{InspectedJournal, Journal, Save, StateFolder};
use malak_testkit::process::{kill_after_checkpoint, pause_at_checkpoint};
use malak_testkit::unix_socket::SocketDir;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const ROOT: &str = "MALAK_JOURNAL_KILL_ROOT";
const STEP: &str = "MALAK_JOURNAL_KILL_STEP";
const OPERATION: &str = "MALAK_JOURNAL_KILL_ACTION";

fn checkpoint(step: &str) -> Option<PathBuf> {
    static CHILD: OnceLock<Option<(PathBuf, String)>> = OnceLock::new();
    let (root, selected) = CHILD
        .get_or_init(|| Some((std::env::var_os(ROOT)?.into(), std::env::var(STEP).ok()?)))
        .as_ref()?;
    (selected == step).then(|| root.join(format!("checkpoint-{step}")))
}

pub(crate) fn hit(step: &str) {
    if let Some(path) = checkpoint(step) {
        pause_at_checkpoint(&path);
    }
}

pub(crate) fn tear(step: &str, file: &mut File, bytes: &[u8]) -> io::Result<()> {
    if let Some(path) = checkpoint(step) {
        file.write_all(&bytes[..bytes.len() / 2])?;
        pause_at_checkpoint(&path);
    }
    Ok(())
}

fn inspect(root: &Path) -> InspectedJournal {
    StateFolder::acquire(root.join("state"))
        .unwrap()
        .inspect()
        .unwrap()
}

fn writer(root: &Path) -> Journal {
    inspect(root).activate().unwrap()
}

fn crash(root: &Path, operation: &str, step: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "kill::journal_checkpoint_child"])
        .env(ROOT, root)
        .env(OPERATION, operation)
        .env(STEP, step);
    kill_after_checkpoint(&mut child, &root.join(format!("checkpoint-{step}")));
}

fn accept(root: &Path) -> Vec<(crate::Checkpoint, Vec<u8>)> {
    inspect(root)
        .accepted_entries()
        .unwrap()
        .map(|(at, bytes)| (at, bytes.to_vec()))
        .collect()
}

const COMPACT: &[&str] = &[
    "covered-files-synced",
    "successor-opened",
    "snapshot-temporary-opened",
    "snapshot-temporary-torn",
    "snapshot-temporary-written",
    "snapshot-temporary-synced",
    "snapshot-renamed",
    "snapshot-published",
    "covered-entry-removed",
];

#[test]
fn every_compaction_checkpoint_restarts_at_the_same_logical_state() {
    for step in COMPACT {
        let scratch = SocketDir::new().unwrap();
        let root = scratch.path();
        let mut journal = writer(root);
        let first = journal
            .append(b"first accepted change", Save::OnDisk)
            .unwrap();
        let second = journal
            .append(b"second accepted change", Save::OnDisk)
            .unwrap();
        drop(journal);
        let saved = accept(root);

        crash(root, "compact", step);

        assert_eq!(accept(root), saved, "{step}");
        crash(root, "activate", "directory-synced-on-activation");
        assert_eq!(accept(root), saved, "second restart at {step}");
        let mut journal = writer(root);
        let next = journal.append(b"later change", Save::OnDisk).unwrap();
        assert_eq!((first.sequence, second.sequence, next.sequence), (1, 2, 3));
    }
}

#[test]
fn a_torn_append_and_two_kills_keep_the_synced_prefix() {
    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    let mut journal = writer(root);
    let stable = journal.append(b"must survive", Save::OnDisk).unwrap();
    drop(journal);

    crash(root, "append", "append-torn");
    assert!(inspect(root).loaded().tail.is_some());
    crash(root, "activate", "tail-truncated");
    crash(root, "activate", "entry-synced-on-activation");

    let mut journal = writer(root);
    assert_eq!(journal.head(), stable);
    let next = journal.append(b"following change", Save::OnDisk).unwrap();
    assert_eq!(next.sequence, stable.sequence + 1);
}

#[test]
fn either_directory_image_before_snapshot_sync_preserves_the_prefix() {
    for snapshot_survives in [false, true] {
        let scratch = SocketDir::new().unwrap();
        let root = scratch.path();
        let mut journal = writer(root);
        journal
            .append(b"counted before the rename", Save::OnDisk)
            .unwrap();
        drop(journal);
        let saved = accept(root);

        crash(root, "compact", "snapshot-renamed");

        if !snapshot_survives {
            // Model the prior directory image if the new name did not survive.
            fs::remove_file(root.join("state/snapshot")).unwrap();
            fs::remove_file(root.join("state/entries-00000000000000000002")).unwrap();
        }
        assert_eq!(accept(root), saved);
    }
}

#[test]
fn journal_checkpoint_child() {
    let Some(root) = std::env::var_os(ROOT) else {
        return;
    };
    let root = Path::new(&root);
    match std::env::var(OPERATION).unwrap().as_str() {
        "compact" => {
            let inspected = inspect(root);
            let archive = inspected.archive_payload().unwrap();
            inspected.activate().unwrap().compact(&archive).unwrap();
        }
        "append" => {
            writer(root)
                .append(b"a change that will tear", Save::OnDisk)
                .unwrap();
        }
        "activate" => {
            drop(writer(root));
        }
        _ => panic!("unknown operation"),
    }
    panic!("did not reach {}", std::env::var(STEP).unwrap());
}
