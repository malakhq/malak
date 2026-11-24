//! Numbered journal entries and replay before the board writes anything.
//!
//! This module works on bytes. The state folder driver owns the file lock,
//! reads and syncs files, and cuts a replay cut point before writing again.

use sha2::{Digest, Sha256};
use std::fmt;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod folder;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use folder::StateFolder;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod disk;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use disk::{DiskError, InspectedJournal, Journal, Save};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod kill;

const VERSION: u8 = 1;
const CONTENT_HEADER: usize = 1 + 8 + 8 + 4;
const DIGEST_LEN: usize = 32;
const CHECKSUM_LABEL: &[u8] = b"malak/journal/checksum\0";

/// The sequence and running hash at a known journal boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    /// The last entry included in this prefix.
    pub sequence: u64,
    /// The running hash including that entry.
    pub running_hash: [u8; DIGEST_LEN],
}

impl Checkpoint {
    /// Journal state before the first entry.
    pub const EMPTY: Self = Self {
        sequence: 0,
        running_hash: [0; DIGEST_LEN],
    };
}

fn running_hash(previous: Checkpoint, content: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hash = Sha256::new();
    hash.update(previous.running_hash);
    hash.update(content);
    hash.finalize().into()
}

fn checksum(bytes: &[u8]) -> [u8; DIGEST_LEN] {
    let mut hash = Sha256::new();
    hash.update(CHECKSUM_LABEL);
    hash.update(bytes);
    hash.finalize().into()
}

/// The reason an entry or replay could not be accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The journal cannot encode another entry at a larger sequence.
    SequenceExhausted,
    /// This entry exceeds the representable byte length.
    EntryTooLarge,
    /// A sync was reported for an entry absent from the written journal.
    SyncBeyondHead,
    /// A later entry proves that a broken entry was previously synced.
    Damaged {
        /// The first broken or missing entry's sequence.
        sequence: u64,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceExhausted => output.write_str("journal sequence exhausted"),
            Self::EntryTooLarge => output.write_str("journal entry is too large"),
            Self::SyncBeyondHead => output.write_str("sync watermark is past the journal head"),
            Self::Damaged { sequence } => write!(output, "journal damaged at entry {sequence}"),
        }
    }
}

impl std::error::Error for Error {}

/// A journal entry ready to append and report back to the cursor.
pub struct Prepared<'a> {
    cursor: &'a mut Cursor,
    next: Checkpoint,
    bytes: Vec<u8>,
}

impl Prepared<'_> {
    /// The whole byte frame to append to the file.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The journal prefix this entry will create.
    pub const fn checkpoint(&self) -> Checkpoint {
        self.next
    }

    /// Advance the cursor after all bytes of this entry were written.
    ///
    /// After a failed write, discard this value and cut the torn bytes before
    /// preparing another entry. The driver must stop changes after a failed sync.
    pub fn written(self) -> Checkpoint {
        self.cursor.head = self.next;
        self.next
    }
}

/// Tracks written entries and successful sync coverage under the journal lock.
pub struct Cursor {
    head: Checkpoint,
    synced_through: u64,
}

impl Cursor {
    /// Start from a loaded checkpoint whose files and directory have been synced.
    pub const fn from_synced(head: Checkpoint) -> Self {
        Self {
            head,
            synced_through: head.sequence,
        }
    }

    /// Return the newest completely written entry.
    pub const fn head(&self) -> Checkpoint {
        self.head
    }

    /// Return the newest sequence known to have reached stable storage.
    pub const fn synced_through(&self) -> u64 {
        self.synced_through
    }

    /// Encode the next entry with the watermark known before its write.
    pub fn prepare(&mut self, payload: &[u8]) -> Result<Prepared<'_>, Error> {
        let sequence = self
            .head
            .sequence
            .checked_add(1)
            .ok_or(Error::SequenceExhausted)?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| Error::EntryTooLarge)?;
        let len = (CONTENT_HEADER + 2 * DIGEST_LEN)
            .checked_add(payload.len())
            .ok_or(Error::EntryTooLarge)?;
        len.checked_add(len / 254 + 4).ok_or(Error::EntryTooLarge)?;

        let mut entry = Vec::with_capacity(len);
        entry.push(VERSION);
        entry.extend_from_slice(&sequence.to_be_bytes());
        entry.extend_from_slice(&self.synced_through.to_be_bytes());
        entry.extend_from_slice(&payload_len.to_be_bytes());
        entry.extend_from_slice(payload);
        let running_hash = running_hash(self.head, &entry);
        entry.extend_from_slice(&running_hash);
        entry.extend_from_slice(&checksum(&entry));

        let mut bytes = vec![0; cobs::max_encoding_length(entry.len()) + 2];
        // The slice encoder counts both sentinels in its returned length.
        let written = cobs::encode_including_sentinels(&entry, &mut bytes);
        bytes.truncate(written);
        Ok(Prepared {
            cursor: self,
            next: Checkpoint {
                sequence,
                running_hash,
            },
            bytes,
        })
    }

    /// Report successful sync coverage through a written sequence.
    ///
    /// Report only the entries present when the sync began, and only if every
    /// file and directory entry required for the prefix has been synced.
    pub fn synced(&mut self, sequence: u64) -> Result<(), Error> {
        if sequence > self.head.sequence {
            return Err(Error::SyncBeyondHead);
        }
        self.synced_through = self.synced_through.max(sequence);
        Ok(())
    }
}

struct Decoded {
    checkpoint: Checkpoint,
    synced_through: u64,
    content: Vec<u8>,
}

impl Decoded {
    fn from_frame(frame: &[u8]) -> Option<Self> {
        let mut entry = cobs::decode_vec(frame).ok()?;
        let (through_hash, check) = entry.split_at_checked(entry.len().checked_sub(DIGEST_LEN)?)?;
        if checksum(through_hash) != check {
            return None;
        }
        let (content, hash) =
            through_hash.split_at_checked(through_hash.len().checked_sub(DIGEST_LEN)?)?;
        let (header, payload) = content.split_at_checked(CONTENT_HEADER)?;
        let [version, rest @ ..] = header else {
            return None;
        };
        if *version != VERSION {
            return None;
        }

        let sequence = u64::from_be_bytes(rest[..8].try_into().ok()?);
        let synced_through = u64::from_be_bytes(rest[8..16].try_into().ok()?);
        let payload_len = u32::from_be_bytes(rest[16..].try_into().ok()?) as usize;
        if synced_through >= sequence || payload.len() != payload_len {
            return None;
        }
        let checkpoint = Checkpoint {
            sequence,
            running_hash: hash.try_into().ok()?,
        };
        entry.truncate(content.len());
        Some(Self {
            checkpoint,
            synced_through,
            content: entry,
        })
    }

    fn follows(&self, head: Checkpoint) -> bool {
        head.sequence.checked_add(1) == Some(self.checkpoint.sequence)
            && running_hash(head, &self.content) == self.checkpoint.running_hash
    }
}

/// Where to cut a tail that cannot be shown to have reached stable storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tail {
    /// First sequence number of the file holding the break.
    pub file: u64,
    /// Byte offset of the first broken frame, or end of the preceding file.
    pub offset: usize,
    /// Sequence number expected at that point.
    pub sequence: u64,
}

/// The replayed prefix and any unsynced tail to discard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Loaded {
    /// Checkpoint reached by the accepted prefix.
    pub head: Checkpoint,
    /// Uncertain bytes to cut, with all later entry files.
    pub tail: Option<Tail>,
}

/// Replays entry files against a loaded snapshot without modifying the files.
pub struct Replay {
    base: u64,
    head: Checkpoint,
    tail: Option<Tail>,
    known_good: Option<(u64, usize)>,
    damaged: Option<u64>,
}

impl Replay {
    /// Start from the checkpoint saved with the snapshot.
    pub const fn new(snapshot: Checkpoint) -> Self {
        Self {
            base: snapshot.sequence,
            head: snapshot,
            tail: None,
            known_good: None,
            damaged: None,
        }
    }

    /// Read one numbered entry file and visit its accepted entries in order.
    ///
    /// Pass files in ascending order. The file number is the sequence of its
    /// first entry. Files covered by the snapshot can reappear after a crash
    /// and are skipped. A replay whose later file proves damage must be discarded.
    pub fn file(
        &mut self,
        first_sequence: u64,
        bytes: &[u8],
        mut visit: impl FnMut(Checkpoint, &[u8]),
    ) -> Result<(), Error> {
        if let Some(sequence) = self.damaged {
            return Err(Error::Damaged { sequence });
        }
        if first_sequence <= self.base {
            return Ok(());
        }

        // A file is rotated only between entries. A numbering gap is a break
        // even before we inspect the first frame in the next file.
        if let Some(expected) = self.head.sequence.checked_add(1)
            && self.tail.is_none()
            && first_sequence != expected
        {
            let (file, offset) = self.known_good.unwrap_or((first_sequence, 0));
            self.break_at(file, offset);
        }

        let mut remaining = bytes;
        while !remaining.is_empty() {
            let offset = bytes.len() - remaining.len();
            let (decoded, consumed) = next_frame(remaining);
            remaining = &remaining[consumed.get()..];
            if self.tail.is_none() {
                if let Some(entry) = &decoded
                    && entry.follows(self.head)
                {
                    self.head = entry.checkpoint;
                    self.known_good = Some((first_sequence, bytes.len() - remaining.len()));
                    visit(self.head, &entry.content[CONTENT_HEADER..]);
                    continue;
                }
                self.break_at(first_sequence, offset);
            }
            if let (Some(tail), Some(entry)) = (self.tail, decoded)
                && entry.synced_through >= tail.sequence
            {
                self.damaged = Some(tail.sequence);
                return Err(Error::Damaged {
                    sequence: tail.sequence,
                });
            }
        }
        Ok(())
    }

    /// Report the state after all files have been examined.
    pub const fn finish(self) -> Result<Loaded, Error> {
        if let Some(sequence) = self.damaged {
            return Err(Error::Damaged { sequence });
        }
        Ok(Loaded {
            head: self.head,
            tail: self.tail,
        })
    }

    fn break_at(&mut self, file: u64, offset: usize) {
        self.tail = Some(Tail {
            file,
            offset,
            sequence: self.head.sequence.saturating_add(1),
        });
    }
}

fn next_frame(bytes: &[u8]) -> (Option<Decoded>, std::num::NonZeroUsize) {
    debug_assert!(!bytes.is_empty());
    // Each entry has both a leading and a trailing zero. A damaged boundary
    // leaves the next independently framed entry discoverable.
    let body_start = usize::from(bytes[0] == 0);
    let end = bytes[body_start..].iter().position(|byte| *byte == 0);
    let (decoded, consumed) = match end {
        Some(end) => {
            let end = end + body_start;
            let decoded = Decoded::from_frame(&bytes[body_start..end]);
            // If a malformed frame lost its trailing zero, this delimiter
            // can be the next frame's start. Complete frames consume it.
            let leave_delimiter = decoded.is_none() && end + 1 < bytes.len();
            (decoded, end + usize::from(!leave_delimiter))
        }
        None => (None, bytes.len()),
    };
    (decoded, std::num::NonZeroUsize::new(consumed).unwrap())
}
