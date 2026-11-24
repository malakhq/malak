use super::Checkpoint;
use std::io;

const HEADER: &[u8] = b"malak-journal-archive\x01";
const PREFIX: usize = 32 + 4;

pub(super) fn decode(covered: Checkpoint, snapshot: &[u8]) -> io::Result<Vec<(Checkpoint, &[u8])>> {
    let mut bytes = snapshot.strip_prefix(HEADER).ok_or_else(invalid)?;
    if covered.sequence > (bytes.len() / PREFIX) as u64 {
        return Err(invalid());
    }
    let mut entries = Vec::new();
    for sequence in 1..=covered.sequence {
        let (prefix, remaining) = bytes.split_at_checked(PREFIX).ok_or_else(invalid)?;
        let running_hash = prefix[..32].try_into().unwrap();
        let length = u32::from_be_bytes(prefix[32..].try_into().unwrap()) as usize;
        let (payload, remaining) = remaining.split_at_checked(length).ok_or_else(invalid)?;
        entries.push((
            Checkpoint {
                sequence,
                running_hash,
            },
            payload,
        ));
        bytes = remaining;
    }
    if !bytes.is_empty() || entries.last().map_or(Checkpoint::EMPTY, |(at, _)| *at) != covered {
        return Err(invalid());
    }
    Ok(entries)
}

pub(super) fn encode<'a>(
    entries: impl Iterator<Item = (Checkpoint, &'a [u8])>,
) -> io::Result<Vec<u8>> {
    let mut output = HEADER.to_vec();
    for (at, payload) in entries {
        let len = u32::try_from(payload.len()).map_err(|_| invalid())?;
        output.extend_from_slice(&at.running_hash);
        output.extend_from_slice(&len.to_be_bytes());
        output.extend_from_slice(payload);
    }
    Ok(output)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "journal archive is damaged")
}
