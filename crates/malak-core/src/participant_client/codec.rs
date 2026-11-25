//! Encode the four requests and decode their replies: the client half of the
//! codec the board's connection loop uses for the same frames.

use crate::frame_fields::Fields;
use crate::message::{durability_name, parse_durability};
use crate::{
    CreationId, Error, GetReply, Grade, Header, HeaderValue, Kind, Message, Position, ReadChannel,
    ReadReply, Receipt, SendRequest, TraceId,
};
use bytes::Bytes;
use malak_proto::{Frame, encode_frame};
use minicbor::{Decoder, Encoder};
use std::time::{Duration, UNIX_EPOCH};

pub(super) fn send(rid: u64, request: &SendRequest<'_>) -> Result<Bytes, Error> {
    let durability = request.minimum_durability.map(durability_name);
    let count = 3 + usize::from(!request.headers.is_empty()) + usize::from(durability.is_some());
    encode_frame(
        rid,
        "SEND",
        count,
        |out| {
            text(out, "channel", request.channel);
            text(out, "id", request.id);
            text(out, "kind", request.kind.as_str());
            if !request.headers.is_empty() {
                out.str("headers")
                    .unwrap()
                    .map(request.headers.len() as u64)
                    .unwrap();
                for (key, value) in request.headers {
                    out.str(key).unwrap();
                    match value {
                        HeaderValue::Text(value) => {
                            out.str(value).unwrap();
                        }
                        HeaderValue::ReplyTo {
                            channel,
                            creation,
                            position,
                        } => {
                            out.array(3)
                                .unwrap()
                                .str(channel)
                                .unwrap()
                                .str(&creation.to_string())
                                .unwrap()
                                .u64(*position)
                                .unwrap();
                        }
                    }
                }
            }
            if let Some(durability) = durability {
                text(out, "minimum_durability", durability);
            }
        },
        request.body,
    )
    .map_err(|_| oversized())
}

pub(super) fn read(
    rid: u64,
    channels: &[&str],
    max: usize,
    wait: Duration,
) -> Result<Bytes, Error> {
    let limit = u64::try_from(max).ok().filter(|_| max != usize::MAX);
    encode_frame(
        rid,
        "READ",
        2 + usize::from(limit.is_some()),
        |out| {
            out.str("channels")
                .unwrap()
                .array(channels.len() as u64)
                .unwrap();
            for channel in channels {
                out.str(channel).unwrap();
            }
            if let Some(limit) = limit {
                number(out, "limit", limit);
            }
            number(
                out,
                "wait_ms",
                u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            );
        },
        &[],
    )
    .map_err(|_| oversized())
}

pub(super) fn get(rid: u64, channel: &str, position: u64) -> Result<Bytes, Error> {
    encode_frame(
        rid,
        "GET",
        2,
        |out| {
            text(out, "channel", channel);
            number(out, "position", position);
        },
        &[],
    )
    .map_err(|_| oversized())
}

pub(super) fn ack(
    rid: u64,
    channel: &str,
    creation: CreationId,
    position: u64,
) -> Result<Bytes, Error> {
    encode_frame(
        rid,
        "ACK",
        3,
        |out| {
            text(out, "channel", channel);
            number(out, "position", position);
            text(out, "creation", &creation.to_string());
        },
        &[],
    )
    .map_err(|_| oversized())
}

/// Turn an `ERR` reply into the error it carries.
pub(super) fn failure(reply: &Frame) -> Error {
    let fields = match Fields::new(reply) {
        Ok(fields) => fields,
        Err(_) => return invalid(),
    };
    let (Ok(outcome), Ok(reason)) = (fields.required("outcome"), fields.required("reason")) else {
        return invalid();
    };
    let reason = reason.to_owned();
    match outcome {
        "refused" => Error::refused(reason),
        "not_supported" => Error::not_supported(reason),
        "unavailable" => Error::unavailable(reason),
        _ => invalid(),
    }
}

/// Decode the receipt that closes a `SEND`.
pub(super) fn receipt(reply: &Frame) -> Result<Receipt, Error> {
    let fields = Fields::new(reply)?;
    Ok(Receipt {
        position: position(&fields)?,
        creation: creation(&fields)?,
        durability: durability(&fields)?,
        duplicate: fields
            .get("duplicate")
            .ok_or_else(invalid)?
            .bool()
            .map_err(|_| invalid())?,
        trace: fields
            .text("trace")?
            .map(|trace| TraceId::parse(trace).ok_or_else(invalid))
            .transpose()?,
    })
}

/// Decode one `MSG` reply into the message it carries.
pub(super) fn message(reply: &Frame) -> Result<Message, Error> {
    let fields = Fields::new(reply)?;
    let mut headers = Vec::new();
    if let Some(mut input) = fields.get("headers") {
        for _ in 0..input.map().map_err(|_| invalid())?.ok_or_else(invalid)? {
            headers.push(header(&mut input)?);
        }
    }
    let trace = headers.iter().find_map(|header| match header {
        Header::Trace(trace) => TraceId::parse(trace),
        _ => None,
    });
    let sender_trace = fields
        .get("sender_trace")
        .map(|mut input| crate::sender::read(&mut input).map(|(trace, _)| trace))
        .transpose()
        .map_err(|_| invalid())?;
    let (seconds, nanos) = {
        let mut input = fields.get("sent_at").ok_or_else(invalid)?;
        if input.array().map_err(|_| invalid())? != Some(2) {
            return Err(invalid());
        }
        (
            input.u64().map_err(|_| invalid())?,
            input.u32().map_err(|_| invalid())?,
        )
    };
    Ok(Message {
        id: fields.required("id")?.parse()?,
        channel: fields.required("channel")?.parse()?,
        kind: fields.required("kind")?.parse::<Kind>()?,
        headers,
        body: reply.body().to_vec().into(),
        sender: fields.required("sender")?.parse()?,
        sender_name: fields.required("sender_name")?.parse()?,
        sender_trace,
        agent_session_id: None,
        earlier_session: None,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        record_run: [0; 16],
        grade: match fields.required("grade")? {
            "proven" => Grade::Proven,
            "link_only" => Grade::LinkOnly,
            _ => return Err(invalid()),
        },
        sent_at: UNIX_EPOCH + Duration::new(seconds, nanos),
        receipt: Receipt {
            position: position(&fields)?,
            creation: creation(&fields)?,
            durability: durability(&fields)?,
            duplicate: false,
            trace,
        },
    })
}

fn header(input: &mut Decoder<'_>) -> Result<Header, Error> {
    let key = input.str().map_err(|_| invalid())?;
    if key == "reply_to" {
        if input.array().map_err(|_| invalid())? != Some(3) {
            return Err(invalid());
        }
        Header::parse(
            key,
            HeaderValue::ReplyTo {
                channel: input.str().map_err(|_| invalid())?,
                creation: input.str().map_err(|_| invalid())?.parse()?,
                position: input.u64().map_err(|_| invalid())?,
            },
        )
    } else {
        Header::parse(key, HeaderValue::Text(input.str().map_err(|_| invalid())?))
    }
}

/// Decode the `OK` that closes a `READ`, adding the messages that preceded it.
pub(super) fn read_reply(reply: &Frame, messages: Vec<Message>) -> Result<ReadReply, Error> {
    let fields = Fields::new(reply)?;
    let held_back = fields
        .get("held_back")
        .ok_or_else(invalid)?
        .bool()
        .map_err(|_| invalid())?;
    let mut input = fields.get("channels").ok_or_else(invalid)?;
    let mut channels = Vec::new();
    for _ in 0..input.array().map_err(|_| invalid())?.ok_or_else(invalid)? {
        let count = input.map().map_err(|_| invalid())?.ok_or_else(invalid)?;
        let (mut channel, mut voids, mut creation, mut trimmed_before) = (None, 0, None, None);
        for _ in 0..count {
            match input.str().map_err(|_| invalid())? {
                "channel" => channel = Some(input.str().map_err(|_| invalid())?.parse()?),
                "voids" => voids = input.u64().map_err(|_| invalid())?,
                "creation" => creation = Some(input.str().map_err(|_| invalid())?.parse()?),
                "trimmed_before" => trimmed_before = Some(input.u64().map_err(|_| invalid())?),
                _ => input.skip().map_err(|_| invalid())?,
            }
        }
        channels.push(ReadChannel {
            channel: channel.ok_or_else(invalid)?,
            creation,
            trimmed_before,
            voids,
        });
    }
    Ok(ReadReply {
        messages,
        channels,
        held_back,
    })
}

/// Decode the `OK` that closes a `GET`, using the message that preceded it.
pub(super) fn get_reply(reply: &Frame, found: Option<Message>) -> Result<GetReply, Error> {
    let fields = Fields::new(reply)?;
    let creation = || creation(&fields);
    Ok(match fields.required("result")? {
        "message" => GetReply::Message(found.ok_or_else(invalid)?),
        "trimmed" => GetReply::Trimmed {
            creation: creation()?,
            before: fields.number("trimmed_before")?.ok_or_else(invalid)?,
        },
        "void" => GetReply::Void {
            creation: creation()?,
        },
        "never_written" => GetReply::NeverWritten {
            creation: creation()?,
        },
        "absent" => GetReply::Absent,
        _ => return Err(invalid()),
    })
}

fn position(fields: &Fields<'_>) -> Result<Position, Error> {
    Position::try_from(fields.number("position")?.ok_or_else(invalid)?)
}

fn creation(fields: &Fields<'_>) -> Result<CreationId, Error> {
    fields.required("creation")?.parse()
}

fn durability(fields: &Fields<'_>) -> Result<crate::Durability, Error> {
    parse_durability(fields.required("durability")?).ok_or_else(invalid)
}

fn text(out: &mut Encoder<Vec<u8>>, key: &str, value: &str) {
    out.str(key).unwrap().str(value).unwrap();
}

fn number(out: &mut Encoder<Vec<u8>>, key: &str, value: u64) {
    out.str(key).unwrap().u64(value).unwrap();
}

fn oversized() -> Error {
    Error::refused("request exceeds the frame limit")
}

fn invalid() -> Error {
    Error::unavailable("invalid reply from the board")
}
