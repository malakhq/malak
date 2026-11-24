use crate::journal_entry::durability_code;
use crate::{Durability, Grade, Header, Kind, ParticipantId, ParticipantName, SendContent};
use malak_layers::{SenderPeer, SenderTrace};
use malak_record::HashedBody;
use minicbor::{Encoder, encode::Write};
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub(crate) struct FieldBytes(SmallVec<[u8; 128]>);

impl FieldBytes {
    pub(crate) fn bytes(&self) -> &[u8] {
        self.0.as_slice()
    }
}

impl Write for FieldBytes {
    type Error = Infallible;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}

pub(crate) struct Fields<'a> {
    pub(crate) encoded: FieldBytes,
    pub(crate) body: HashedBody<'a>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) kind: Kind,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) sent_at: SystemTime,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) peer: Option<Arc<SenderPeer>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) lead_session: Option<crate::LeadSession>,
}

pub(crate) struct RecordSender<'a> {
    pub(crate) id: &'a ParticipantId,
    pub(crate) name: &'a ParticipantName,
    pub(crate) grade: Grade,
    pub(crate) peer: Option<&'a Arc<SenderPeer>>,
    pub(crate) agent_session_id: Option<&'a str>,
}

impl<'a> Fields<'a> {
    pub(crate) fn encode(
        message: &SendContent<'a>,
        body: HashedBody<'a>,
        sender: RecordSender<'_>,
        at: SystemTime,
        durability: Durability,
    ) -> Self {
        let mut fields = Encoder::new(FieldBytes::default());
        prefix(&mut fields, 9, message.kind, message.headers.len());
        encode_headers(&mut fields, &message.headers);

        let time = at.duration_since(UNIX_EPOCH).unwrap_or_default();
        fields
            .str(message.id.as_str())
            .unwrap()
            .str(sender.id.as_str())
            .unwrap()
            .str(sender.name.as_str())
            .unwrap()
            .u8(grade_code(sender.grade))
            .unwrap()
            .array(2)
            .unwrap()
            .u64(time.as_secs())
            .unwrap()
            .u32(time.subsec_nanos())
            .unwrap()
            .u8(durability_code(durability))
            .unwrap();

        let agent_session_id = sender.agent_session_id.filter(|_| sender.id.is_lead());
        match sender.peer {
            Some(peer) => crate::sender::write(&mut fields, &peer.trace, None, agent_session_id),
            // A present null identifies a Direct handle. Older records have no ninth field.
            None => {
                fields.null().unwrap();
            }
        }

        Self {
            encoded: fields.into_writer(),
            body,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            kind: message.kind,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            sent_at: at,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            peer: sender.peer.cloned(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            lead_session: if sender.id.is_lead() {
                sender
                    .peer
                    .and_then(|peer| peer.trace.session.clone())
                    .map(|process| crate::LeadSession {
                        process,
                        agent_session_id: agent_session_id.map(Into::into),
                    })
            } else {
                None
            },
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum RetryDigest {
    Legacy([u8; 32]),
    TraceAware([u8; 32]),
}

fn kind_code(kind: Kind) -> u8 {
    match kind {
        Kind::Task => 0,
        Kind::Report => 1,
        Kind::Note => 2,
    }
}

fn prefix<W: Write>(out: &mut Encoder<W>, fields: u64, kind: Kind, headers: usize)
where
    W::Error: std::fmt::Debug,
{
    out.array(fields)
        .unwrap()
        .u8(kind_code(kind))
        .unwrap()
        .array(headers as u64)
        .unwrap();
}

fn encode_headers<'a, W: Write>(
    fields: &mut Encoder<W>,
    headers: impl IntoIterator<Item = &'a Header>,
) where
    W::Error: std::fmt::Debug,
{
    for header in headers {
        fields.array(2).unwrap().str(header.key()).unwrap();
        match header {
            Header::ReplyTo {
                channel,
                creation,
                position,
            } => {
                fields
                    .array(3)
                    .unwrap()
                    .str(channel.as_str())
                    .unwrap()
                    .bytes(creation.as_bytes())
                    .unwrap()
                    .u64(position.get())
                    .unwrap();
            }
            Header::ContentType(text)
            | Header::Trace(text)
            | Header::Extension { value: text, .. } => {
                fields.str(text).unwrap();
            }
        }
    }
}

pub(crate) fn trace_header(headers: &[Header]) -> Option<&str> {
    headers.iter().find_map(|header| match header {
        Header::Trace(text) => Some(text.as_ref()),
        _ => None,
    })
}

struct DigestWriter(Sha256);

impl Write for DigestWriter {
    type Error = Infallible;

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Infallible> {
        self.0.update(bytes);
        Ok(())
    }
}

pub(crate) struct RetryInput<'a> {
    pub(crate) kind: Kind,
    pub(crate) headers: &'a [Header],
    pub(crate) body: &'a [u8; 32],
}

impl RetryInput<'_> {
    pub(crate) fn current(&self) -> RetryDigest {
        RetryDigest::TraceAware(self.digest(false, false))
    }

    pub(crate) fn matches(&self, saved: RetryDigest) -> bool {
        let trace = trace_header(self.headers);
        match saved {
            RetryDigest::TraceAware(saved) => {
                trace != Some("") && saved == self.digest(false, false)
            }
            RetryDigest::Legacy(saved) => {
                saved == self.digest(true, false)
                    || (trace.is_none() && saved == self.digest(true, true))
            }
        }
    }

    fn digest(&self, legacy: bool, insert_empty_trace: bool) -> [u8; 32] {
        let domain = if legacy {
            b"malak/retry/v2\0"
        } else {
            b"malak/retry/v3\0"
        };
        let keep = |header: &&Header| legacy || !matches!(header, Header::Trace(_));
        let count = self.headers.iter().filter(keep).count() + usize::from(insert_empty_trace);
        let mut out = Encoder::new(DigestWriter(Sha256::new_with_prefix(domain)));
        prefix(&mut out, if legacy { 8 } else { 2 }, self.kind, count);
        if insert_empty_trace {
            let at = self
                .headers
                .partition_point(|header| header.key() < "trace");
            encode_headers(&mut out, &self.headers[..at]);
            out.array(2).unwrap().str("trace").unwrap().str("").unwrap();
            encode_headers(&mut out, &self.headers[at..]);
        } else {
            encode_headers(&mut out, self.headers.iter().filter(keep));
        }
        let mut hash = out.into_writer().0;
        hash.update(self.body);
        hash.finalize().into()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn restored_retry_digest(
    verified: malak_record::Verified<'_>,
    message: &crate::Message,
) -> Option<RetryDigest> {
    let body = HashedBody::new(verified.body);
    let input = RetryInput {
        kind: message.kind,
        headers: &message.headers,
        body: body.digest(),
    };
    if trace_header(input.headers) != Some("") {
        return Some(input.current());
    }
    // Legacy retries include the exact prefix of the sealed metadata.
    let mut metadata = minicbor::Decoder::new(verified.metadata);
    metadata.array().ok()?;
    metadata.skip().ok()?;
    metadata.skip().ok()?;
    let mut hash = Sha256::new_with_prefix(b"malak/retry/v2\0");
    hash.update(&verified.metadata[..metadata.position()]);
    hash.update(body.digest());
    Some(RetryDigest::Legacy(hash.finalize().into()))
}

fn grade_code(grade: Grade) -> u8 {
    match grade {
        Grade::Proven => 1,
        Grade::LinkOnly => 0,
    }
}

pub(crate) fn decode(
    channel: &crate::ChannelName,
    creation: crate::CreationId,
    record: &malak_layers::Record,
    verified: malak_record::Verified<'_>,
) -> Option<crate::Message> {
    use crate::{HeaderValue, Message, Receipt};
    use minicbor::{Decoder, data::Type};
    use std::time::Duration;

    let mut decoded = Decoder::new(verified.metadata);
    let fields = decoded.array().ok()??;
    if !matches!(fields, 8 | 9) {
        return None;
    }
    let kind = match decoded.u8().ok()? {
        0 => Kind::Task,
        1 => Kind::Report,
        2 => Kind::Note,
        _ => return None,
    };
    let count = usize::try_from(decoded.array().ok()??).ok()?;
    if count > verified.metadata.len() / 3 {
        return None;
    }
    let mut headers = Vec::with_capacity(count);
    for _ in 0..count {
        if decoded.array().ok()? != Some(2) {
            return None;
        }
        let key = decoded.str().ok()?;
        let value = match decoded.datatype().ok()? {
            Type::Array if key == "reply_to" => {
                if decoded.array().ok()? != Some(3) {
                    return None;
                }
                HeaderValue::ReplyTo {
                    channel: decoded.str().ok()?,
                    creation: crate::CreationId::from_bytes(decoded.bytes().ok()?.try_into().ok()?),
                    position: decoded.u64().ok()?,
                }
            }
            _ => HeaderValue::Text(decoded.str().ok()?),
        };
        headers.push(Header::parse(key, value).ok()?);
    }

    let id = decoded.str().ok()?.parse().ok()?;
    let sender = decoded.str().ok()?.parse().ok()?;
    let sender_name = decoded.str().ok()?.parse().ok()?;
    let grade = match decoded.u8().ok()? {
        0 => Grade::LinkOnly,
        1 => Grade::Proven,
        _ => return None,
    };
    if decoded.array().ok()? != Some(2) {
        return None;
    }
    let seconds = decoded.u64().ok()?;
    let nanos = decoded.u32().ok()?;
    if nanos >= 1_000_000_000 {
        return None;
    }
    let sent_at = UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))?;
    let durability = match decoded.u8().ok()? {
        0 => Durability::Memory,
        1 => Durability::Restart,
        2 => Durability::Synced,
        _ => return None,
    };
    let (sender_trace, agent_session_id) = if fields == 9 {
        if decoded.datatype().ok()? == Type::Null {
            decoded.null().ok()?;
            (Some(SenderTrace::transport("in_process")), None)
        } else {
            let (trace, agent_session_id) = crate::sender::read(&mut decoded).ok()?;
            (Some(trace), agent_session_id)
        }
    } else {
        (None, None)
    };
    if decoded.position() != verified.metadata.len() {
        return None;
    }
    let trace = trace_header(&headers).and_then(crate::TraceId::parse);
    Some(Message {
        id,
        channel: channel.clone(),
        kind,
        headers,
        body: record.data.slice_ref(verified.body),
        sender,
        sender_name,
        sender_trace,
        agent_session_id,
        earlier_session: None,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        record_run: verified.stamp.run,
        grade,
        sent_at,
        receipt: Receipt {
            creation,
            position: record.position,
            durability,
            duplicate: false,
            trace,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use malak_record::{RecordKey, Stamp};

    #[test]
    fn a_legacy_record_reads_with_unknown_sender_trace() {
        // The eight fields written by releases before sender tracing.
        let metadata = b"\x88\x02\x80\x62id\x64lead\x64lead\x01\x82\x00\x00\x00";
        let channel = "lead".parse().unwrap();
        let creation = crate::CreationId::from_bytes([1; 16]);
        let name = malak_record::store_name(&channel, creation);
        let key = RecordKey::new(&[2; 32]);
        let record = key
            .seal(
                &name,
                crate::Position::FIRST,
                Stamp {
                    run: [3; 16],
                    attempt: 0,
                },
                metadata,
                b"legacy message",
            )
            .unwrap();

        let message = decode(
            &channel,
            creation,
            &record,
            key.verify(&name, &record).unwrap(),
        )
        .unwrap();

        assert_eq!(message.body.as_ref(), b"legacy message");
        assert!(message.sender_trace.is_none());
    }
}
