use crate::Checkpoint;
use sha2::{Digest, Sha256};
use std::io;

const VERSION: u8 = 1;
const HEADER: usize = 1 + 8 + 32;
const CHECKSUM: usize = 32;

pub(super) const FILE: &str = "snapshot";
pub(super) const PENDING: &str = "snapshot.pending";

pub(super) struct Snapshot {
    pub(super) checkpoint: Checkpoint,
    bytes: Vec<u8>,
}

impl Snapshot {
    pub(super) fn decode(bytes: Vec<u8>) -> io::Result<Self> {
        if bytes.len() < HEADER + CHECKSUM || bytes[0] != VERSION {
            return Err(damaged());
        }
        let (content, checksum) = bytes.split_at(bytes.len() - CHECKSUM);
        if digest(content) != checksum {
            return Err(damaged());
        }
        let checkpoint = Checkpoint {
            sequence: u64::from_be_bytes(content[1..9].try_into().unwrap()),
            running_hash: content[9..HEADER].try_into().unwrap(),
        };
        if checkpoint.sequence == 0 && checkpoint != Checkpoint::EMPTY {
            return Err(damaged());
        }

        Ok(Self { checkpoint, bytes })
    }

    pub(super) fn payload(&self) -> &[u8] {
        &self.bytes[HEADER..self.bytes.len() - CHECKSUM]
    }
}

pub(super) fn encode(at: Checkpoint, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + payload.len() + CHECKSUM);
    bytes.push(VERSION);
    bytes.extend_from_slice(&at.sequence.to_be_bytes());
    bytes.extend_from_slice(&at.running_hash);
    bytes.extend_from_slice(payload);
    let check = digest(&bytes);
    bytes.extend_from_slice(&check);
    bytes
}

fn digest(content: &[u8]) -> [u8; CHECKSUM] {
    let mut hash = Sha256::new();
    hash.update(b"malak/journal/snapshot/v1\0");
    hash.update(content);
    hash.finalize().into()
}

fn damaged() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "journal snapshot is damaged")
}
