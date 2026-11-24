//! The one-shot watcher's delivery cursors, private to this board and lead.

use super::super::model::{Address, Channel};
use crate::cli::config::io_error;
use malak::Error;
use malak_fs::Directory;
use nix::fcntl::OFlag;
use serde::{Deserialize, Serialize};
use std::{fs::File, io, path::Path};

const STATE: &str = "mailbox-next-lead.json";
const LOCK: &str = "mailbox-next-lead.lock";
const TEMP: &str = "mailbox-next-lead.tmp";
const HOUR_MS: u64 = 60 * 60 * 1_000;

pub(super) struct Store(Directory);

impl Store {
    pub fn open(path: &Path) -> Result<Self, Error> {
        Directory::open(path).map(Self).map_err(io_error)
    }

    pub fn update<T>(
        &self,
        operation: impl FnOnce(&mut Shown, bool) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let directory = &self.0;
        let lock = open_lock_file(directory).map_err(io_error)?;
        lock.lock().map_err(io_error)?;
        let previous = match directory.open_file(STATE, OFlag::O_RDONLY) {
            Ok(file) => Some(
                serde_json::from_reader(file)
                    .map_err(io::Error::from)
                    .map_err(io_error)?,
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(error)),
        };
        let mut shown: Shown = previous.clone().unwrap_or_default();
        let result = operation(&mut shown, previous.is_none());
        if previous.as_ref() != Some(&shown) {
            self.save(&shown).map_err(io_error)?;
        }
        result
    }

    fn save(&self, shown: &Shown) -> io::Result<()> {
        let staged = self.0.open_file(TEMP, OFlag::O_WRONLY | OFlag::O_CREAT)?;
        staged.set_len(0)?;
        serde_json::to_writer(&staged, shown)?;
        staged.sync_all()?;
        self.0.rename(TEMP, STATE)?;
        self.0.sync()
    }
}

fn open_lock_file(directory: &Directory) -> io::Result<File> {
    match directory.open_file(LOCK, OFlag::O_RDWR) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        result => return result,
    }

    // Concurrent nonexclusive creates can return ENOENT on macOS.
    match directory.open_file(LOCK, OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            directory.open_file(LOCK, OFlag::O_RDWR)
        }
        result => result,
    }
}

// Positions are ordered within a creation. The three channels need only one
// cursor each, regardless of how many records the board retains.
#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct Shown {
    lead: Cursor,
    questions: Cursor,
    orders: Cursor,
    reminder: Option<Reminder>,
}

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
struct Cursor {
    creation: String,
    through: u64,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Serialize)]
struct Reminder {
    at_ms: u64,
    through: u64,
}

impl Shown {
    fn cursor(&mut self, channel: &str, creation: &str) -> &mut Cursor {
        let cursor = match channel {
            "lead" => &mut self.lead,
            "questions" => &mut self.questions,
            "orders" => &mut self.orders,
            _ => unreachable!("only watched channels have delivery cursors"),
        };
        if cursor.creation != creation {
            *cursor = Cursor {
                creation: creation.to_owned(),
                through: 0,
            };
            if channel == "lead" {
                self.reminder = None;
            }
        }
        cursor
    }

    pub fn start(&mut self, channels: &[Channel]) {
        for channel in channels {
            if matches!(channel.name.as_str(), "questions" | "orders") {
                self.cursor(&channel.name, &channel.creation).through =
                    channel.head.saturating_sub(1);
            }
        }
    }

    pub fn through(&mut self, channel: &str, creation: &str) -> u64 {
        self.cursor(channel, creation).through
    }

    pub fn advance(&mut self, address: &Address) {
        let cursor = self.cursor(&address.channel, &address.creation);
        cursor.through = cursor.through.max(address.position);
    }

    pub fn reminder_due(&self, now: u64) -> Option<u64> {
        self.reminder
            .filter(|reminder| now.saturating_sub(reminder.at_ms) >= HOUR_MS)
            .map(|reminder| reminder.through)
    }

    pub fn reminded(&mut self, now: u64) {
        self.reminder = Some(Reminder {
            at_ms: now,
            through: self.lead.through,
        });
    }

    pub fn mail_shown(&mut self, address: &Address, now: u64) {
        self.advance(address);
        if self.reminder.is_none() {
            self.reminded(now);
        }
    }
}
