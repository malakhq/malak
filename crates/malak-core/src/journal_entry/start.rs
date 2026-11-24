use crate::{CreationId, Error};
use malak_layers::StoreLocation;
use malak_proto::KeyPair;
use minicbor::Decoder;
use zeroize::Zeroizing;

pub(super) const VERSION: u8 = 1;
pub(super) const KEYS: u8 = 0;
pub(super) const DIRECT_LEAD: u8 = 1;
pub(super) const COMPLETE: u8 = 2;
pub(super) const RUN: u8 = 3;
pub(super) const LOCAL_LEAD: u8 = 16;

pub(crate) struct SavedKeys {
    pub(crate) record: Zeroizing<[u8; 32]>,
    pub(crate) board: KeyPair,
    pub(crate) owner: KeyPair,
    pub(crate) store: [u8; 16],
    pub(crate) location: Option<StoreLocation>,
}

pub(crate) enum StartupEntry {
    Keys(Box<SavedKeys>),
    DirectLead(CreationId, Option<u64>),
    LocalLead {
        creation: CreationId,
        key: [u8; 32],
        uid: u32,
        enrolled_at_ms: Option<u64>,
    },
    Complete,
    Run([u8; 16]),
}

impl StartupEntry {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        decode(bytes).ok_or_else(|| Error::unavailable("invalid board startup journal entry"))
    }
}

fn decode(bytes: &[u8]) -> Option<StartupEntry> {
    let (tag, length, mut input) = tagged(bytes)?;
    let entry = match (tag, length) {
        (KEYS, 8 | 9) => {
            let record = Zeroizing::new(*fixed(&mut input)?);
            let board = pair(&mut input)?;
            let owner = pair(&mut input)?;
            if board.public() == owner.public() {
                return None;
            }
            StartupEntry::Keys(Box::new(SavedKeys {
                record,
                board,
                owner,
                store: *fixed(&mut input)?,
                location: if length == 9 {
                    Some(super::location::decode(&mut input)?)
                } else {
                    None
                },
            }))
        }
        (DIRECT_LEAD, 3 | 4) => StartupEntry::DirectLead(
            CreationId::from_bytes(*fixed(&mut input)?),
            read_added_time(&mut input, length == 4)?,
        ),
        (LOCAL_LEAD, 5 | 6) => StartupEntry::LocalLead {
            creation: CreationId::from_bytes(*fixed(&mut input)?),
            key: *fixed(&mut input)?,
            uid: input.u32().ok()?,
            enrolled_at_ms: read_added_time(&mut input, length == 6)?,
        },
        (COMPLETE, 2) => StartupEntry::Complete,
        (RUN, 3) => StartupEntry::Run(*fixed(&mut input)?),
        _ => return None,
    };
    (input.position() == bytes.len()).then_some(entry)
}

// Earlier records ended before the display timestamp.
pub(super) fn read_added_time(input: &mut Decoder<'_>, present: bool) -> Option<Option<u64>> {
    if present {
        Some(Some(input.u64().ok()?))
    } else {
        Some(None)
    }
}

fn pair(input: &mut Decoder<'_>) -> Option<KeyPair> {
    let private = fixed(input)?;
    let expected = fixed(input)?;
    let pair = KeyPair::from_private(private).ok()?;
    (pair.public() == expected).then_some(pair)
}

pub(super) fn tagged(bytes: &[u8]) -> Option<(u8, u64, Decoder<'_>)> {
    let mut input = Decoder::new(bytes);
    let length = input.array().ok()??;
    if input.u8().ok()? != VERSION {
        return None;
    }
    let tag = input.u8().ok()?;
    Some((tag, length, input))
}

pub(super) fn fixed<'a, const N: usize>(input: &mut Decoder<'a>) -> Option<&'a [u8; N]> {
    input.bytes().ok()?.try_into().ok()
}
