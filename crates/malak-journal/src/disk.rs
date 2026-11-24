use super::{Checkpoint, Cursor, Error, Loaded, Replay, StateFolder};
use malak_fs::sync;
use nix::fcntl::OFlag;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};

mod archive;
mod snapshot;
use snapshot::Snapshot;

const PREFIX: &str = "entries-";
const APPEND: OFlag = OFlag::O_RDWR.union(OFlag::O_APPEND);
const CREATE: OFlag = OFlag::O_CREAT.union(OFlag::O_EXCL);

/// The durability required of one journal save.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Save {
    /// A whole write must reach the filesystem before returning.
    Written,
    /// The entry and the necessary directory entries must reach stable storage.
    OnDisk,
}

/// Why an active journal could not save.
#[derive(Debug)]
pub enum DiskError {
    /// The change could not be encoded.
    Format(Error),
    /// A write failed. A clean write boundary was restored if the journal is writable.
    Write(io::Error),
    /// A sync failed. The journal must be reopened before any further save.
    Sync(io::Error),
    /// A sync or failed cleanup prevented further writes on this journal.
    Stopped,
}

impl fmt::Display for DiskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(error) => error.fmt(f),
            Self::Write(error) => write!(f, "journal write failed: {error}"),
            Self::Sync(error) => write!(f, "journal sync failed: {error}"),
            Self::Stopped => f.write_str("journal has stopped writes"),
        }
    }
}

impl std::error::Error for DiskError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Format(error) => Some(error),
            Self::Write(error) | Self::Sync(error) => Some(error),
            Self::Stopped => None,
        }
    }
}

/// The accepted state of a locked journal, before any torn tail is removed.
pub struct InspectedJournal {
    folder: StateFolder,
    snapshot: Option<Snapshot>,
    loaded: Loaded,
    files: Vec<u64>,
    entries: Vec<(Checkpoint, Vec<u8>)>,
}

impl StateFolder {
    /// Recognize a journal snapshot or numbered entry file in the state folder.
    pub fn manages_file(name: &str) -> bool {
        matches!(name, snapshot::FILE | snapshot::PENDING) || entry_number(name).is_some()
    }

    /// Examine the snapshot and entry files without saving or cutting anything.
    ///
    /// The result retains the directory lock. Approve the state against the
    /// store before activating it for writes.
    pub fn inspect(self) -> io::Result<InspectedJournal> {
        let snapshot = match read_file(&self, snapshot::FILE) {
            Ok(bytes) => Some(Snapshot::decode(bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let base = snapshot
            .as_ref()
            .map_or(Checkpoint::EMPTY, |value| value.checkpoint);
        let files: Vec<_> = entry_files(&self)?
            .into_iter()
            .filter(|&number| number > base.sequence)
            .collect();

        let mut replay = Replay::new(base);
        let mut entries = Vec::new();
        for &number in &files {
            let bytes = read_file(&self, &entry_name(number))?;
            replay
                .file(number, &bytes, |at, payload| {
                    entries.push((at, payload.to_vec()))
                })
                .map_err(invalid)?;
        }
        let loaded = replay.finish().map_err(invalid)?;
        Ok(InspectedJournal {
            folder: self,
            snapshot,
            loaded,
            files,
            entries,
        })
    }
}

impl InspectedJournal {
    /// Return the snapshot payload and its covered checkpoint, if one exists.
    pub fn snapshot(&self) -> Option<(Checkpoint, &[u8])> {
        self.snapshot
            .as_ref()
            .map(|snapshot| (snapshot.checkpoint, snapshot.payload()))
    }

    /// Iterate accepted entries above the snapshot, in sequence order.
    pub fn entries(&self) -> impl Iterator<Item = (Checkpoint, &[u8])> {
        self.entries
            .iter()
            .map(|(at, bytes)| (*at, bytes.as_slice()))
    }

    /// Read accepted entries, including those inside a replay archive snapshot.
    ///
    /// An opaque snapshot written by a different application format cannot be
    /// read as an archive.
    pub fn accepted_entries(&self) -> io::Result<impl Iterator<Item = (Checkpoint, &[u8])>> {
        let archived = self
            .snapshot()
            .map(|(at, bytes)| archive::decode(at, bytes))
            .transpose()?
            .unwrap_or_default();
        Ok(archived.into_iter().chain(self.entries()))
    }

    /// Encode the accepted entries as a snapshot replay archive.
    ///
    /// The archive preserves the running hashes for witness comparisons.
    /// Pass it to the writer at the same checkpoint, before appending again.
    pub fn archive_payload(&self) -> io::Result<Vec<u8>> {
        archive::encode(self.accepted_entries()?)
    }

    /// Return the accepted prefix and the pending cut, if any.
    pub const fn loaded(&self) -> Loaded {
        self.loaded
    }

    /// Cut a torn tail and sync the accepted files before allowing further saves.
    ///
    /// Call only after the board has accepted the state against the store.
    pub fn activate(self) -> io::Result<Journal> {
        let Self {
            mut folder,
            loaded,
            files,
            ..
        } = self;
        folder.settle_parent()?;
        let mut current = None;
        for first in files.into_iter().rev() {
            let name = entry_name(first);
            if loaded.tail.is_some_and(|tail| first > tail.file) {
                folder.remove(&name)?;
                continue;
            }
            let file = folder.open_file(&name, APPEND)?;
            let len = if let Some(tail) = loaded.tail.filter(|tail| tail.file == first) {
                let offset = tail.offset as u64;
                file.set_len(offset)?;
                #[cfg(test)]
                crate::kill::hit("tail-truncated");
                offset
            } else {
                file.metadata()?.len()
            };
            if len == 0 && first > loaded.head.sequence.saturating_add(1) {
                drop(file);
                folder.remove(&name)?;
                continue;
            }
            sync(&file)?;
            #[cfg(test)]
            crate::kill::hit("entry-synced-on-activation");
            if current.is_none() {
                current = Some(EntryFile {
                    first,
                    file,
                    len,
                    sync_name: false,
                });
            }
        }
        folder.sync()?;
        #[cfg(test)]
        crate::kill::hit("directory-synced-on-activation");
        Ok(Journal {
            folder,
            cursor: Cursor::from_synced(loaded.head),
            current,
            writable: true,
        })
    }
}

/// The writer for a board's locked journal.
///
/// Serialise access with the board's journal lock. Dropping the writer releases
/// custody without promoting unsynced entries to stable storage.
pub struct Journal {
    folder: StateFolder,
    cursor: Cursor,
    current: Option<EntryFile>,
    writable: bool,
}

struct EntryFile {
    first: u64,
    file: File,
    len: u64,
    sync_name: bool,
}

impl Journal {
    /// Inspect the current journal again while retaining the state folder lock.
    pub fn inspect(self) -> io::Result<InspectedJournal> {
        drop(self.current);
        self.folder.inspect()
    }

    /// Return the last wholly written checkpoint, regardless of sync status.
    pub const fn head(&self) -> Checkpoint {
        self.cursor.head()
    }

    /// Return the last sequence covered by a successful sync.
    pub const fn synced_through(&self) -> u64 {
        self.cursor.synced_through()
    }

    /// Indicate whether a failed entry write can be retried on this writer.
    pub const fn writable(&self) -> bool {
        self.writable
    }

    /// Write one opaque change and reach its requested durability.
    pub fn append(&mut self, payload: &[u8], save: Save) -> Result<Checkpoint, DiskError> {
        self.check_writable()?;
        let entry = self.cursor.prepare(payload).map_err(DiskError::Format)?;
        if self.current.is_none() {
            self.current = Some(new_entry_file(&self.folder, entry.checkpoint().sequence)?);
        }
        let current = self.current.as_mut().unwrap();
        let bytes = entry.bytes();
        let next_len = current
            .len
            .checked_add(bytes.len() as u64)
            .ok_or(DiskError::Format(Error::EntryTooLarge))?;
        #[cfg(test)]
        crate::kill::tear("append-torn", &mut current.file, bytes).map_err(DiskError::Write)?;
        if let Err(error) = current.file.write_all(bytes) {
            if current.file.set_len(current.len).is_err() {
                self.writable = false;
            }
            return Err(DiskError::Write(error));
        }
        current.len = next_len;
        let written = entry.written();
        if save == Save::OnDisk {
            self.flush()?;
        }
        Ok(written)
    }

    /// Replace the snapshot with the complete state at the current head.
    ///
    /// This payload must include the board's witness bookkeeping. Older files
    /// remain until the new snapshot's name is durable.
    pub fn compact(&mut self, payload: &[u8]) -> Result<(), DiskError> {
        self.check_writable()?;
        self.flush()?;
        #[cfg(test)]
        crate::kill::hit("covered-files-synced");
        let at = self.cursor.head();
        let next = at
            .sequence
            .checked_add(1)
            .ok_or(DiskError::Format(Error::SequenceExhausted))?;
        if self.current.as_ref().is_none_or(|file| file.first != next) {
            self.current = Some(new_entry_file(&self.folder, next)?);
            #[cfg(test)]
            crate::kill::hit("successor-opened");
        }

        match self.folder.remove(snapshot::PENDING) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(DiskError::Write(error)),
        }
        let mut file = self
            .folder
            .open_file(snapshot::PENDING, OFlag::O_WRONLY | CREATE)
            .map_err(DiskError::Write)?;
        let snapshot = snapshot::encode(at, payload);
        #[cfg(test)]
        {
            crate::kill::hit("snapshot-temporary-opened");
            crate::kill::tear("snapshot-temporary-torn", &mut file, &snapshot)
                .map_err(DiskError::Write)?;
        }
        file.write_all(&snapshot).map_err(DiskError::Write)?;
        #[cfg(test)]
        crate::kill::hit("snapshot-temporary-written");
        self.require_sync(sync(&file))?;
        #[cfg(test)]
        crate::kill::hit("snapshot-temporary-synced");
        self.folder
            .rename(snapshot::PENDING, snapshot::FILE)
            .map_err(DiskError::Write)?;
        #[cfg(test)]
        crate::kill::hit("snapshot-renamed");
        self.require_sync(self.folder.sync())?;
        #[cfg(test)]
        crate::kill::hit("snapshot-published");

        let files = entry_files(&self.folder).map_err(DiskError::Write)?;
        for number in files
            .into_iter()
            .take_while(|&number| number <= at.sequence)
        {
            self.folder
                .remove(&entry_name(number))
                .map_err(DiskError::Write)?;
            #[cfg(test)]
            crate::kill::hit("covered-entry-removed");
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DiskError> {
        if let Some(current) = &self.current {
            self.require_sync(sync(&current.file))?;
        }
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.sync_name)
        {
            self.require_sync(self.folder.sync())?;
            self.current.as_mut().unwrap().sync_name = false;
        }
        self.cursor
            .synced(self.cursor.head().sequence)
            .map_err(DiskError::Format)
    }

    fn require_sync(&mut self, result: io::Result<()>) -> Result<(), DiskError> {
        result.map_err(|error| {
            self.writable = false;
            DiskError::Sync(error)
        })
    }

    fn check_writable(&self) -> Result<(), DiskError> {
        if self.writable {
            Ok(())
        } else {
            Err(DiskError::Stopped)
        }
    }
}

fn invalid(error: Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn new_entry_file(folder: &StateFolder, first: u64) -> Result<EntryFile, DiskError> {
    let file = folder
        .open_file(&entry_name(first), APPEND | CREATE)
        .map_err(DiskError::Write)?;
    Ok(EntryFile {
        first,
        file,
        len: 0,
        sync_name: true,
    })
}

fn read_file(folder: &StateFolder, name: &str) -> io::Result<Vec<u8>> {
    let mut file = folder.open_file(name, OFlag::O_RDONLY)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn entry_name(sequence: u64) -> String {
    format!("{PREFIX}{sequence:020}")
}

fn entry_number(name: &str) -> Option<u64> {
    let number = name.strip_prefix(PREFIX)?.parse().ok()?;
    (number > 0 && entry_name(number) == name).then_some(number)
}

fn entry_files(folder: &StateFolder) -> io::Result<Vec<u64>> {
    let mut files = Vec::new();
    for entry in folder.files()?.iter() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name.starts_with(PREFIX.as_bytes()) {
            let number = std::str::from_utf8(name)
                .ok()
                .and_then(entry_number)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid numbered journal file")
                })?;
            files.push(number);
        }
    }
    files.sort_unstable();
    Ok(files)
}
