use crate::frame_fields::Fields;
use crate::{
    CreationId, Durability, Error, GetReply, Header, HeaderValue, Kind, Message, Outcome,
    ReadReply, Receipt, SendRequest,
};
use bytes::Bytes;
use malak_proto::{Frame, Operation, OwnerOutcome, encode_frame, owner_outcome};
use minicbor::{Decoder, Encoder};
use std::time::{Duration, UNIX_EPOCH};

pub(super) enum Call<'a> {
    Send(Send<'a>),
    Read {
        channels: Vec<&'a str>,
        limit: usize,
        wait: Duration,
    },
    Get {
        channel: &'a str,
        position: u64,
    },
    Ack {
        channel: &'a str,
        creation: CreationId,
        position: u64,
    },
}

pub(super) fn channel(frame: &Frame) -> Option<crate::ChannelName> {
    Fields::new(frame)
        .ok()?
        .required("channel")
        .ok()?
        .parse()
        .ok()
}

pub(super) struct Send<'a> {
    channel: &'a str,
    id: &'a str,
    kind: Kind,
    headers: Vec<(&'a str, HeaderValue<'a>)>,
    body: &'a [u8],
    minimum_durability: Option<Durability>,
    pub(super) agent_session_id: Option<&'a str>,
}

impl Send<'_> {
    pub(super) fn request(&self) -> SendRequest<'_> {
        SendRequest {
            channel: self.channel,
            id: self.id,
            kind: self.kind,
            headers: &self.headers,
            body: self.body,
            minimum_durability: self.minimum_durability,
        }
    }
}

pub(super) fn decode(operation: Operation, frame: &Frame) -> Result<Call<'_>, Error> {
    let fields = Fields::new(frame)?;
    let call = match operation {
        Operation::Send => {
            let kind = fields.text("kind")?.unwrap_or("note").parse()?;
            let headers = fields
                .get("headers")
                .map(headers)
                .transpose()?
                .unwrap_or_default();
            let minimum_durability = fields
                .text("minimum_durability")?
                .map(durability)
                .transpose()?;
            Call::Send(Send {
                channel: fields.required("channel")?,
                id: fields.required("id")?,
                kind,
                headers,
                body: frame.body(),
                minimum_durability,
                agent_session_id: fields.text("agent_session_id")?.filter(|id| !id.is_empty()),
            })
        }
        Operation::Read => {
            let mut input = fields.get("channels").ok_or_else(shape)?;
            let count = input.array().map_err(|_| shape())?.ok_or_else(shape)?;
            let mut channels = Vec::new();
            for _ in 0..count {
                channels.push(input.str().map_err(|_| shape())?);
            }
            let limit = fields
                .number("limit")?
                .map(usize::try_from)
                .transpose()
                .map_err(|_| shape())?
                .unwrap_or(usize::MAX);
            let wait = Duration::from_millis(fields.number("wait_ms")?.unwrap_or(0));
            Call::Read {
                channels,
                limit,
                wait,
            }
        }
        Operation::Get => Call::Get {
            channel: fields.required("channel")?,
            position: fields.number("position")?.ok_or_else(shape)?,
        },
        Operation::Ack => Call::Ack {
            channel: fields.required("channel")?,
            creation: fields.required("creation")?.parse()?,
            position: fields.number("position")?.ok_or_else(shape)?,
        },
        _ => return Err(shape()),
    };
    Ok(call)
}

fn headers<'a>(mut input: Decoder<'a>) -> Result<Vec<(&'a str, HeaderValue<'a>)>, Error> {
    let count = input.map().map_err(|_| shape())?.ok_or_else(shape)?;
    let mut headers = Vec::new();
    for _ in 0..count {
        let key = input.str().map_err(|_| shape())?;
        let value = if key == "reply_to" {
            if input.array().map_err(|_| shape())? != Some(3) {
                return Err(shape());
            }
            HeaderValue::ReplyTo {
                channel: input.str().map_err(|_| shape())?,
                creation: input.str().map_err(|_| shape())?.parse()?,
                position: input.u64().map_err(|_| shape())?,
            }
        } else {
            HeaderValue::Text(input.str().map_err(|_| shape())?)
        };
        headers.push((key, value));
    }
    Ok(headers)
}

pub(super) fn message(rid: u64, message: &Message, lost_before: u64) -> Result<Bytes, Error> {
    message_frame(rid, message, lost_before, None)
}

pub(in crate::board::local) fn exit_record(
    rid: u64,
    message: &Message,
    duplicate: bool,
) -> Result<Bytes, Error> {
    message_frame(rid, message, 0, Some(duplicate))
}

fn message_frame(
    rid: u64,
    message: &Message,
    lost_before: u64,
    duplicate: Option<bool>,
) -> Result<Bytes, Error> {
    let receipt = &message.receipt;
    let time = message
        .sent_at
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    build(
        rid,
        if duplicate.is_some() { "OK" } else { "MSG" },
        11 + usize::from(lost_before != 0)
            + usize::from(message.sender_trace.is_some())
            + usize::from(duplicate.is_some()),
        |out| {
            if let Some(duplicate) = duplicate {
                boolean(out, "duplicate", duplicate);
            }
            text(out, "channel", message.channel.as_str());
            text(out, "id", message.id.as_str());
            text(out, "kind", message.kind.as_str());
            text(out, "sender", message.sender.as_str());
            text(out, "sender_name", message.sender_name.as_str());
            if let Some(sender) = &message.sender_trace {
                out.str("sender_trace").unwrap();
                crate::sender::write(out, sender, message.earlier_session.as_ref(), None);
            }
            text(
                out,
                "grade",
                match message.grade {
                    crate::Grade::Proven => "proven",
                    crate::Grade::LinkOnly => "link_only",
                },
            );
            out.str("sent_at")
                .unwrap()
                .array(2)
                .unwrap()
                .u64(time.as_secs())
                .unwrap()
                .u32(time.subsec_nanos())
                .unwrap();
            out.str("headers")
                .unwrap()
                .map(message.headers.len() as u64)
                .unwrap();
            for header in &message.headers {
                write_header(out, header);
            }
            text(out, "creation", &receipt.creation.to_string());
            number(out, "position", receipt.position.get());
            text(out, "durability", durability_name(receipt.durability));
            if lost_before != 0 {
                number(out, "lost_before", lost_before);
            }
        },
        &message.body,
    )
}

pub(super) fn sent(rid: u64, receipt: Receipt) -> Result<Bytes, Error> {
    build(
        rid,
        "OK",
        4 + usize::from(receipt.trace.is_some()),
        |out| {
            text(out, "creation", &receipt.creation.to_string());
            number(out, "position", receipt.position.get());
            text(out, "durability", durability_name(receipt.durability));
            boolean(out, "duplicate", receipt.duplicate);
            if let Some(trace) = &receipt.trace {
                text(out, "trace", trace.as_str());
            }
        },
        &[],
    )
}

pub(super) fn got(
    rid: u64,
    channel: &str,
    position: u64,
    reply: &GetReply,
) -> Result<Bytes, Error> {
    let (result, creation, floor) = match *reply {
        GetReply::Message(ref message) => ("message", Some(message.receipt.creation), None),
        GetReply::Trimmed { creation, before } => ("trimmed", Some(creation), Some(before)),
        GetReply::Void { creation } => ("void", Some(creation), None),
        GetReply::NeverWritten { creation } => ("never_written", Some(creation), None),
        GetReply::Absent => ("absent", None, None),
    };
    build(
        rid,
        "OK",
        2 + 2 * usize::from(creation.is_some()) + usize::from(floor.is_some()),
        |out| {
            text(out, "result", result);
            text(out, "channel", channel);
            if let Some(creation) = creation {
                text(out, "creation", &creation.to_string());
                number(out, "position", position);
            }
            if let Some(floor) = floor {
                number(out, "trimmed_before", floor);
            }
        },
        &[],
    )
}

pub(super) fn read(rid: u64, reply: &ReadReply) -> Result<Bytes, Error> {
    build(
        rid,
        "OK",
        2,
        |out| {
            boolean(out, "held_back", reply.held_back);
            out.str("channels")
                .unwrap()
                .array(reply.channels.len() as u64)
                .unwrap();
            for channel in &reply.channels {
                out.map(
                    2 + u64::from(channel.creation.is_some())
                        + u64::from(channel.trimmed_before.is_some()),
                )
                .unwrap();
                text(out, "channel", channel.channel.as_str());
                number(out, "voids", channel.voids);
                if let Some(creation) = channel.creation {
                    text(out, "creation", &creation.to_string());
                }
                if let Some(floor) = channel.trimmed_before {
                    number(out, "trimmed_before", floor);
                }
            }
        },
        &[],
    )
}

pub(super) fn cancelled(rid: u64, key: &str, value: bool) -> Result<Bytes, Error> {
    build(rid, "OK", 1, |out| boolean(out, key, value), &[])
}

pub(super) fn ok(rid: u64) -> Result<Bytes, Error> {
    owner_outcome(rid, OwnerOutcome::Done).map_err(|_| oversized())
}

pub(super) fn error(rid: u64, error: &Error) -> Result<Bytes, Error> {
    let outcome = match error.outcome() {
        Outcome::Refused => "refused",
        Outcome::Unavailable => "unavailable",
        Outcome::NotSupported => "not_supported",
        Outcome::Done => unreachable!("error always has a failure outcome"),
    };
    let reason = error.to_string();
    owner_outcome(
        rid,
        OwnerOutcome::Error {
            outcome,
            reason: &reason,
        },
    )
    .map_err(|_| oversized())
}

fn write_header(out: &mut Encoder<Vec<u8>>, header: &Header) {
    match header {
        Header::ReplyTo {
            channel,
            creation,
            position,
        } => {
            out.str("reply_to")
                .unwrap()
                .array(3)
                .unwrap()
                .str(channel.as_str())
                .unwrap()
                .str(&creation.to_string())
                .unwrap()
                .u64(position.get())
                .unwrap();
        }
        Header::ContentType(value) | Header::Trace(value) | Header::Extension { value, .. } => {
            text(out, header.key(), value);
        }
    }
}

fn durability_name(value: Durability) -> &'static str {
    match value {
        Durability::Memory => "memory",
        Durability::Restart => "restart",
        Durability::Synced => "synced",
    }
}

fn durability(value: &str) -> Result<Durability, Error> {
    match value {
        "memory" => Ok(Durability::Memory),
        "restart" => Ok(Durability::Restart),
        "synced" => Ok(Durability::Synced),
        _ => Err(shape()),
    }
}

fn build(
    rid: u64,
    op: &str,
    count: usize,
    fields: impl FnOnce(&mut Encoder<Vec<u8>>),
    body: &[u8],
) -> Result<Bytes, Error> {
    encode_frame(rid, op, count, fields, body).map_err(|_| oversized())
}

fn text(out: &mut Encoder<Vec<u8>>, key: &str, value: &str) {
    out.str(key).unwrap().str(value).unwrap();
}
fn number(out: &mut Encoder<Vec<u8>>, key: &str, value: u64) {
    out.str(key).unwrap().u64(value).unwrap();
}
fn boolean(out: &mut Encoder<Vec<u8>>, key: &str, value: bool) {
    out.str(key).unwrap().bool(value).unwrap();
}
fn shape() -> Error {
    Error::refused("invalid call fields")
}
fn oversized() -> Error {
    Error::unavailable("reply exceeds the frame limit")
}
