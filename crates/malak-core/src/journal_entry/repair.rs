//! Journal representation of accepted repair plans and their progress.

use super::start::{VERSION, fixed};
use super::{Change, Entry, SavedHead};
use crate::{ChannelName, CreationId, Error, Position};
use malak_layers::{KeptRecord, StoreLocation};
use minicbor::{Decoder, Encoder, data::Type};
use std::fmt;
use std::ops::Range;
use std::str::FromStr;

pub(super) const ACCEPT: u8 = 21;
pub(super) const BEGIN: u8 = 22;
pub(super) const FINISH: u8 = 23;
pub(super) const WITHDRAW_CHANNEL: u8 = 24;
pub(super) const WITHDRAW: u8 = 25;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct PlanNumber {
    pub(crate) factor: u64,
    pub(crate) digest: [u8; 32],
}

impl fmt::Display for PlanNumber {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}-", self.factor)?;
        for byte in self.digest {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for PlanNumber {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Error> {
        let invalid = || Error::refused("invalid recovery plan number");
        let (factor, digest) = value.split_once('-').ok_or_else(invalid)?;
        let parsed: u64 = factor.parse().map_err(|_| invalid())?;
        if parsed == 0 || factor != parsed.to_string() || digest.len() != 64 {
            return Err(invalid());
        }
        fn hex(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            }
        }
        let mut bytes = [0; 32];
        for (out, pair) in bytes.iter_mut().zip(digest.as_bytes().as_chunks::<2>().0) {
            *out = hex(pair[0]).ok_or_else(invalid)? * 16 + hex(pair[1]).ok_or_else(invalid)?;
        }
        Ok(Self {
            factor: parsed,
            digest: bytes,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AcceptedPlace {
    Host(Vec<u8>),
    Local(StoreLocation),
}

impl AcceptedPlace {
    fn encode(&self, out: &mut Encoder<Vec<u8>>) {
        match self {
            Self::Host(evidence) => {
                out.bytes(evidence).unwrap();
            }
            Self::Local(location) => super::location::encode(out, location),
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Option<Self> {
        match input.datatype().ok()? {
            Type::Bytes => {
                let evidence = input.bytes().ok()?;
                (!evidence.is_empty()).then(|| Self::Host(evidence.to_vec()))
            }
            Type::Array => super::location::decode(input).map(Self::Local),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AcceptedPlan {
    pub(crate) number: PlanNumber,
    pub(crate) store_id: [u8; 16],
    pub(crate) place: AcceptedPlace,
    pub(crate) channels: Vec<PlannedChannel>,
}

#[derive(Clone)]
pub(crate) struct PlannedChannel {
    pub(crate) name: ChannelName,
    pub(crate) creation: CreationId,
    pub(crate) input: [u8; 32],
    pub(crate) head: u64,
    pub(crate) new_head: u64,
    pub(crate) kept: Vec<KeptRecord>,
    pub(crate) voids: Vec<Range<u64>>,
    pub(crate) phase: Phase,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Phase {
    Pending,
    Begun,
    Finished,
    Withdrawn,
}

impl Phase {
    pub(crate) fn unfinished(self) -> bool {
        matches!(self, Self::Pending | Self::Begun)
    }
}

pub(crate) enum RepairChange {
    Accept(AcceptedPlan),
    Begin(PlanNumber, CreationId, u64),
    Finish(PlanNumber, CreationId, Vec<SavedHead>),
    WithdrawChannel(PlanNumber, CreationId),
    Withdraw(PlanNumber),
}

impl RepairChange {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new(Vec::new());
        match self {
            Self::Accept(plan) => {
                prefix(&mut out, ACCEPT, 4, plan.number);
                out.bytes(&plan.store_id).unwrap();
                plan.place.encode(&mut out);
                out.array(plan.channels.len() as u64).unwrap();
                for channel in &plan.channels {
                    out.array(7)
                        .unwrap()
                        .str(channel.name.as_str())
                        .unwrap()
                        .bytes(channel.creation.as_bytes())
                        .unwrap()
                        .bytes(&channel.input)
                        .unwrap()
                        .u64(channel.head)
                        .unwrap()
                        .u64(channel.new_head)
                        .unwrap();
                    out.array(channel.kept.len() as u64).unwrap();
                    for item in &channel.kept {
                        out.array(2)
                            .unwrap()
                            .u64(item.position.get())
                            .unwrap()
                            .bytes(&item.digest)
                            .unwrap();
                    }
                    out.array(channel.voids.len() as u64).unwrap();
                    for range in &channel.voids {
                        out.array(2)
                            .unwrap()
                            .u64(range.start)
                            .unwrap()
                            .u64(range.end)
                            .unwrap();
                    }
                }
            }
            Self::Begin(number, creation, attempt) => {
                prefix(&mut out, BEGIN, 3, *number);
                out.bytes(creation.as_bytes())
                    .unwrap()
                    .u64(*attempt)
                    .unwrap();
            }
            Self::Finish(number, creation, heads) => {
                prefix(&mut out, FINISH, 3, *number);
                out.bytes(creation.as_bytes())
                    .unwrap()
                    .array(heads.len() as u64)
                    .unwrap();
                for head in heads {
                    out.bytes(
                        &Entry::Head {
                            creation: head.receipt.creation,
                            position: head.receipt.position,
                            stamp: head.stamp,
                            new_stamp: true,
                            bytes_kept: head.kept,
                            record_bytes: head.bytes,
                            day: head.day,
                            sender: &head.sender,
                            id: &head.id,
                            digest: &head.digest,
                            durability: head.receipt.durability,
                            trace: head.receipt.trace,
                            lead_session: head.lead_session.as_ref(),
                        }
                        .encode(),
                    )
                    .unwrap();
                }
            }
            Self::WithdrawChannel(number, creation) => {
                prefix(&mut out, WITHDRAW_CHANNEL, 2, *number);
                out.bytes(creation.as_bytes()).unwrap();
            }
            Self::Withdraw(number) => prefix(&mut out, WITHDRAW, 1, *number),
        }
        out.into_writer()
    }

    pub(super) fn decode(tag: u8, length: u64, input: &mut Decoder<'_>) -> Option<Self> {
        let number = read_number(input)?;
        Some(match (tag, length) {
            (ACCEPT, 6) => {
                let store_id = *fixed(input)?;
                let place = AcceptedPlace::decode(input)?;
                let channels = many(input, planned)?;
                if channels.is_empty() {
                    return None;
                }
                Self::Accept(AcceptedPlan {
                    number,
                    store_id,
                    place,
                    channels,
                })
            }
            (BEGIN, 5) => Self::Begin(number, creation(input)?, input.u64().ok()?),
            (FINISH, 5) => Self::Finish(
                number,
                creation(input)?,
                many(input, |input| {
                    match Change::decode(input.bytes().ok()?).ok()? {
                        Change::Head(head) => Some(head),
                        _ => None,
                    }
                })?,
            ),
            (WITHDRAW_CHANNEL, 4) => Self::WithdrawChannel(number, creation(input)?),
            (WITHDRAW, 3) => Self::Withdraw(number),
            _ => return None,
        })
    }
}

fn planned(input: &mut Decoder<'_>) -> Option<PlannedChannel> {
    if input.array().ok()? != Some(7) {
        return None;
    }
    let name = input.str().ok()?.parse().ok()?;
    let creation = creation(input)?;
    let digest = *fixed(input)?;
    let head = input.u64().ok()?;
    let new_head = input.u64().ok()?;
    let kept = many(input, |input| {
        if input.array().ok()? != Some(2) {
            return None;
        }
        Some(KeptRecord {
            position: input.u64().ok()?.try_into().ok()?,
            digest: *fixed(input)?,
        })
    })?;
    let voids = many(input, |input| {
        if input.array().ok()? != Some(2) {
            return None;
        }
        let range = input.u64().ok()?..input.u64().ok()?;
        (!range.is_empty() && range.start > 0 && range.end <= head).then_some(range)
    })?;
    if !(1..=Position::MAX + 1).contains(&head)
        || !(head..=Position::MAX + 1).contains(&new_head)
        || kept
            .windows(2)
            .any(|pair| pair[0].position >= pair[1].position)
        || voids.windows(2).any(|pair| pair[0].end > pair[1].start)
        || kept
            .last()
            .is_some_and(|record| record.position.get() >= new_head)
    {
        return None;
    }
    let adopted = kept.iter().filter(|record| record.position.get() >= head);
    if adopted.count() as u64 != new_head - head {
        return None;
    }
    Some(PlannedChannel {
        name,
        creation,
        input: digest,
        head,
        new_head,
        kept,
        voids,
        phase: Phase::Pending,
    })
}

fn many<T>(
    input: &mut Decoder<'_>,
    mut item: impl FnMut(&mut Decoder<'_>) -> Option<T>,
) -> Option<Vec<T>> {
    let count = input.array().ok()??;
    if count > (input.input().len() - input.position()) as u64 {
        return None;
    }
    (0..count).map(|_| item(input)).collect()
}

fn prefix(out: &mut Encoder<Vec<u8>>, tag: u8, length: u64, number: PlanNumber) {
    out.array(length + 2)
        .unwrap()
        .u8(VERSION)
        .unwrap()
        .u8(tag)
        .unwrap()
        .array(2)
        .unwrap()
        .u64(number.factor)
        .unwrap()
        .bytes(&number.digest)
        .unwrap();
}

fn read_number(input: &mut Decoder<'_>) -> Option<PlanNumber> {
    if input.array().ok()? != Some(2) {
        return None;
    }
    let factor = input.u64().ok()?;
    let digest = *fixed(input)?;
    (factor > 0).then_some(PlanNumber { factor, digest })
}

fn creation(input: &mut Decoder<'_>) -> Option<CreationId> {
    Some(CreationId::from_bytes(*fixed(input)?))
}
