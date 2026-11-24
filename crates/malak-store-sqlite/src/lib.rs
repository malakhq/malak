//! SQLite persistence for Malak on local Linux and macOS filesystems.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod database;

use database::{Writer, sqlite_error};
use malak_layers::{
    Claim, Claimed, Durability, Error, Position, ReadLimit, ReadResult, Record, Store, StoreInfo,
    Trim, Trimmed,
};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// A SQLite store with WAL and a durable commit for each operation.
///
/// The path names a dedicated private directory on a local filesystem.
/// Only one active claim may use a directory. Repair is currently unsupported.
pub struct SqliteStore {
    directory: PathBuf,
    writer: Mutex<Option<Writer>>,
}

impl SqliteStore {
    /// Recognize the database and its sidecar file names in the store directory.
    pub fn manages_file(name: &str) -> bool {
        name == database::DATABASE || database::SIDECARS.contains(&name)
    }

    /// Construct an unopened store. The directory is opened or created on claim.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            writer: Mutex::new(None),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Writer>>, Error> {
        self.writer
            .lock()
            .map_err(|_| Error::unavailable("SQLite store lock is poisoned"))
    }
}

impl Store for SqliteStore {
    fn describe(&self) -> StoreInfo {
        StoreInfo {
            durability: Durability::Synced,
            max_record_bytes: MAX_RECORD_BYTES,
            slow_writes: false,
        }
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        let mut active = self.lock()?;
        match mode {
            Claim::Take => {
                if active.is_some() {
                    return Err(Error::unavailable("SQLite store is already claimed"));
                }
                let (writer, ownership) = Writer::take(&self.directory)?;
                *active = Some(writer);
                Ok(Claimed::Taken(ownership))
            }
            Claim::Repair => Err(Error::not_supported("repair is unsupported for SQLite")),
            Claim::Settle { generation, .. } => {
                let writer = database::active(&mut active)?;
                writer.check(generation)?;
                writer.checkpoint()?;
                Ok(Claimed::Done)
            }
            Claim::Release { generation } => {
                let writer = database::active(&mut active)?;
                writer.check(generation)?;
                *active = None;
                Ok(Claimed::Done)
            }
        }
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        if records
            .iter()
            .any(|record| record.data.len() > MAX_RECORD_BYTES)
            || records
                .windows(2)
                .any(|pair| pair[0].position >= pair[1].position)
        {
            return Err(Error::unavailable("invalid SQLite record batch"));
        }
        let mut active = self.lock()?;
        let writer = database::active(&mut active)?;
        let transaction = writer.write(generation)?;
        let Some(first) = records.first() else {
            return Ok(());
        };
        let last: Option<i64> = transaction
            .query_row(
                "SELECT last_position FROM channel_end WHERE channel = ?1",
                [channel],
                |row| row.get(0),
            )
            .optional()
            .map_err(sqlite_error)?;
        if last.is_some_and(|last| last >= number(first.position)) {
            return Err(Error::unavailable("record positions must increase"));
        }
        {
            let mut insert = transaction
                .prepare_cached("INSERT INTO records VALUES (?1, ?2, ?3)")
                .map_err(sqlite_error)?;
            for record in records {
                insert
                    .execute(params![channel, number(record.position), &record.data[..]])
                    .map_err(sqlite_error)?;
            }
        }
        let last = number(records.last().expect("nonempty batch").position);
        transaction
            .execute(
                "INSERT INTO channel_end VALUES (?1, ?2)
                 ON CONFLICT(channel) DO UPDATE SET last_position = excluded.last_position",
                params![channel, last],
            )
            .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        let mut active = self.lock()?;
        let writer = database::active(&mut active)?;
        let end = limit.before.map_or(i64::MAX, |before| number(before) - 1);
        let count = i64::try_from(limit.records).unwrap_or(i64::MAX);
        let mut query = writer
            .db
            .prepare_cached(
                "SELECT position, data FROM records
             WHERE channel = ?1 AND position BETWEEN ?2 AND ?3 ORDER BY position LIMIT ?4",
            )
            .map_err(sqlite_error)?;
        let mut rows = query
            .query(params![channel, number(from), end, count])
            .map_err(sqlite_error)?;
        let mut records = Vec::new();
        while let Some(row) = rows.next().map_err(sqlite_error)? {
            let position = row
                .get::<_, i64>(0)
                .ok()
                .and_then(|position| (position as u64).try_into().ok());
            let data = row.get_ref(1).ok().and_then(|value| value.as_blob().ok());
            let (Some(position), Some(data)) = (position, data) else {
                return Ok(ReadResult::Records {
                    records,
                    unparseable: true,
                });
            };
            if data.len() > MAX_RECORD_BYTES {
                return Ok(ReadResult::Records {
                    records,
                    unparseable: true,
                });
            }
            records.push(Record {
                position,
                data: data.to_vec().into(),
            });
        }
        Ok(ReadResult::Records {
            records,
            unparseable: false,
        })
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        let Trim::Keep(keep) = trim else {
            return Err(Error::not_supported("SQLite repair is not supported"));
        };
        if keep.start == 0 || keep.end == 0 || keep.start.max(keep.end) > Position::MAX + 1 {
            return Err(Error::unavailable("invalid SQLite trim bounds"));
        }

        let mut active = self.lock()?;
        let writer = database::active(&mut active)?;
        let transaction = writer.write(generation)?;
        let first = (keep.start - 1) as i64;
        let last = (keep.end - 1) as i64;
        transaction
            .execute(
                "DELETE FROM records WHERE channel = ?1 AND (position <= ?2 OR position > ?3)",
                params![channel, first, last],
            )
            .map_err(sqlite_error)?;
        if last == 0 {
            transaction.execute("DELETE FROM channel_end WHERE channel = ?1", [channel])
        } else {
            transaction.execute(
                "UPDATE channel_end SET last_position = ?2
                 WHERE channel = ?1 AND last_position > ?2",
                params![channel, last],
            )
        }
        .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)?;
        Ok(Trimmed::Done)
    }
}

fn number(position: Position) -> i64 {
    position.get() as i64
}

#[cfg(test)]
#[allow(unsafe_code)]
mod power_cut;
