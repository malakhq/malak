use malak_fs::{Directory, sync};
use nix::fcntl::OFlag;
use sha2::{Digest, Sha256};
use std::fs::{File, TryLockError};
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};

pub(super) const OWNER: &str = ".owner";
const CONTENT: usize = 8 + 16;
const SLOT: usize = CONTENT + 32;

pub(crate) struct Ownership {
    file: File,
    id: [u8; 16],
    pub(crate) generation: u64,
}

impl Ownership {
    pub(crate) fn lock(directory: &Directory) -> io::Result<Self> {
        let file = directory.open_file(OWNER, OFlag::O_RDWR | OFlag::O_CREAT)?;
        match file.try_lock() {
            Ok(()) => (),
            Err(TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        let mut owner = Self {
            file,
            id: [0; 16],
            generation: 0,
        };
        let mut slots = [0; 2 * SLOT];
        let mut read = 0;
        while read < slots.len() {
            match owner.file.read_at(&mut slots[read..], read as u64) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if let Some((generation, id)) = slots[..read]
            .as_chunks::<SLOT>()
            .0
            .iter()
            .filter_map(decode)
            .max_by_key(|&(generation, _)| generation)
        {
            owner.generation = generation;
            owner.id = id;
        }
        Ok(owner)
    }

    pub(crate) fn advance(
        &mut self,
        directory: &Directory,
        has_bytes: bool,
    ) -> io::Result<[u8; 16]> {
        if self.generation == 0 {
            if has_bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "store identity missing",
                ));
            }
            getrandom::fill(&mut self.id)
                .map_err(|_| io::Error::other("could not draw a store ID"))?;
        }
        let next = self
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("writer generations exhausted"))?;
        let mut slot = [0; SLOT];
        slot[..8].copy_from_slice(&next.to_be_bytes());
        slot[8..CONTENT].copy_from_slice(&self.id);
        let sum = checksum(&slot[..CONTENT]);
        slot[CONTENT..].copy_from_slice(&sum);
        // A kill during this write leaves the preceding generation in the other slot.
        let offset = ((next - 1) % 2) * SLOT as u64;
        self.file.write_all_at(&slot, offset)?;
        sync(&self.file)?;
        directory.sync()?;
        self.generation = next;
        Ok(self.id)
    }

    pub(crate) fn matches(&self, directory: &Directory, generation: u64) -> io::Result<()> {
        if generation != self.generation {
            return Err(stale());
        }
        let held = self.file.metadata()?;
        let found = directory.open_file(OWNER, OFlag::O_RDONLY)?.metadata()?;
        if (held.dev(), held.ino()) != (found.dev(), found.ino()) {
            return Err(stale());
        }
        Ok(())
    }
}

impl Drop for Ownership {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn stale() -> io::Error {
    io::Error::other("markdown store writer is no longer current")
}

fn decode(slot: &[u8; SLOT]) -> Option<(u64, [u8; 16])> {
    let generation = u64::from_be_bytes(slot[..8].try_into().ok()?);
    if generation == 0 || checksum(&slot[..CONTENT]) != slot[CONTENT..] {
        return None;
    }
    Some((generation, slot[8..CONTENT].try_into().ok()?))
}

fn checksum(content: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"malak/markdown/owner/v1\0");
    hash.update(content);
    hash.finalize().into()
}
