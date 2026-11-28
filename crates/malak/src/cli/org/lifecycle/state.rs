//! Owner-only publication memory shared with the retention recipe.

use crate::cli::config::io_error;
use malak::Error;
use malak_fs::{Directory, check_user_permissions};
use nix::{fcntl::OFlag, unistd::geteuid};
use serde_json::{Value, json};
use std::{fs::File, fs::TryLockError, io, path::Path};

const FILE: &str = "rail-published.json";
const STAGED: &str = "rail-published.tmp";

pub(super) enum Open {
    Ready(State),
    Busy,
    Paused(&'static str),
}

pub(super) struct State {
    folder: Directory,
    _lock: File,
    links: Vec<Value>,
}

impl State {
    pub(super) fn open(state: &Path, rail_floor: u64) -> Result<Open, Error> {
        let root = Directory::open(state).map_err(io_error)?;
        let (folder, created) = root.child_or_create("org".as_ref()).map_err(io_error)?;
        check_user_permissions(folder.file(), geteuid().as_raw(), true).map_err(io_error)?;
        if created {
            root.sync().map_err(io_error)?;
        }
        let lock = folder
            .open_file("rail-published.lock", OFlag::O_RDWR | OFlag::O_CREAT)
            .map_err(io_error)?;
        check_user_permissions(&lock, geteuid().as_raw(), true).map_err(io_error)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(Open::Busy),
            Err(TryLockError::Error(error)) => return Err(io_error(error)),
        }
        let mut fresh = false;
        let links = match folder.open_file(FILE, OFlag::O_RDONLY) {
            Ok(file) => {
                if check_user_permissions(&file, geteuid().as_raw(), true).is_err() {
                    return Ok(Open::Paused("publication history is unreadable"));
                }
                match serde_json::from_reader::<_, Vec<Value>>(file) {
                    Ok(links) if links.iter().all(valid) => links,
                    _ => return Ok(Open::Paused("publication history is unreadable")),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && rail_floor <= 1 => {
                fresh = true;
                Vec::new()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Open::Paused(
                    "publication history is missing after a rail trim",
                ));
            }
            Err(_) => return Ok(Open::Paused("publication history is unreadable")),
        };
        let state = Self {
            folder,
            _lock: lock,
            links,
        };
        if fresh {
            state.save()?;
        }
        Ok(Open::Ready(state))
    }

    pub(super) fn contains(&self, event: &str, link: &Value) -> bool {
        self.links.contains(&key(event, link))
    }

    pub(super) fn remember(&mut self, event: &str, link: &Value) -> bool {
        let key = key(event, link);
        if !valid(&key) || self.links.contains(&key) {
            return false;
        }
        self.links.push(key);
        true
    }

    pub(super) fn save(&self) -> Result<(), Error> {
        let mut staged = self
            .folder
            .open_file(STAGED, OFlag::O_WRONLY | OFlag::O_CREAT)
            .map_err(io_error)?;
        check_user_permissions(&staged, geteuid().as_raw(), true).map_err(io_error)?;
        staged.set_len(0).map_err(io_error)?;
        serde_json::to_writer(&mut staged, &self.links).map_err(|error| io_error(error.into()))?;
        malak_fs::sync(&staged).map_err(io_error)?;
        self.folder.rename(STAGED, FILE).map_err(io_error)?;
        self.folder.sync().map_err(io_error)
    }
}

fn key(event: &str, link: &Value) -> Value {
    json!([event, link[0], link[1], link[2], link[3]])
}

fn valid(link: &Value) -> bool {
    link.as_array().is_some_and(|link| link.len() == 5)
        && matches!(link[0].as_str(), Some("started" | "finished"))
        && (1..4).all(|index| link[index].is_string())
        && link[4].as_u64().is_some_and(|n| n > 0)
}
