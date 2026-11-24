//! Journal replay and activation on real entry files.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::Scratch;
use malak_journal::{Checkpoint, InspectedJournal, Journal, Save, StateFolder};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

fn inspect(path: &Path) -> InspectedJournal {
    StateFolder::acquire(path).unwrap().inspect().unwrap()
}

fn writer(path: &Path) -> Journal {
    inspect(path).activate().unwrap()
}

fn entry_file(path: &Path, first: u64) -> PathBuf {
    path.join(format!("entries-{first:020}"))
}

fn tear(path: &Path) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"\0\x03x")
        .unwrap();
}

#[test]
fn an_inspected_tail_is_unchanged_until_start_is_approved() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut journal = writer(path);
    journal.append(b"first", Save::OnDisk).unwrap();
    drop(journal);
    let file = entry_file(path, 1);
    tear(&file);
    let found = fs::read(&file).unwrap();

    let loaded = inspect(path);
    assert!(loaded.loaded().tail.is_some());
    drop(loaded);

    assert_eq!(fs::read(file).unwrap(), found);
}

#[test]
fn a_torn_entry_is_cut_before_the_sequence_is_written_again() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut journal = writer(path);
    let first = journal.append(b"one", Save::OnDisk).unwrap();
    drop(journal);
    tear(&entry_file(path, 1));

    let mut journal = writer(path);
    let second = journal.append(b"two", Save::Written).unwrap();
    drop(journal);

    let loaded = inspect(path);
    assert_eq!(
        loaded.entries().collect::<Vec<_>>(),
        [(first, b"one".as_slice()), (second, b"two".as_slice())]
    );
    assert!(loaded.loaded().tail.is_none());
}

#[test]
fn opening_a_written_prefix_syncs_it_before_new_entries() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut journal = writer(path);
    let first = journal.append(b"survived", Save::Written).unwrap();
    assert_eq!(journal.synced_through(), Checkpoint::EMPTY.sequence);
    drop(journal);

    let journal = writer(path);

    assert_eq!(journal.synced_through(), first.sequence);
}

#[test]
fn a_later_entry_vouching_for_a_broken_entry_refuses_start() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut journal = writer(path);
    journal.append(b"one", Save::OnDisk).unwrap();
    journal.append(b"two", Save::Written).unwrap();
    drop(journal);
    let file = entry_file(path, 1);
    let mut contents = fs::read(&file).unwrap();
    contents[4] ^= 0x80;
    fs::write(&file, &contents).unwrap();

    let result = StateFolder::acquire(path).unwrap().inspect();

    assert_eq!(result.err().unwrap().kind(), ErrorKind::InvalidData);
    assert_eq!(fs::read(file).unwrap(), contents);
}

#[test]
fn a_snapshot_covers_older_entry_files_even_if_they_reappear() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut journal = writer(path);
    let at = journal.append(b"one", Save::Written).unwrap();
    let old = entry_file(path, 1);
    let before = fs::read(&old).unwrap();

    journal.compact(b"state through one").unwrap();
    assert!(!old.exists());
    let after = journal.append(b"two", Save::OnDisk).unwrap();
    drop(journal);
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&old)
        .unwrap()
        .write_all(&before)
        .unwrap();

    let loaded = inspect(path);
    assert_eq!(
        loaded.snapshot(),
        Some((at, b"state through one".as_slice()))
    );
    assert_eq!(
        loaded.entries().collect::<Vec<_>>(),
        [(after, b"two".as_slice())]
    );
}

#[test]
fn a_damaged_snapshot_prevents_start() {
    let scratch = Scratch::new();
    let path = scratch.path();
    let mut journal = writer(path);
    journal.append(b"before", Save::OnDisk).unwrap();
    journal.compact(b"state").unwrap();
    drop(journal);
    let snapshot = path.join("snapshot");
    let mut contents = fs::read(&snapshot).unwrap();
    contents[12] ^= 0x80;
    fs::write(snapshot, contents).unwrap();

    let result = StateFolder::acquire(path).unwrap().inspect();

    assert_eq!(result.err().unwrap().kind(), ErrorKind::InvalidData);
}

#[test]
fn a_renamed_state_folder_still_receives_its_own_entries() {
    let scratch = Scratch::new();
    let path = scratch.path().join("board");
    let mut journal = writer(&path);
    let moved = scratch.path().join("moved");
    fs::rename(&path, &moved).unwrap();
    drop(StateFolder::acquire(&path).unwrap());

    let checkpoint = journal.append(b"pinned", Save::OnDisk).unwrap();
    drop(journal);

    assert_eq!(inspect(&moved).loaded().head, checkpoint);
    assert_eq!(inspect(&path).loaded().head, Checkpoint::EMPTY);
}
