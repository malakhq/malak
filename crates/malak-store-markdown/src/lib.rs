//! A durable store with one human-readable Markdown file per channel.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_fs::{Directory, sync};
use malak_layers::{
    Claim, Claimed, Durability, Error, Ownership, Position, ReadLimit, ReadResult, Record, Store,
    StoreInfo, StorePlace, Trim, Trimmed,
};
use nix::fcntl::OFlag;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

mod format;
mod owner;
mod place;
mod repair;
#[cfg(test)]
mod repair_kill;

const CREATE: OFlag = OFlag::O_CREAT.union(OFlag::O_EXCL);
const APPEND: OFlag = OFlag::O_RDWR.union(OFlag::O_APPEND);

/// The built-in persistent Markdown store.
pub struct MarkdownStore {
    path: PathBuf,
    sync_writes: bool,
    place_socket: Option<PathBuf>,
    active: RwLock<Option<Lease>>,
}

struct Lease {
    directory: Directory,
    owner: owner::Ownership,
    mode: Mode,
    channels: Mutex<HashMap<String, ChannelSlot>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Ordinary,
    Repair,
}

type ChannelSlot = Arc<Mutex<ChannelState>>;

#[derive(Default)]
struct ChannelState {
    identity: Option<Identity>,
    new_name: bool,
}

impl ChannelState {
    fn sync_name(&mut self, dir: &Directory) -> io::Result<()> {
        if self.new_name {
            dir.sync()?;
            self.new_name = false;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
    len: u64,
}

impl Identity {
    fn read(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
        })
    }
}

impl MarkdownStore {
    /// Recognize ownership, channel, and repair file names in the store directory.
    pub fn manages_file(name: &str) -> bool {
        let base = name.strip_suffix(".next").unwrap_or(name);
        name == owner::OWNER || format::channel_name(base).is_some() || repair::is_backup(base)
    }

    /// Configure a store directory and the durability of successful appends.
    ///
    /// The first claim opens the directory or creates it with mode 0700.
    /// Enable `sync_writes` to request a full flush on every append.
    pub fn new(path: impl Into<PathBuf>, sync_writes: bool) -> Self {
        Self {
            path: path.into(),
            sync_writes,
            place_socket: None,
            active: RwLock::new(None),
        }
    }

    /// Use a host service to attest to the store volume's continuity during repair.
    ///
    /// The service must retain its attachment mark outside the volume's rollback
    /// domain. Until the host can attest, repair claims remain unestablished.
    pub fn with_place_socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.place_socket = Some(socket.into());
        self
    }

    fn take(&self, mode: Mode) -> Result<Claimed, Error> {
        let mut active = self.active.write().map_err(poisoned)?;
        if active.is_some() {
            return Err(Error::unavailable("markdown store is already claimed"));
        }
        let (parent, directory, _) = Directory::open_or_create(&self.path).map_err(io_error)?;
        let mut owner = owner::Ownership::lock(&directory).map_err(io_error)?;
        let mut channels = HashMap::new();
        let mut has_records = false;
        let mut names = Vec::new();
        for entry in directory.entries().map_err(io_error)?.iter() {
            let entry = entry.map_err(|error| io_error(error.into()))?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if let Some(channel) = format::channel_name(name) {
                if mode == Mode::Repair {
                    names.push(channel);
                }
                let file = directory
                    .open_file(name, OFlag::O_RDONLY)
                    .map_err(io_error)?;
                let identity = Identity::read(&file).map_err(io_error)?;
                has_records |= identity.len > 0;
                sync(&file).map_err(io_error)?;
                channels.insert(
                    name.into(),
                    Arc::new(Mutex::new(ChannelState {
                        identity: Some(identity),
                        new_name: false,
                    })),
                );
            } else if let Some(base) = name.strip_suffix(".next")
                && format::channel_name(base).is_some()
            {
                let file = directory
                    .open_file(name, OFlag::O_RDONLY)
                    .map_err(io_error)?;
                sync(&file).map_err(io_error)?;
            }
        }

        parent.sync().map_err(io_error)?;
        let store_id = owner.advance(&directory, has_records).map_err(io_error)?;
        let generation = owner.generation;
        let location = place::location(directory.file()).ok();
        let place = match (mode, self.place_socket.as_deref()) {
            (Mode::Repair, Some(socket)) => {
                place::establish(directory.file(), socket).unwrap_or(StorePlace::NotEstablished)
            }
            (Mode::Repair, None) if location.is_some() => StorePlace::OwnerConfirmation,
            _ => StorePlace::NotEstablished,
        };
        *active = Some(Lease {
            directory,
            owner,
            mode,
            channels: Mutex::new(channels),
        });
        let ownership = Ownership {
            store_id,
            generation,
            has_records,
            location,
        };
        Ok(match mode {
            Mode::Ordinary => Claimed::Taken(ownership),
            Mode::Repair => {
                names.sort_unstable();
                Claimed::Repair {
                    ownership,
                    place,
                    channels: names,
                }
            }
        })
    }

    fn channel<T>(
        &self,
        name: &str,
        generation: Option<u64>,
        required: Option<Mode>,
        run: impl FnOnce(&Directory, &str, &mut ChannelState, Mode) -> io::Result<T>,
    ) -> Result<T, Error> {
        let active = self.active.read().map_err(poisoned)?;
        let lease = active
            .as_ref()
            .ok_or_else(|| Error::unavailable("markdown store is unclaimed"))?;
        lease
            .owner
            .matches(
                &lease.directory,
                generation.unwrap_or(lease.owner.generation),
            )
            .map_err(io_error)?;
        if required.is_some_and(|mode| lease.mode != mode) {
            return Err(Error::refused("store claim does not permit this change"));
        }
        let name =
            format::file_name(name).ok_or_else(|| Error::refused("invalid store channel name"))?;
        let slot = lease
            .channels
            .lock()
            .map_err(poisoned)?
            .entry(name.clone())
            .or_default()
            .clone();
        let mut channel = slot.lock().map_err(poisoned)?;
        run(&lease.directory, &name, &mut channel, lease.mode).map_err(io_error)
    }
}

impl Store for MarkdownStore {
    fn describe(&self) -> StoreInfo {
        StoreInfo {
            durability: if self.sync_writes {
                Durability::Synced
            } else {
                Durability::Restart
            },
            max_record_bytes: format::MAX_RECORD,
            slow_writes: true,
        }
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        match mode {
            Claim::Take => self.take(Mode::Ordinary),
            Claim::Repair => self.take(Mode::Repair),
            Claim::Settle {
                generation,
                channel,
            } => {
                self.channel(
                    channel,
                    Some(generation),
                    None,
                    |directory, name, state, _| {
                        if let Some(file) = existing(directory, name, OFlag::O_RDONLY)? {
                            sync(&file)?;
                        }
                        directory.sync()?;
                        state.new_name = false;
                        Ok(())
                    },
                )?;
                Ok(Claimed::Done)
            }
            Claim::Release { generation } => {
                let mut active = self.active.write().map_err(poisoned)?;
                if active
                    .as_ref()
                    .is_none_or(|lease| lease.owner.generation != generation)
                {
                    return Err(Error::unavailable(
                        "markdown store writer is no longer current",
                    ));
                }
                *active = None;
                Ok(Claimed::Done)
            }
        }
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        self.channel(
            channel,
            Some(generation),
            Some(Mode::Ordinary),
            |directory, name, state, _| {
                if records.is_empty() {
                    return Ok(());
                }
                if records
                    .iter()
                    .any(|record| record.data.len() > format::MAX_RECORD)
                    || records
                        .windows(2)
                        .any(|pair| pair[0].position >= pair[1].position)
                {
                    return Err(bad_append());
                }
                let prior = existing(directory, name, APPEND)?;
                let found = prior.as_ref().map(Identity::read).transpose()?;
                if found != state.identity {
                    return Err(io::Error::other(
                        "channel file changed since the store left it",
                    ));
                }
                let mut file = match prior {
                    Some(mut file) => {
                        let contents = read_all(&mut file)?;
                        let mut previous = format::Records::new(&contents);
                        while previous.next()?.is_some() {}
                        if previous
                            .last()
                            .is_some_and(|last| last >= records[0].position)
                        {
                            return Err(bad_append());
                        }
                        file
                    }
                    None => {
                        let file = directory.open_file(name, APPEND | CREATE)?;
                        state.new_name = true;
                        file
                    }
                };
                let mut batch = Vec::new();
                for record in records {
                    format::encode(record, &mut batch);
                }

                let written = file.write(&batch);
                state.identity = Some(Identity::read(&file)?);
                if written? != batch.len() {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                if self.sync_writes {
                    sync(&file)?;
                    state.sync_name(directory)?;
                }
                Ok(())
            },
        )
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        self.channel(channel, None, None, |directory, name, _, mode| {
            if mode == Mode::Repair {
                let bytes = match existing(directory, name, OFlag::O_RDONLY)? {
                    Some(mut file) => read_all(&mut file)?,
                    None => Vec::new(),
                };
                return Ok(repair::Scan::new(bytes).into_result());
            }
            let mut records = Vec::new();
            let mut unparseable = false;
            if let Some(mut file) = existing(directory, name, OFlag::O_RDONLY)? {
                let bytes = read_all(&mut file)?;
                let mut input = format::Records::new(&bytes);
                while records.len() < limit.records {
                    match input.next() {
                        Ok(Some((record, _))) => {
                            if limit.before.is_some_and(|before| record.position >= before) {
                                break;
                            }
                            if record.position >= from {
                                records.push(record);
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {
                            unparseable = true;
                            break;
                        }
                    }
                }
            }
            Ok(ReadResult::Records {
                records,
                unparseable,
            })
        })
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        let range = match trim {
            Trim::Keep(range) => range,
            Trim::Repair { plan, input, keep } => {
                return self.channel(
                    channel,
                    Some(generation),
                    Some(Mode::Repair),
                    |directory, name, state, _| {
                        repair::rewrite(directory, name, state, plan, input, keep)
                    },
                );
            }
        };
        self.channel(
            channel,
            Some(generation),
            Some(Mode::Ordinary),
            |directory, name, state, _| {
                let Some(mut file) = existing(directory, name, OFlag::O_RDWR)? else {
                    *state = ChannelState::default();
                    return Ok(Trimmed::Done);
                };
                let bytes = read_all(&mut file)?;
                let kept = retained(&bytes, range)?;
                if kept.start == 0 {
                    if kept.end != bytes.len() {
                        file.set_len(kept.end as u64)?;
                        state.identity = Some(Identity::read(&file)?);
                        if self.sync_writes {
                            sync(&file)?;
                        }
                    } else {
                        state.identity = Some(Identity::read(&file)?);
                    }
                } else {
                    let replacement = replace(directory, name, &bytes[kept])?;
                    state.identity = Some(Identity::read(&replacement)?);
                    state.new_name = true;
                    state.sync_name(directory)?;
                }
                Ok(Trimmed::Done)
            },
        )
    }
}

fn existing(directory: &Directory, name: &str, access: OFlag) -> io::Result<Option<File>> {
    match directory.open_file(name, access) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_all(file: &mut File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn retained(bytes: &[u8], keep: Range<u64>) -> io::Result<Range<usize>> {
    if keep.is_empty() {
        return Ok(0..0);
    }
    let mut input = format::Records::new(bytes);
    let mut kept: Option<Range<usize>> = None;
    loop {
        match input.next() {
            Ok(Some((record, range))) => {
                if record.position.get() >= keep.end {
                    break;
                }
                if keep.contains(&record.position.get()) {
                    let start = kept.as_ref().map_or(range.start, |current| current.start);
                    kept = Some(start..range.end);
                }
                if record.position.get() + 1 == keep.end {
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => return Err(error),
        }
    }
    Ok(kept.unwrap_or(0..0))
}

fn replace(directory: &Directory, name: &str, bytes: &[u8]) -> io::Result<File> {
    let pending = format!("{name}.next");
    match directory.remove(&pending) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
    }
    let mut file = directory.open_file(&pending, OFlag::O_WRONLY | CREATE)?;
    #[cfg(test)]
    repair_kill::torn(&mut file, bytes, "replacement-torn")?;
    file.write_all(bytes)?;
    #[cfg(test)]
    repair_kill::checkpoint("replacement-written");
    sync(&file)?;
    #[cfg(test)]
    repair_kill::checkpoint("replacement-synced");
    directory.rename(&pending, name)?;
    #[cfg(test)]
    repair_kill::checkpoint("replacement-installed");
    Ok(file)
}

fn bad_append() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "record sizes or positions cannot be appended",
    )
}

fn io_error(error: io::Error) -> Error {
    Error::unavailable(format!("markdown store: {error}"))
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::unavailable("markdown store lock is poisoned")
}
