use crate::{Durability, Error, Position};
use bytes::Bytes;
use std::ops::Range;

/// Description fixed for the duration of a board run.
#[derive(Clone, Copy, Debug)]
pub struct StoreInfo {
    /// Durability reached by a successful append.
    pub durability: Durability,
    /// Maximum opaque record size, in bytes.
    pub max_record_bytes: usize,
    /// Whether appends have enough latency to benefit from batching.
    pub slow_writes: bool,
}

/// A record's position and its opaque payload as given to the store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Record {
    /// Position provided by the board.
    pub position: Position,
    /// The exact bytes provided by the board.
    pub data: Bytes,
}

/// A board request to claim, settle or release a store.
#[derive(Clone, Copy, Debug)]
pub enum Claim<'a> {
    /// Claim exclusive write access, syncing what the store already holds.
    Take,
    /// Claim exclusive access in repair mode, syncing what the store holds.
    Repair,
    /// Sync the named channel and any metadata needed to reach it after a crash.
    Settle {
        /// The current writer's generation.
        generation: u64,
        /// The creation-qualified store name.
        channel: &'a str,
    },
    /// Relinquish ownership.
    Release {
        /// The current writer's generation.
        generation: u64,
    },
}

/// Continuity evidence available to a repair claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorePlace {
    /// Host evidence distinguishing this store from any copy, across a restart.
    Established(Vec<u8>),
    /// The store can report its local filesystem and folder identity.
    ///
    /// The board must match it to its saved identity and require explicit
    /// confirmation from the owner before applying a repair.
    OwnerConfirmation,
    /// Continuity evidence is unavailable.
    NotEstablished,
}

/// The identity of a store directory as reported by its filesystem.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreLocation {
    /// A stable filesystem identifier in the store's own encoding.
    pub filesystem_id: Vec<u8>,
    /// The file number of the store directory.
    pub folder_number: u64,
}

/// The state of the store when exclusive access is granted.
#[derive(Clone, Debug)]
pub struct Ownership {
    /// Persistent random identity of the store.
    pub store_id: [u8; 16],
    /// The writer generation to use on changes.
    pub generation: u64,
    /// Whether there are any records, excluding store bookkeeping.
    pub has_records: bool,
    /// The directory identity, when this store can obtain it from the filesystem.
    pub location: Option<StoreLocation>,
}

/// A successful store claim, sync or release.
#[derive(Clone, Debug)]
pub enum Claimed {
    /// Ownership was acquired in ordinary mode.
    Taken(Ownership),
    /// Ownership was acquired in repair mode.
    Repair {
        /// The store and current writer.
        ownership: Ownership,
        /// The evidence available to establish the store's place.
        place: StorePlace,
        /// Names of all channels with files or records, including reserved channels.
        channels: Vec<String>,
    },
    /// Sync or release completed.
    Done,
}

/// Bounds for an ordinary store read.
#[derive(Clone, Copy, Debug)]
pub struct ReadLimit {
    /// Maximum number of records to return.
    pub records: usize,
    /// Exclusive position bound. `None` reads to the store's end.
    pub before: Option<Position>,
}

/// Result of scanning an ordinary store read or a repair read.
#[derive(Clone, Debug)]
pub enum ReadResult {
    /// Whole records in order, stopping at the first unparseable bytes.
    Records {
        /// Records found within the requested bounds.
        records: Vec<Record>,
        /// Whether unparseable bytes stopped the read.
        unparseable: bool,
    },
    /// Independent candidates from the whole channel in repair mode.
    Repair {
        /// One parseable candidate for each possible record start.
        candidates: Vec<Candidate>,
        /// Number of channel bytes scanned.
        bytes: u64,
        /// SHA-256 of the entire channel's bytes.
        digest: [u8; 32],
        /// Whether the candidates in position order cover the bytes exactly.
        exact: bool,
    },
}

/// A repair candidate. Only the core can decide if it is current.
#[derive(Clone, Debug)]
pub struct Candidate {
    /// The record's parsed position and opaque payload.
    pub record: Record,
    /// The entire original record as encoded by the store.
    pub encoded: Bytes,
}

/// A request to drop a range or to rewrite a channel after damage.
#[derive(Clone, Debug)]
pub enum Trim<'a> {
    /// Keep only positions in a half-open range. Bounds are one-based.
    ///
    /// An empty range empties the channel. A bound can be one past the largest
    /// assignable position so a trim can keep the last position.
    Keep(Range<u64>),
    /// Rewrite approved bytes, setting the old bytes aside before replacement.
    Repair {
        /// The plan's digest.
        plan: [u8; 32],
        /// Digest of the original bytes approved by the owner.
        input: [u8; 32],
        /// Digests and positions of the records to keep in order.
        keep: &'a [KeptRecord],
    },
}

/// Identifies one record by position and the digest of its exact stored bytes.
#[derive(Clone, Copy, Debug)]
pub struct KeptRecord {
    /// The record's position.
    pub position: Position,
    /// SHA-256 of the encoded record.
    pub digest: [u8; 32],
}

/// The result of a trim or approved repair rewrite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Trimmed {
    /// The ordinary trim or repair rewrite completed.
    Done,
    /// A previous repair already left exactly the approved records.
    AlreadyRewritten,
    /// The bytes matched neither the approved input nor the approved output.
    ChangedInput,
}

/// Stores opaque records. The board's journal decides which records count.
///
/// Store names are creation-qualified and still require one-to-one encoding by
/// the implementation. Mutation requires the generation from a successful claim.
pub trait Store: Send + Sync {
    /// State the store's capabilities before it is used.
    fn describe(&self) -> StoreInfo;

    /// Claim exclusive write access, sync a channel or release the current claim.
    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error>;

    /// Append records at increasing positions, reaching the stated durability.
    ///
    /// An error may leave a front part of the append in the store. No part may
    /// arrive after the call answers.
    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error>;

    /// Read from the specified position within the limit.
    ///
    /// In repair mode the bounds are ignored and the entire channel is scanned.
    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error>;

    /// Drop records outside the range, or carry out a rewrite in repair mode.
    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error>;
}

impl<T: Store + ?Sized> Store for Box<T> {
    fn describe(&self) -> StoreInfo {
        (**self).describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        (**self).claim(mode)
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        (**self).append(generation, channel, records)
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        (**self).read(channel, from, limit)
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        (**self).trim(generation, channel, trim)
    }
}
