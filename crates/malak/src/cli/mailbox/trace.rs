//! Follow the stored messages that carry one untrusted trace label.

use super::sender::Sender as SenderTrace;
use super::{
    LiveBoard, follow_tail, history,
    messages::{RecordBody, message_body, read_record, reply_to, sent_at},
    model::{Address, Channel, Preview, Status},
    owner, owner_commands,
    plain::write_plain,
    render, status,
};
use crate::cli::{
    config::{Config, io_error},
    output::{self, display},
    participant::{Session, check_preview_board, json_message},
};
use malak::{Error, Outcome};
use serde_json::{Map, Value};
use std::{
    fmt::Write as _,
    io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

pub(in crate::cli) fn id(value: &str) -> Result<String, &'static str> {
    if value.is_empty() || value.len() > 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("trace must be 1 to 64 ASCII hexadecimal characters");
    }
    Ok(value.to_owned())
}

pub(in crate::cli) fn run(
    config: Option<PathBuf>,
    trace: String,
    plain: bool,
    json: bool,
) -> Result<(), Error> {
    check_preview_board(config.clone())?;
    if !plain && !json && output::Format::stdout().interactive() {
        return super::watch(config, None, false, false, Some(trace));
    }
    let mut owner = owner(config.clone())?;
    // Subscribe before the snapshot. A store between these calls can be
    // seen in both sources, with the same channel creation and position.
    let tail = (!json)
        .then(|| owner_commands::Tail::start(&mut owner, None))
        .transpose()?;
    let snapshot = if tail.is_some() {
        status(&mut super::owner(config.clone())?, false)?
    } else {
        status(&mut owner, false)?
    };
    let (mut session, viewer) = Session::open_preview(config.clone())
        .ok_or_else(|| Error::unavailable("could not open mailbox message previews"))?;
    let title = format!("Trace {trace}");
    output::diagnostic(&format!("{title} is a label any sender can set."), &[]);
    let Some(tail) = tail else {
        let records = retained(&snapshot, viewer.as_str());
        let found = load(&mut session, &records, &trace, || false)?;
        note_empty(found.is_empty());
        let mut stdout = io::stdout().lock();
        for message in found {
            let json = json_message(message.label, &message.body);
            if !write_plain(&mut stdout, &format!("{json}\n"))? {
                break;
            }
        }
        return Ok(());
    };

    let settings = Config::read(config)?.board;
    let (events, stores) = mpsc::channel();
    thread::Builder::new()
        .name("mailbox-trace-tail".into())
        .spawn(move || {
            let end = follow_tail(&settings, owner, tail, None, |event| {
                event
                    .address()
                    .is_none_or(|address| events.send(Ok(address)).is_ok())
            });
            if let Err(error) = end {
                let _ = events.send(Err(error));
            }
        })
        .map_err(io_error)?;
    let (updates, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("mailbox-trace-history".into())
        .spawn(move || {
            let end = follow(
                &mut session,
                &snapshot,
                viewer.as_str(),
                &trace,
                stores,
                &updates,
                || false,
            );
            let _ = updates.send(Update::End(end));
        })
        .map_err(io_error)?;
    stream(receiver)
}

struct Message {
    address: Address,
    sent_at: [u64; 2],
    label: Map<String, Value>,
    body: Vec<u8>,
}

impl Message {
    fn into_preview(self) -> (Address, Preview) {
        let reply_to = reply_to(&self.label);
        let preview = Preview {
            sender: self.label["sender"].as_str().unwrap_or_default().to_owned(),
            sender_trace: SenderTrace::from_label(&self.label),
            kind: self.label["kind"].as_str().unwrap_or("message").to_owned(),
            sent_at: self.sent_at,
            trace: self.label["headers"]["trace"].as_str().map(str::to_owned),
            text: None,
            cut_bytes: None,
            has_reply_to: reply_to.is_some(),
            reply_to,
            quote: None,
            body: Some(message_body(self.body)),
        };
        (self.address, preview)
    }

    fn block(self) -> String {
        let [seconds, nanos] = self.sent_at;
        let at_ms = seconds
            .saturating_mul(1000)
            .saturating_add(nanos / 1_000_000);
        let sender = self.label["sender_name"]
            .as_str()
            .or_else(|| self.label["sender"].as_str().map(output::sender_name_or_id))
            .unwrap_or("unknown sender");
        let kind = self.label["kind"].as_str().unwrap_or("message");
        let mut block = output::event(
            Some(at_ms),
            &format!(
                "{sender} sent a {kind} to {} #{}",
                self.address.channel, self.address.position,
            ),
        );
        if let Some(parent) = reply_to(&self.label) {
            write!(
                block,
                "  Reply to #{} {} (creation {})",
                parent.position,
                display(&parent.channel),
                display(&parent.creation)
            )
            .unwrap();
        }
        if let Some(origin) = SenderTrace::from_label(&self.label) {
            write!(block, "\n  {}", origin.line(sender, usize::MAX)).unwrap();
        }
        writeln!(
            block,
            "\n  {}",
            render::reply_address(&self.address, usize::MAX)
        )
        .unwrap();
        render::body_block(&mut block, &message_body(self.body).text);
        block.push('\n');
        block
    }
}

fn message(session: &mut Session, address: Address, trace: &str) -> Result<Option<Message>, Error> {
    let record = read_record(session, &address, RecordBody::Raw, |label| {
        label["headers"]["trace"].as_str() == Some(trace)
    });
    let Some((record, body)) = (match record {
        // Only messages visible with this participant's GET authority appear.
        Err(error) if error.outcome() == Outcome::Refused => return Ok(None),
        result => result?,
    }) else {
        return Ok(None);
    };
    let sent_at = sent_at(&record.label)
        .ok_or_else(|| Error::unavailable("invalid participant message timestamp"))?;
    Ok(Some(Message {
        address,
        sent_at,
        label: record.label,
        body,
    }))
}

fn retained<'a>(snapshot: &'a Status, viewer: &str) -> Vec<&'a Channel> {
    history::retained(snapshot, viewer).collect()
}

fn load(
    session: &mut Session,
    channels: &[&Channel],
    trace: &str,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<Message>, Error> {
    let mut messages = Vec::new();
    for address in channels.iter().flat_map(|channel| channel.addresses()) {
        if cancelled() {
            return Ok(Vec::new());
        }
        messages.extend(message(session, address, trace)?);
    }
    messages.sort_unstable_by(|a, b| {
        a.address
            .message_order(a.sent_at)
            .cmp(&b.address.message_order(b.sent_at))
    });
    Ok(messages)
}

enum Update {
    History(Vec<Message>),
    Message(Message),
    End(Result<(), Error>),
}

fn follow(
    session: &mut Session,
    snapshot: &Status,
    viewer: &str,
    trace: &str,
    stores: Receiver<Result<Address, Error>>,
    updates: &Sender<Update>,
    cancelled: impl Fn() -> bool,
) -> Result<(), Error> {
    let viewer = snapshot
        .participants
        .iter()
        .find(|person| person.id == viewer);
    let channels = viewer.map_or_else(Vec::new, |viewer| retained(snapshot, &viewer.id));
    let found = load(session, &channels, trace, cancelled)?;
    if updates.send(Update::History(found)).is_err() {
        return Ok(());
    }
    for address in stores {
        let address = address?;
        if !viewer.is_some_and(|viewer| viewer.can_get(&address.channel))
            || channels.iter().any(|channel| {
                channel.name == address.channel
                    && channel.creation == address.creation
                    && (channel.floor..channel.head).contains(&address.position)
            })
        {
            continue;
        }
        if let Some(message) = message(session, address, trace)?
            && updates.send(Update::Message(message)).is_err()
        {
            break;
        }
    }
    Ok(())
}

fn note_empty(empty: bool) {
    if empty {
        output::diagnostic("No messages found on this trace.", &[]);
    }
}

fn stream(receiver: Receiver<Update>) -> Result<(), Error> {
    let mut stdout = io::stdout().lock();
    for update in receiver {
        match update {
            Update::History(messages) => {
                note_empty(messages.is_empty());
                for message in messages {
                    if !write_plain(&mut stdout, &message.block())? {
                        return Ok(());
                    }
                }
            }
            Update::Message(message) => {
                if !write_plain(&mut stdout, &message.block())? {
                    return Ok(());
                }
            }
            Update::End(end) => return end,
        }
    }
    Ok(())
}

pub(super) struct LiveTrace {
    stores: Option<Sender<Result<Address, Error>>>,
    updates: Receiver<Update>,
    cancelled: Arc<AtomicBool>,
    pub id: String,
    pub messages: Vec<(Address, Preview)>,
    pub complete: bool,
    pub error: Option<String>,
}

impl LiveTrace {
    pub fn open(board: &LiveBoard, trace: String, stopped: bool) -> Result<Self, Error> {
        let (stores, events) = mpsc::channel();
        let (sender, updates) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        if !stopped {
            let trace = trace.clone();
            // Use the mailbox's existing owner connection. The new trace
            // already receives stores from its tail while history is loading.
            let (request, snapshot) = mpsc::sync_channel(1);
            let _ = board.snapshots.send(request);
            let door = board.previews.clone();
            let cancel = Arc::clone(&cancelled);
            thread::Builder::new()
                .name("mailbox-selected-trace".into())
                .spawn(move || {
                    let run = || {
                        let snapshot = snapshot.recv().map_err(|_| {
                            Error::unavailable("could not get a mailbox status snapshot")
                        })??;
                        let (mut session, viewer) = door.open().ok_or_else(|| {
                            Error::unavailable("could not open mailbox message previews")
                        })?;
                        follow(
                            &mut session,
                            &snapshot,
                            viewer.as_str(),
                            &trace,
                            events,
                            &sender,
                            || cancel.load(Ordering::Relaxed),
                        )
                    };
                    let _ = sender.send(Update::End(run()));
                })
                .map_err(io_error)?;
        }
        Ok(Self {
            stores: (!stopped).then_some(stores),
            updates,
            cancelled,
            id: trace,
            messages: Vec::new(),
            complete: false,
            error: stopped.then(|| "Board stopped. Trace history unavailable.".to_owned()),
        })
    }

    pub fn stored(&self, address: Address) {
        if let Some(stores) = &self.stores {
            let _ = stores.send(Ok(address));
        }
    }

    pub fn stop(&mut self) {
        self.stores = None;
    }

    pub fn refresh(&mut self) -> bool {
        let mut changed = false;
        for update in self.updates.try_iter().take(128) {
            changed = true;
            match update {
                Update::History(messages) => {
                    self.messages
                        .extend(messages.into_iter().map(Message::into_preview));
                    self.complete = true;
                }
                Update::Message(message) => self.messages.push(message.into_preview()),
                Update::End(end) => self.error = end.err().map(|error| error.to_string()),
            }
        }
        changed
    }
}

impl Drop for LiveTrace {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::mailbox::{
        model::{Model, tests::status},
        screen,
        view::{Tab, View},
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn message(channel: &str, ns: u64, body: &str) -> Message {
        Message {
            address: Address {
                channel: channel.into(),
                creation: "first".into(),
                position: 1,
            },
            sent_at: [9, ns],
            label: serde_json::json!({
                "sender": "lead", "kind": "note", "headers": {"trace": "c0ffee"}
            })
            .as_object()
            .unwrap()
            .clone(),
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_late_trace_match_appears_once_at_its_sent_at_position() {
        let (send, updates) = mpsc::channel();
        let mut trace = LiveTrace {
            stores: None,
            updates,
            cancelled: Arc::new(AtomicBool::new(false)),
            id: "c0ffee".into(),
            messages: Vec::new(),
            complete: false,
            error: None,
        };
        send.send(Update::History(vec![
            message("questions", 100, "Oldest retained"),
            message("lead", 900, "Newest retained"),
        ]))
        .unwrap();
        trace.refresh();

        send.send(Update::Message(message("orders", 500, "Late arrival")))
            .unwrap();
        trace.refresh();

        let model = Model::new(status(), None);
        let mut view = View::default();
        view.tab = Tab::Trace;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| {
                screen::draw(
                    frame,
                    &model,
                    "lanes",
                    10_000,
                    &mut view,
                    false,
                    Some(&trace),
                )
            })
            .unwrap();
        let output = terminal.backend().to_string();
        let newest = output.find("Newest retained").unwrap();
        let late = output.find("Late arrival").unwrap();
        let oldest = output.find("Oldest retained").unwrap();
        assert!(newest < late && late < oldest, "{output}");
        assert_eq!(output.matches("Late arrival").count(), 1);
    }
}
