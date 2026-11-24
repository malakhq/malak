use super::repair::{ACCEPT, WITHDRAW};
use super::start::{RUN, fixed, read_added_time, tagged};
use super::{RepairChange, TailCut};
use crate::grant::decode_grant;
use crate::record_fields::RetryDigest;
use crate::{
    ChannelName, CreationId, Durability, Error, Grant, MessageId, ParticipantId, ParticipantName,
    Position, Receipt,
};
use malak_journal::Checkpoint;
use malak_record::Stamp;
use minicbor::{Decoder, data::Type};

pub(crate) enum Change {
    Run([u8; 16]),
    Repair(RepairChange),
    Enrol {
        id: ParticipantId,
        name: ParticipantName,
        creation: CreationId,
        grant: Grant,
        socket: Option<SavedSocket>,
        enrolled_at_ms: Option<u64>,
    },
    Grant(ParticipantId, u64, Grant, Option<u64>),
    Revoke(ParticipantId, Option<u64>),
    Rekey(ParticipantId, u64, [u8; 32], Option<Vec<u8>>),
    Create(ChannelName, CreationId, ParticipantId),
    Reserve(CreationId, ParticipantId, Position, u64, u64),
    Head(SavedHead),
    Trim(CreationId, u64, u64),
    TrimDone(CreationId),
    Remove(CreationId),
    RemoveDone(CreationId),
    Cut(CreationId, u64, u64, Option<TailCut>),
    CutDone(CreationId, bool),
    Damage(CreationId, Position),
    Ack(CreationId, ParticipantId, Position),
    SocketRecord(ParticipantId, u64, Vec<u8>),
    WitnessHanded(Checkpoint),
    WitnessPublished(Checkpoint, Position),
}

pub(crate) struct SavedSocket {
    pub(crate) key: [u8; 32],
    pub(crate) uid: u32,
    pub(crate) place: Option<malak_layers::Source>,
    pub(crate) mechanism: String,
    pub(crate) record: Option<Vec<u8>>,
}

pub(crate) struct SavedHead {
    pub(crate) receipt: Receipt,
    pub(crate) stamp: Stamp,
    pub(crate) kept: u64,
    pub(crate) bytes: u64,
    pub(crate) day: u64,
    pub(crate) sender: ParticipantId,
    pub(crate) id: MessageId,
    pub(crate) digest: RetryDigest,
    pub(crate) lead_session: Option<crate::LeadSession>,
}

impl Change {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        decode(bytes).ok_or_else(|| Error::unavailable("invalid board journal change"))
    }
}

fn decode(bytes: &[u8]) -> Option<Change> {
    let (tag, size, mut input) = tagged(bytes)?;
    let change = match (tag, size) {
        (RUN, 3) => Change::Run(*fixed(&mut input)?),
        (4, 6 | 7) => Change::Enrol {
            id: name(&mut input)?,
            name: name(&mut input)?,
            creation: creation(&mut input)?,
            grant: decode_grant(&mut input)?,
            socket: None,
            enrolled_at_ms: read_added_time(&mut input, size == 7)?,
        },
        (5, 5 | 6) => Change::Grant(
            name(&mut input)?,
            input.u64().ok()?,
            decode_grant(&mut input)?,
            read_added_time(&mut input, size == 6)?,
        ),
        (6, 3 | 4) => Change::Revoke(name(&mut input)?, read_added_time(&mut input, size == 4)?),
        (7, 5) => Change::Create(name(&mut input)?, creation(&mut input)?, name(&mut input)?),
        (8, 7) => Change::Reserve(
            creation(&mut input)?,
            name(&mut input)?,
            position(&mut input)?,
            input.u64().ok()?,
            input.u64().ok()?,
        ),
        (9, 13) | (29, 14 | 15) => {
            let creation = creation(&mut input)?;
            let position = position(&mut input)?;
            let stamp = Stamp {
                run: *fixed(&mut input)?,
                attempt: input.u64().ok()?,
            };
            let kept = input.u64().ok()?;
            let bytes = input.u64().ok()?;
            let day = input.u64().ok()?;
            let sender = name(&mut input)?;
            let id = name(&mut input)?;
            let digest = *fixed(&mut input)?;
            let durability = match input.u8().ok()? {
                1 => Durability::Restart,
                2 => Durability::Synced,
                _ => return None,
            };
            let (digest, trace) = if tag == 9 {
                (RetryDigest::Legacy(digest), None)
            } else {
                let trace = if input.datatype().ok()? == Type::Null {
                    input.null().ok()?;
                    None
                } else {
                    Some(crate::TraceId::parse(input.str().ok()?)?)
                };
                (RetryDigest::TraceAware(digest), trace)
            };
            let lead_session = if size == 15 {
                Some(crate::LeadSession::read(&mut input).ok()?)
            } else {
                None
            };
            Change::Head(SavedHead {
                receipt: Receipt {
                    creation,
                    position,
                    durability,
                    duplicate: false,
                    trace,
                },
                stamp,
                kept,
                bytes,
                day,
                sender,
                id,
                digest,
                lead_session,
            })
        }
        (10, 5) => Change::Trim(creation(&mut input)?, input.u64().ok()?, input.u64().ok()?),
        (11, 3) => Change::TrimDone(creation(&mut input)?),
        (12, 5 | 6) => {
            let creation = creation(&mut input)?;
            let head = input.u64().ok()?;
            let attempt = input.u64().ok()?;
            let tail = if size == 6 {
                Some(tail_cut(&mut input)?)
            } else {
                None
            };
            Change::Cut(creation, head, attempt, tail)
        }
        (13, 4) => Change::CutDone(creation(&mut input)?, input.bool().ok()?),
        (14, 4) => Change::Damage(creation(&mut input)?, position(&mut input)?),
        (15, 5) => Change::Ack(
            creation(&mut input)?,
            name(&mut input)?,
            position(&mut input)?,
        ),
        (17, 11 | 12) => {
            let id = name(&mut input)?;
            let name = name(&mut input)?;
            let inbox = creation(&mut input)?;
            let key = *fixed(&mut input)?;
            let uid = input.u32().ok()?;
            let place = optional(&mut input, |input| {
                if input.datatype().ok()? == minicbor::data::Type::Bytes {
                    creation(input).map(malak_layers::Source::VirtualMachine)
                } else {
                    input.u32().ok().map(malak_layers::Source::LocalUser)
                }
            })?;
            let mechanism = input.str().ok()?.to_owned();
            let record = optional(&mut input, |input| input.bytes().ok().map(<[u8]>::to_vec))?;
            let grant = decode_grant(&mut input)?;
            Change::Enrol {
                id,
                name,
                creation: inbox,
                grant,
                enrolled_at_ms: read_added_time(&mut input, size == 12)?,
                socket: Some(SavedSocket {
                    key,
                    uid,
                    place,
                    mechanism,
                    record,
                }),
            }
        }
        (18, 6) => {
            let id = name(&mut input)?;
            let generation = input.u64().ok()?;
            input.null().ok()?;
            let record = input.bytes().ok()?.to_vec();
            Change::SocketRecord(id, generation, record)
        }
        (19, 4) => Change::WitnessHanded(checkpoint(&mut input)?),
        (20, 5) => Change::WitnessPublished(checkpoint(&mut input)?, position(&mut input)?),
        (26, 3) => Change::Remove(creation(&mut input)?),
        (27, 3) => Change::RemoveDone(creation(&mut input)?),
        (28, 6) => Change::Rekey(
            name(&mut input)?,
            input.u64().ok()?,
            *fixed(&mut input)?,
            optional(&mut input, |input| input.bytes().ok().map(<[u8]>::to_vec))?,
        ),
        (ACCEPT..=WITHDRAW, _) => Change::Repair(RepairChange::decode(tag, size, &mut input)?),
        _ => return None,
    };
    (input.position() == bytes.len()).then_some(change)
}

fn optional<T>(
    input: &mut Decoder<'_>,
    read: impl FnOnce(&mut Decoder<'_>) -> Option<T>,
) -> Option<Option<T>> {
    if input.datatype().ok()? == Type::Null {
        input.null().ok()?;
        Some(None)
    } else {
        read(input).map(Some)
    }
}

fn name<T: std::str::FromStr>(input: &mut Decoder<'_>) -> Option<T> {
    input.str().ok()?.parse().ok()
}

fn creation(input: &mut Decoder<'_>) -> Option<CreationId> {
    Some(CreationId::from_bytes(*fixed(input)?))
}

fn tail_cut(input: &mut Decoder<'_>) -> Option<TailCut> {
    if input.array().ok()? != Some(3) {
        return None;
    }
    let last = match input.u64().ok()? {
        0 => None,
        value => Some(Position::try_from(value).ok()?),
    };
    let tail = TailCut {
        last,
        unparseable: input.bool().ok()?,
        tampered: input.bool().ok()?,
    };
    (tail.has_bytes() && (!tail.tampered || tail.last.is_some())).then_some(tail)
}

fn position(input: &mut Decoder<'_>) -> Option<Position> {
    input.u64().ok()?.try_into().ok()
}

fn checkpoint(input: &mut Decoder<'_>) -> Option<Checkpoint> {
    Some(Checkpoint {
        sequence: input.u64().ok()?,
        running_hash: *fixed(input)?,
    })
}
