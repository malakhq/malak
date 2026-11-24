//! Read authorized message labels, previews, bodies and reply targets.

use super::{
    PreviewDoor, REFRESH, Update,
    model::{self, Address, Body, Participant, Preview, Quote},
    sender::Sender,
    text, trace,
};
use crate::cli::{
    client_transport::{LINK_DROPPED, link_failed, reconnect},
    config::io_error,
    participant::{ParticipantRecord, Session},
};
use malak::Error;
use serde_json::{Map, Value};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Instant;

pub(super) fn readable_channels<'a>(
    channels: impl Iterator<Item = &'a model::Channel> + Clone,
    viewer: &'a model::Participant,
) -> impl Iterator<Item = &'a model::Channel> + Clone {
    channels.filter(move |channel| viewer.can_get(&channel.name))
}

pub(super) struct PreviewRequest {
    pub address: Address,
    pub viewer: Participant,
}

pub(super) fn preview_worker(
    tx: SyncSender<Update>,
    door: PreviewDoor,
) -> Result<SyncSender<PreviewRequest>, Error> {
    let (send, receive) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("mailbox-preview".into())
        .spawn(move || {
            let Some((session, id)) = door.open() else {
                let _ = tx.send(Update::PreviewsUnavailable);
                return;
            };
            let mut previews = LivePreviews::new(door, session, id);
            while let Ok(PreviewRequest { address, viewer }) = receive.recv() {
                let preview = previews.fetch(&viewer, &address);
                if tx.send(Update::Preview(address, preview)).is_err() {
                    break;
                }
            }
        })
        .map_err(io_error)?;
    Ok(send)
}

pub(super) struct LivePreviews {
    door: PreviewDoor,
    session: Option<Session>,
    pub id: malak::ParticipantId,
}

impl LivePreviews {
    pub fn new(door: PreviewDoor, session: Session, id: malak::ParticipantId) -> Self {
        Self {
            door,
            session: Some(session),
            id,
        }
    }

    pub fn fetch(
        &mut self,
        viewer: &Participant,
        address: &Address,
    ) -> Result<Option<Preview>, Error> {
        if self.id.as_str() != viewer.id {
            return Ok(None);
        }
        fetch_with(viewer, address, |address| {
            self.read_with_body(address, RecordBody::Full)
        })
    }

    pub(super) fn read_with_body(
        &mut self,
        address: &Address,
        body: RecordBody,
    ) -> Result<Option<Preview>, Error> {
        match self.read(address, body) {
            Err(error) if link_failed(&error) => {
                reconnect(Instant::now() + REFRESH, |_| self.read(address, body))
            }
            result => result,
        }
    }

    fn read(&mut self, address: &Address, body: RecordBody) -> Result<Option<Preview>, Error> {
        if self.session.is_none() {
            let (session, id) = self
                .door
                .open()
                .ok_or_else(|| Error::unavailable(LINK_DROPPED))?;
            if id != self.id {
                return Err(Error::refused(
                    "mailbox preview handover changed participant",
                ));
            }
            self.session = Some(session);
        }
        let session = self.session.as_mut().expect("preview session is open");
        let result = read_message(session, address, body);
        if !session.is_connected() {
            self.session = None;
        }
        result
    }
}

pub(super) fn fetch(
    session: &mut Session,
    viewer: &Participant,
    address: &Address,
) -> Result<Option<Preview>, Error> {
    fetch_with(viewer, address, |address| {
        read_message(session, address, RecordBody::Full)
    })
}

fn fetch_with(
    viewer: &Participant,
    address: &Address,
    mut read: impl FnMut(&Address) -> Result<Option<Preview>, Error>,
) -> Result<Option<Preview>, Error> {
    if !viewer.can_get(&address.channel) {
        return Ok(None);
    }
    let Some(mut preview) = read(address)? else {
        return Ok(None);
    };
    add_quote_with(viewer, &mut preview, read);
    Ok(Some(preview))
}

pub(super) fn add_quote(session: &mut Session, viewer: &Participant, preview: &mut Preview) {
    add_quote_with(viewer, preview, |target| {
        read_message(session, target, RecordBody::Full)
    });
}

fn add_quote_with(
    viewer: &Participant,
    preview: &mut Preview,
    read: impl FnOnce(&Address) -> Result<Option<Preview>, Error>,
) {
    preview.quote = preview
        .reply_to
        .as_ref()
        .filter(|target| viewer.can_get(&target.channel))
        .and_then(|target| read(target).ok().flatten())
        .map(|target| {
            let at_ms = target.at_ms();
            Quote {
                excerpt: target.body_text().map(text::quote),
                sender: target.sender,
                kind: target.kind,
                at_ms,
            }
        });
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordBody {
    Omit,
    Preview,
    Full,
    Raw,
}

pub(super) fn read_message(
    session: &mut Session,
    address: &Address,
    text: RecordBody,
) -> Result<Option<Preview>, Error> {
    retry_rate_limit(|| read_message_once(session, address, text))
}

fn read_message_once(
    session: &mut Session,
    address: &Address,
    text: RecordBody,
) -> Result<Option<Preview>, Error> {
    let full = text == RecordBody::Full;
    let Some((record, bytes)) = read_record_once(session, address, text, |_| true)? else {
        return Ok(None);
    };
    let mut preview = preview_record(record, address)?;
    if full {
        preview.body = Some(message_body(bytes));
    }
    Ok(Some(preview))
}

pub(super) fn read_record(
    session: &mut Session,
    address: &Address,
    text: RecordBody,
    matches: impl Fn(&Map<String, Value>) -> bool,
) -> Result<Option<(ParticipantRecord, Vec<u8>)>, Error> {
    retry_rate_limit(|| read_record_once(session, address, text, &matches))
}

fn read_record_once(
    session: &mut Session,
    address: &Address,
    text: RecordBody,
    matches: impl Fn(&Map<String, Value>) -> bool,
) -> Result<Option<(ParticipantRecord, Vec<u8>)>, Error> {
    session.get_record_matching(
        &address.channel,
        &address.creation,
        address.position,
        matches!(text, RecordBody::Preview | RecordBody::Full),
        matches!(text, RecordBody::Full | RecordBody::Raw),
        matches,
    )
}

pub(super) fn message_body(bytes: Vec<u8>) -> Body {
    Body {
        text: String::from_utf8(bytes)
            .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned()),
    }
}

fn retry_rate_limit<T>(mut call: impl FnMut() -> Result<T, Error>) -> Result<T, Error> {
    loop {
        match call() {
            Err(error) if error.to_string() == "participant call rate exceeded" => {
                thread::sleep(REFRESH);
            }
            result => return result,
        }
    }
}

fn preview_record(record: ParticipantRecord, address: &Address) -> Result<Preview, Error> {
    let label = record.label;
    let field = || Error::unavailable("invalid participant message label");
    let sent_at = sent_at(&label).ok_or_else(field)?;
    let reply_to = reply_to(&label);
    Ok(Preview {
        sender: label["sender"].as_str().ok_or_else(field)?.into(),
        sender_trace: Sender::from_label(&label),
        kind: label["kind"].as_str().ok_or_else(field)?.into(),
        sent_at,
        trace: label["headers"]["trace"]
            .as_str()
            .and_then(|value| trace::id(value).ok()),
        text: record.preview,
        cut_bytes: record.preview_cut.then_some(record.body_bytes),
        has_reply_to: reply_to.is_some(),
        reply_to: reply_to.filter(|target| {
            target.channel != address.channel
                || target.creation == address.creation && target.position < address.position
        }),
        quote: None,
        body: None,
    })
}

pub(super) fn sent_at(label: &Map<String, Value>) -> Option<[u64; 2]> {
    let time = label.get("sent_at")?;
    Some([time[0].as_u64()?, time[1].as_u64()?])
}

pub(super) fn reply_to(label: &Map<String, Value>) -> Option<Address> {
    let reply = label.get("headers")?.get("reply_to")?;
    Some(Address {
        channel: reply[0].as_str()?.into(),
        creation: reply[1].as_str()?.into(),
        position: reply[2].as_u64()?,
    })
}
