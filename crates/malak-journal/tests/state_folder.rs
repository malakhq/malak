//! Tests of state-folder custody on real local directories.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::Scratch;
use malak_journal::StateFolder;
use std::fs::{Permissions, create_dir, set_permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn the_state_folder_is_private_from_creation() {
    let scratch = Scratch::new();
    let path = scratch.path().join("board");

    let _state = StateFolder::acquire(&path).unwrap();

    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn an_existing_public_state_folder_is_refused_without_changing_it() {
    let scratch = Scratch::new();
    let path = scratch.path().join("board");
    create_dir(&path).unwrap();
    set_permissions(&path, Permissions::from_mode(0o755)).unwrap();

    let error = StateFolder::acquire(&path).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o755);
}

#[test]
fn a_state_folder_is_never_acquired_through_a_final_symlink() {
    let scratch = Scratch::new();
    let actual = scratch.path().join("actual");
    drop(StateFolder::acquire(&actual).unwrap());
    let link = scratch.path().join("link");
    symlink(actual, &link).unwrap();

    assert!(StateFolder::acquire(link.join("")).is_err());
}

#[test]
fn the_same_state_folder_cannot_be_held_twice_even_under_another_name() {
    let scratch = Scratch::new();
    let path = scratch.path().join("board");
    let first = StateFolder::acquire(&path).unwrap();
    let moved = scratch.path().join("moved");
    std::fs::rename(path, &moved).unwrap();

    assert_eq!(
        StateFolder::acquire(&moved).unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    drop(first);
    StateFolder::acquire(moved).unwrap();
}

#[cfg(target_os = "macos")]
#[test]
fn an_extended_acl_cannot_make_a_private_mode_folder_public() {
    let scratch = Scratch::new();
    let path = scratch.path().join("board");
    drop(StateFolder::acquire(&path).unwrap());
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow list,search"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());

    assert_eq!(
        StateFolder::acquire(path).unwrap_err().kind(),
        ErrorKind::PermissionDenied
    );
}
