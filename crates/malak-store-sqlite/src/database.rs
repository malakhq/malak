use malak_fs::{Directory, check_user_permissions, trusted_parent};
use malak_layers::{Error, Ownership};
use nix::fcntl::OFlag;
use nix::unistd::geteuid;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use std::fs::TryLockError;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub(super) const DATABASE: &str = "records.sqlite3";
pub(super) const SIDECARS: [&str; 3] = [
    "records.sqlite3-journal",
    "records.sqlite3-wal",
    "records.sqlite3-shm",
];
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS owner (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        store_id BLOB NOT NULL CHECK (length(store_id) = 16),
        generation INTEGER NOT NULL CHECK (generation > 0)
    ) STRICT;
    CREATE TABLE IF NOT EXISTS records (
        channel TEXT NOT NULL,
        position INTEGER NOT NULL CHECK (position > 0),
        data BLOB NOT NULL,
        PRIMARY KEY (channel, position)
    ) STRICT, WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS channel_end (
        channel TEXT PRIMARY KEY,
        last_position INTEGER NOT NULL CHECK (last_position > 0)
    ) STRICT, WITHOUT ROWID;";

pub(super) struct Writer {
    // Close SQLite before releasing the directory lock.
    pub(super) db: Connection,
    directory: Directory,
}

impl Writer {
    pub(super) fn take(path: &Path) -> Result<(Self, Ownership), Error> {
        let path = std::path::absolute(path).map_err(io_error)?;
        let uid = geteuid().as_raw();
        let (parent, name) = trusted_parent(&path, uid).map_err(io_error)?;
        let (directory, _) = parent.child_or_create(name).map_err(io_error)?;
        if directory.file().metadata().map_err(io_error)?.uid() != uid {
            return Err(Error::unavailable(
                "SQLite directory belongs to another account",
            ));
        }
        check_user_permissions(directory.file(), uid, true).map_err(io_error)?;
        match directory.file().try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::unavailable("SQLite store is already claimed"));
            }
            Err(TryLockError::Error(error)) => return Err(io_error(error)),
        }
        for sidecar in SIDECARS {
            match directory.open_file(sidecar, OFlag::O_RDONLY) {
                Ok(file) => check_user_permissions(&file, uid, true).map_err(io_error)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        let file = directory
            .open_file(DATABASE, OFlag::O_RDWR | OFlag::O_CREAT)
            .map_err(io_error)?;
        check_user_permissions(&file, uid, true).map_err(io_error)?;
        malak_fs::sync(&file).map_err(io_error)?;
        drop(file);
        directory.sync().map_err(io_error)?;
        parent.sync().map_err(io_error)?;

        let db = Connection::open_with_flags(
            path.join(DATABASE),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(sqlite_error)?;
        // Configure flush strength before opening or recovering any journal.
        db.execute_batch(
            "PRAGMA fullfsync = ON;
             PRAGMA checkpoint_fullfsync = ON;
             PRAGMA synchronous = FULL;
             PRAGMA temp_store = MEMORY;",
        )
        .map_err(sqlite_error)?;
        let mode: String = db
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(sqlite_error)?;
        if mode != "wal" {
            return Err(Error::unavailable("SQLite could not activate WAL"));
        }
        db.execute_batch(SCHEMA).map_err(sqlite_error)?;
        let mut writer = Self { db, directory };
        writer.checkpoint()?;
        let ownership = writer.advance()?;
        writer.directory.sync().map_err(io_error)?;
        Ok((writer, ownership))
    }

    fn advance(&mut self) -> Result<Ownership, Error> {
        let transaction = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        let has_records: bool = transaction
            .query_row("SELECT EXISTS(SELECT 1 FROM records)", [], |row| row.get(0))
            .map_err(sqlite_error)?;
        let current: Option<([u8; 16], i64)> = transaction
            .query_row(
                "SELECT store_id, generation FROM owner WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?;
        let (store_id, generation) = match current {
            Some((id, generation)) => (
                id,
                generation
                    .checked_add(1)
                    .ok_or_else(|| Error::unavailable("SQLite writer generations exhausted"))?,
            ),
            None if has_records => {
                return Err(Error::unavailable("SQLite store identity is missing"));
            }
            None => {
                let mut id = [0; 16];
                getrandom::fill(&mut id)
                    .map_err(|_| Error::unavailable("could not draw a SQLite store ID"))?;
                (id, 1)
            }
        };
        transaction
            .execute(
                "INSERT INTO owner VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET generation = excluded.generation",
                params![&store_id[..], generation],
            )
            .map_err(sqlite_error)?;
        transaction.commit().map_err(sqlite_error)?;
        Ok(Ownership {
            store_id,
            generation: generation as u64,
            has_records,
            location: None,
        })
    }

    pub(super) fn check(&self, generation: u64) -> Result<(), Error> {
        check_generation(&self.db, generation)
    }

    pub(super) fn write(&mut self, generation: u64) -> Result<Transaction<'_>, Error> {
        let transaction = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_error)?;
        check_generation(&transaction, generation)?;
        Ok(transaction)
    }

    pub(super) fn checkpoint(&self) -> Result<(), Error> {
        let busy: bool = self
            .db
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| row.get(0))
            .map_err(sqlite_error)?;
        if busy {
            return Err(Error::unavailable("SQLite could not settle its WAL"));
        }
        Ok(())
    }
}

pub(super) fn active(writer: &mut Option<Writer>) -> Result<&mut Writer, Error> {
    writer
        .as_mut()
        .ok_or_else(|| Error::unavailable("SQLite store has no active claim"))
}

fn check_generation(db: &Connection, generation: u64) -> Result<(), Error> {
    let current: i64 = db
        .query_row(
            "SELECT generation FROM owner WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_error)?;
    if current as u64 != generation {
        return Err(Error::unavailable("SQLite writer is no longer current"));
    }
    Ok(())
}

pub(super) fn sqlite_error(error: rusqlite::Error) -> Error {
    Error::unavailable(format!("SQLite store: {error}"))
}

fn io_error(error: io::Error) -> Error {
    Error::unavailable(format!("SQLite store: {error}"))
}
