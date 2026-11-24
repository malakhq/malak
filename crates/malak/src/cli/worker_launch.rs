//! Observe the exclusive claims held by the stock worker launchers.

use super::config;
use malak::ParticipantId;
use malak_fs::{Directory, check_user_permissions};
use nix::{fcntl::OFlag, unistd::geteuid};
use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

pub(super) fn open(state: &Path) -> io::Result<Option<Directory>> {
    config::open_owned_directory(&state.join("launches"), true)
}

pub(super) fn live_claim(launches: &Directory, id: &ParticipantId) -> io::Result<bool> {
    let Some(file) = open_file(launches, &format!("{id}.lock"))? else {
        return Ok(false);
    };
    // LaunchRecord.claim holds the exclusive flock for the live recipe.
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

pub(super) fn state(launches: &Directory, id: &ParticipantId) -> io::Result<Option<&'static str>> {
    if open_file(launches, &format!("{id}.json"))?.is_none() {
        return Ok(None);
    }
    Ok(Some(if live_claim(launches, id)? {
        "running"
    } else {
        "exited"
    }))
}

fn open_file(launches: &Directory, name: &str) -> io::Result<Option<File>> {
    match launches.open_file(name, OFlag::O_RDONLY) {
        Ok(file) => {
            check_user_permissions(&file, geteuid().as_raw(), true)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
