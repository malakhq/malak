//! The integrity boundary between the Malak journal and an opaque store.
//!
//! This crate is used by the board. Store implementations carry the encoded
//! records unchanged and can operate without the keys.

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use malak_layers::{ChannelName, CreationId, Error, Position, Record};
use sha2::{Digest, Sha256};
use std::ops::Range;

const VERSION: u8 = 1;
const MESSAGE_LABEL: &[u8] = b"malak/message\0";
const WITNESS_LABEL: &[u8] = b"malak/witness\0";
const CHECK_LEN: usize = 32;
const HEADER_LEN: usize = 1 + 16 + 8 + 4 + 4;

/// Qualify a channel name by its creation before handing it to a store.
pub fn store_name(channel: &ChannelName, creation: CreationId) -> String {
    format!("{channel}/_{creation}")
}

/// The run and attempt under which the board signed a record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stamp {
    /// Random identifier saved before the run signs records.
    pub run: [u8; 16],
    /// The attempt number saved before cutting a channel's end.
    pub attempt: u64,
}

/// The fields borrowed from a record that passed its check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verified<'a> {
    /// Stamp carried by the signed record.
    pub stamp: Stamp,
    /// Canonically encoded board and message fields, including the receipt.
    pub metadata: &'a [u8],
    /// Opaque message body.
    pub body: &'a [u8],
}

/// Channel state supplied by the journal for an ordinary read.
pub struct History<'a> {
    /// Creation-qualified name supplied to the store.
    pub channel: &'a str,
    /// The range from the journal floor up to, excluding, the head.
    pub retained: Range<u64>,
    /// Stamp change points sorted by ascending position.
    pub stamps: &'a [(Position, Stamp)],
    /// Disjoint half-open void ranges sorted by ascending start position.
    pub voids: &'a [Range<u64>],
}

impl History<'_> {
    /// Whether the journal names this stamp as current at this position.
    ///
    /// Callers must independently verify the record before using its contents.
    pub fn names(&self, position: Position, stamp: Stamp) -> bool {
        self.expects(position) == Some(stamp)
    }

    fn expects(&self, position: Position) -> Option<Stamp> {
        let number = position.get();
        if !self.retained.contains(&number) {
            return None;
        }

        let index = self.voids.partition_point(|range| range.start <= number);
        if index > 0 && self.voids[index - 1].contains(&number) {
            return None;
        }

        let index = self.stamps.partition_point(|(from, _)| *from <= position);
        index.checked_sub(1).map(|index| self.stamps[index].1)
    }
}

/// An opaque body together with its SHA-256 digest.
///
/// The hash and bytes are bound together so signing cannot accept a digest
/// for a different body.
pub struct HashedBody<'a> {
    bytes: &'a [u8],
    digest: [u8; 32],
}

impl<'a> HashedBody<'a> {
    /// Hash a body without copying it.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            digest: Sha256::digest(bytes).into(),
        }
    }

    /// Digest of these exact bytes.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// The original bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Extract body bytes for a presentation of an untrusted record.
///
/// This checks lengths but does not authenticate the record. Board reads must
/// use the journal and the record key.
pub fn unverified_body(record: &Record) -> Option<&[u8]> {
    record_parts(&record.data).map(|parts| parts.body)
}

struct Parts<'a> {
    covered: &'a [u8],
    body: &'a [u8],
    check: &'a [u8],
}

fn record_parts(bytes: &[u8]) -> Option<Parts<'_>> {
    let header = bytes.get(..HEADER_LEN)?;
    if header[0] != VERSION {
        return None;
    }
    let metadata_len = u32::from_be_bytes(header[25..29].try_into().ok()?) as usize;
    let body_len = u32::from_be_bytes(header[29..33].try_into().ok()?) as usize;
    let (covered, rest) = bytes.split_at_checked(HEADER_LEN.checked_add(metadata_len)?)?;
    let (body, check) = rest.split_at_checked(body_len)?;
    (check.len() == CHECK_LEN).then_some(Parts {
        covered,
        body,
        check,
    })
}

/// The board's secret key for message and witness checks.
pub struct RecordKey(Hmac<Sha256>);

impl RecordKey {
    /// Prepare the board's existing 256-bit record key for use.
    pub fn new(key: &[u8; 32]) -> Self {
        Self(Hmac::new_from_slice(key).expect("HMAC accepts any key length"))
    }

    /// Length of a signed message record with the given metadata and body sizes.
    pub fn sealed_size(metadata_len: usize, body_len: usize) -> Result<usize, Error> {
        let too_large = || Error::unavailable("message record is too large");
        u32::try_from(metadata_len).map_err(|_| too_large())?;
        u32::try_from(body_len).map_err(|_| too_large())?;
        (HEADER_LEN + CHECK_LEN)
            .checked_add(metadata_len)
            .and_then(|length| length.checked_add(body_len))
            .ok_or_else(too_large)
    }

    /// Seal the board's metadata and a body at a journal-assigned position.
    ///
    /// The journal must ensure this stamp has not already signed this position
    /// in the creation. The metadata contains all message fields and the receipt.
    pub fn seal(
        &self,
        channel: &str,
        position: Position,
        stamp: Stamp,
        metadata: &[u8],
        body: &[u8],
    ) -> Result<Record, Error> {
        self.seal_hashed(channel, position, stamp, metadata, &HashedBody::new(body))
    }

    /// Seal a message with a body hash that the caller already computed.
    ///
    /// Uses the same record format and check as [`Self::seal`].
    pub fn seal_hashed(
        &self,
        channel: &str,
        position: Position,
        stamp: Stamp,
        metadata: &[u8],
        body: &HashedBody<'_>,
    ) -> Result<Record, Error> {
        let len = Self::sealed_size(metadata.len(), body.bytes().len())?;
        let metadata_len = metadata.len() as u32;
        let body_len = body.bytes().len() as u32;
        let mut data = Vec::with_capacity(len);

        data.push(VERSION);
        data.extend_from_slice(&stamp.run);
        data.extend_from_slice(&stamp.attempt.to_be_bytes());
        data.extend_from_slice(&metadata_len.to_be_bytes());
        data.extend_from_slice(&body_len.to_be_bytes());
        data.extend_from_slice(metadata);

        let check = self
            .message_mac(channel, position, &data, body.digest())
            .finalize();
        data.extend_from_slice(body.bytes());
        data.extend_from_slice(&check.into_bytes());
        Ok(Record {
            position,
            data: Bytes::from(data),
        })
    }

    /// Verify an opaque record under its creation-qualified store name.
    ///
    /// The record may be unfinished or outside history. Use the journal to
    /// decide whether to return it, adopt it, or treat it as damage.
    pub fn verify<'a>(&self, channel: &str, record: &'a Record) -> Option<Verified<'a>> {
        let parts = record_parts(&record.data)?;
        let digest: [u8; 32] = Sha256::digest(parts.body).into();
        self.message_mac(channel, record.position, parts.covered, &digest)
            .verify_slice(parts.check)
            .ok()?;

        Some(Verified {
            stamp: Stamp {
                run: parts.covered[1..17].try_into().ok()?,
                attempt: u64::from_be_bytes(parts.covered[17..25].try_into().ok()?),
            },
            metadata: &parts.covered[HEADER_LEN..],
            body: parts.body,
        })
    }

    /// Verify a record and confirm that the journal names it as current.
    pub fn current<'a>(&self, history: &History<'_>, record: &'a Record) -> Option<Verified<'a>> {
        let verified = self.verify(history.channel, record)?;
        history
            .names(record.position, verified.stamp)
            .then_some(verified)
    }

    /// Sign a witness for one synced journal sequence and its running hash.
    pub fn witness_check(&self, sequence: u64, running_hash: &[u8; 32]) -> [u8; 32] {
        self.witness_mac(sequence, running_hash)
            .finalize()
            .into_bytes()
            .into()
    }

    /// Check a stored witness under the board's record key.
    pub fn verify_witness(&self, sequence: u64, running_hash: &[u8; 32], check: &[u8]) -> bool {
        self.witness_mac(sequence, running_hash)
            .verify_slice(check)
            .is_ok()
    }

    fn message_mac(
        &self,
        channel: &str,
        position: Position,
        header_and_metadata: &[u8],
        body_digest: &[u8; 32],
    ) -> Hmac<Sha256> {
        let mut mac = self.0.clone();
        mac.update(MESSAGE_LABEL);
        mac.update(&(channel.len() as u64).to_be_bytes());
        mac.update(channel.as_bytes());
        mac.update(&position.get().to_be_bytes());
        mac.update(header_and_metadata);
        mac.update(body_digest);
        mac
    }

    fn witness_mac(&self, sequence: u64, running_hash: &[u8; 32]) -> Hmac<Sha256> {
        let mut mac = self.0.clone();
        mac.update(WITNESS_LABEL);
        mac.update(&[VERSION]);
        mac.update(&sequence.to_be_bytes());
        mac.update(running_hash);
        mac
    }
}
