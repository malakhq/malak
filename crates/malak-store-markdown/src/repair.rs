use super::format::{MARKER, decode};
use super::{CREATE, ChannelState, Identity, existing, read_all, replace};
use bytes::Bytes;
use data_encoding::HEXLOWER;
use malak_fs::{Directory, sync};
use malak_layers::{Candidate, KeptRecord, ReadResult, Trimmed};
use nix::fcntl::OFlag;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, Write};

pub(super) struct Scan {
    bytes: Bytes,
    digest: [u8; 32],
    candidates: Vec<Candidate>,
    exact: bool,
}

impl Scan {
    pub(super) fn new(bytes: Vec<u8>) -> Self {
        let bytes = Bytes::from(bytes);
        let digest = Sha256::digest(&bytes).into();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut end = 0;
        let mut exact = true;
        for (offset, _) in bytes
            .windows(MARKER.len())
            .enumerate()
            .filter(|(_, part)| *part == MARKER)
        {
            if let Some((record, length)) = decode(&bytes[offset..]) {
                exact &= offset == end
                    && candidates
                        .last()
                        .is_none_or(|previous| previous.record.position < record.position);
                end = offset + length;
                candidates.push(Candidate {
                    record,
                    encoded: bytes.slice(offset..end),
                });
            }
        }
        exact &= end == bytes.len();
        Self {
            bytes,
            digest,
            candidates,
            exact,
        }
    }

    fn matches(&self, keep: &[KeptRecord]) -> bool {
        self.exact
            && self.candidates.len() == keep.len()
            && self
                .candidates
                .iter()
                .zip(keep)
                .all(|(candidate, expected)| {
                    candidate.record.position == expected.position
                        && Sha256::digest(&candidate.encoded).as_slice() == expected.digest
                })
    }

    pub(super) fn into_result(self) -> ReadResult {
        ReadResult::Repair {
            candidates: self.candidates,
            bytes: self.bytes.len() as u64,
            digest: self.digest,
            exact: self.exact,
        }
    }
}

pub(super) fn rewrite(
    directory: &Directory,
    name: &str,
    state: &mut ChannelState,
    plan: [u8; 32],
    input: [u8; 32],
    keep: &[KeptRecord],
) -> io::Result<Trimmed> {
    let bytes = match existing(directory, name, OFlag::O_RDONLY)? {
        Some(mut file) => read_all(&mut file)?,
        None => Vec::new(),
    };
    let scan = Scan::new(bytes);
    if scan.digest != input {
        return Ok(if scan.matches(keep) {
            Trimmed::AlreadyRewritten
        } else {
            Trimmed::ChangedInput
        });
    }
    if keep
        .windows(2)
        .any(|pair| pair[0].position >= pair[1].position)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "repair positions are not increasing",
        ));
    }
    let by_record: HashMap<_, _> = scan
        .candidates
        .iter()
        .map(|candidate| {
            let digest: [u8; 32] = Sha256::digest(&candidate.encoded).into();
            ((candidate.record.position, digest), &candidate.encoded)
        })
        .collect();
    let mut output = Vec::new();
    for record in keep {
        let bytes = by_record
            .get(&(record.position, record.digest))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "approved repair record is absent",
                )
            })?;
        output.extend_from_slice(bytes);
    }
    let output = Scan::new(output);
    if !output.matches(keep) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "approved records do not make an exact channel",
        ));
    }
    set_aside(directory, name, &plan, &scan)?;
    let rewritten = replace(directory, name, &output.bytes)?;
    state.identity = Some(Identity::read(&rewritten)?);
    state.new_name = true;
    state.sync_name(directory)?;
    #[cfg(test)]
    crate::repair_kill::checkpoint("replacement-directory-synced");
    Ok(Trimmed::Done)
}

const BACKUP_PREFIX: &str = ".repair-";

pub(super) fn is_backup(name: &str) -> bool {
    name.strip_prefix(BACKUP_PREFIX)
        .and_then(|suffix| suffix.split_once('-'))
        .is_some_and(|(plan, channel)| {
            [plan, channel]
                .iter()
                .all(|digest| digest.len() == 64 && HEXLOWER.decode(digest.as_bytes()).is_ok())
        })
}

fn set_aside(directory: &Directory, name: &str, plan: &[u8; 32], scan: &Scan) -> io::Result<()> {
    let channel = HEXLOWER.encode(&Sha256::digest(name.as_bytes()));
    let backup = format!("{BACKUP_PREFIX}{}-{channel}", HEXLOWER.encode(plan));
    if let Some(mut file) = existing(directory, &backup, OFlag::O_RDONLY)? {
        if Sha256::digest(read_all(&mut file)?).as_slice() != scan.digest {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "set-aside copy has unexpected bytes",
            ));
        }
        sync(&file)?;
    } else {
        let pending = format!("{backup}.next");
        match directory.remove(&pending) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => (),
        }
        let mut file = directory.open_file(&pending, OFlag::O_WRONLY | CREATE)?;
        #[cfg(test)]
        crate::repair_kill::torn(&mut file, &scan.bytes, "backup-torn")?;
        file.write_all(&scan.bytes)?;
        #[cfg(test)]
        crate::repair_kill::checkpoint("backup-written");
        sync(&file)?;
        #[cfg(test)]
        crate::repair_kill::checkpoint("backup-synced");
        directory.rename_to_new(&pending, &backup)?;
        #[cfg(test)]
        crate::repair_kill::checkpoint("backup-installed");
    }
    directory.sync()?;
    #[cfg(test)]
    crate::repair_kill::checkpoint("backup-directory-synced");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::encode;
    use malak_layers::Record;

    #[test]
    fn a_candidate_does_not_hide_a_marker_inside_its_stated_length() {
        let mut inner = Vec::new();
        encode(
            &Record {
                position: 2.try_into().unwrap(),
                data: Bytes::from_static(b"inner"),
            },
            &mut inner,
        );
        let mut outer = Vec::new();
        encode(
            &Record {
                position: 1.try_into().unwrap(),
                data: Bytes::from(vec![b'A'; inner.len()]),
            },
            &mut outer,
        );
        let view = outer
            .windows(inner.len())
            .position(|part| part.iter().all(|byte| *byte == b'A'))
            .unwrap();
        outer[view..view + inner.len()].copy_from_slice(&inner);

        let scanned = Scan::new(outer);

        assert_eq!(scanned.candidates.len(), 2);
        assert_eq!(scanned.candidates[1].encoded, inner);
        assert!(!scanned.exact);
    }
}
