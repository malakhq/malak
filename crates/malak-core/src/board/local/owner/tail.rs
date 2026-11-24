use super::{
    LocalOwner, State, cancel_target, closed, outcome_frame, outcome_name, owner_fields,
    owner_protocol_error,
};
use crate::Error;
use crate::board::events::TailWriter;
use crate::board::local::Protected;
use crate::link::{Segments, Wire};
use bytes::Bytes;
use malak_layers::{Event, EventKind, Transport};
use malak_proto::{
    Encrypted, MAX_CONTENT, OwnerOperation, OwnerRequest, OwnerTicket, SegmentKind, encode_frame,
};
use parking_lot::Mutex;
use std::sync::{Arc, mpsc};
use std::time::Instant;

impl<T: Transport + 'static> LocalOwner<T>
where
    T::Connection: 'static,
{
    pub(super) fn tail(&mut self, request: &OwnerRequest) -> Result<(), Error> {
        let writer = match watch(&self.link.board, request) {
            Ok(writer) => writer,
            Err(error) => return self.reply(request.ticket, Err(error)),
        };
        let watcher = writer.watcher();

        let mut state = self.state.take().ok_or_else(closed)?;
        let protected = self.link.protected.take().ok_or_else(closed)?;
        state
            .protocol
            .start_tail(request.ticket)
            .map_err(owner_protocol_error)?;
        let opened = self.link.opened;
        let deadline = opened + state.protocol.deadline();
        let session = Arc::new(TailLink {
            wire: self.link.wire.clone(),
            encrypted: Mutex::new(protected.encrypted),
            output: Mutex::new(()),
        });
        let sink = session.clone();
        let ticket = request.ticket;
        let (finished, completion) = mpsc::sync_channel(1);
        let send = std::thread::Builder::new()
            .name("malak-tail".into())
            .spawn(move || {
                let result = writer.run(|event, missed| {
                    event_frame(ticket, &event, missed).and_then(|frame| sink.send(&frame))
                });
                if result.is_err() {
                    sink.wire.close();
                }
                drop(sink);
                let _ = finished.send(());
            });
        if send.is_err() {
            self.link.wire.close();
            return Err(Error::unavailable("could not start owner tail"));
        }
        let reading = session.clone();
        let cancellation = self.link.settings.owner.workers.until(deadline, move || {
            let mut segments = protected.segments;
            let control = reading.controls(&mut state, &mut segments, opened, ticket)?;
            drop(reading);
            Ok((state, segments, control))
        });
        watcher.close();
        let (state, segments, control) = match cancellation {
            Ok(cancelled) => cancelled,
            Err(error) => {
                // A close can race an in-flight write in a transport. This
                // connection has ended, and the writer owns everything it uses.
                self.link.wire.close();
                return Err(error);
            }
        };
        if completion
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_err()
        {
            self.link.wire.close();
            return Err(Error::unavailable("owner tail writer did not finish"));
        }
        let session = Arc::try_unwrap(session).map_err(|_| closed())?;
        self.link.protected = Some(Protected {
            segments,
            encrypted: session.encrypted.into_inner(),
            replied: protected.replied,
        });
        self.state = Some(state);
        self.reply(ticket, Ok(()))?;
        self.reply(control, Ok(()))
    }
}

struct TailLink<T: Transport> {
    wire: Arc<Wire<T>>,
    encrypted: Mutex<Encrypted>,
    output: Mutex<()>,
}

impl<T: Transport> TailLink<T> {
    fn send(&self, frame: &[u8]) -> Result<(), Error> {
        let _output = self.output.lock();
        for chunk in frame.chunks(MAX_CONTENT) {
            let packet = Segments::packet(&mut self.encrypted.lock(), chunk)?;
            self.wire.write_with(&packet, |_| Ok(()))?;
        }
        Ok(())
    }

    fn controls(
        &self,
        state: &mut State,
        segments: &mut Segments,
        opened: Instant,
        tail: OwnerTicket,
    ) -> Result<OwnerTicket, Error> {
        loop {
            let Some(request) = state
                .protocol
                .receive(&mut state.plaintext, opened.elapsed())
                .map_err(owner_protocol_error)?
            else {
                let encrypted =
                    segments.receive_with(&self.wire, SegmentKind::Protected, |at| {
                        state
                            .protocol
                            .peer_progress(at.duration_since(opened))
                            .map_err(owner_protocol_error)
                    })?;
                state.plaintext = Segments::decrypt(&mut self.encrypted.lock(), &encrypted)?;
                continue;
            };
            let outcome = match request.operation {
                OwnerOperation::Ping => owner_fields(&request.frame).map(|_| ()),
                OwnerOperation::Cancel => match cancel_target(&request.frame) {
                    Ok(target) if state.protocol.cancellable(target) == Some(tail) => {
                        return Ok(request.ticket);
                    }
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                },
                _ => return Err(Error::refused("owner request received during tail")),
            };
            self.send(&outcome_frame(request.ticket, outcome)?)?;
            state
                .protocol
                .reply_sent(request.ticket, opened.elapsed())
                .map_err(owner_protocol_error)?;
        }
    }
}

pub(super) fn watch(
    board: &crate::board::Board,
    request: &OwnerRequest,
) -> Result<TailWriter, Error> {
    owner_fields(&request.frame)
        .and_then(|fields| fields.text("pattern")?.map(str::parse).transpose())
        .and_then(|pattern| board.events.stream(pattern))
}

pub(super) fn event_frame(ticket: OwnerTicket, event: &Event, missed: u64) -> Result<Bytes, Error> {
    let counted = matches!(event.kind, EventKind::Denied | EventKind::LimitHit);
    let fields = 4
        + usize::from(event.reason.is_some())
        + usize::from(event.message_kind.is_some())
        + usize::from(event.outcome.is_some())
        + usize::from(counted)
        + usize::from(event.operation.is_some())
        + usize::from(event.participant.is_some())
        + usize::from(event.sender_trace.is_some())
        + usize::from(event.owner_target.is_some())
        + usize::from(event.channel.is_some())
        + usize::from(event.record.is_some()) * 2;
    encode_frame(
        ticket.rid(),
        "EVENT",
        fields,
        |output| {
            output.str("version").unwrap().u8(1).unwrap();
            output
                .str("at_ms")
                .unwrap()
                .u64(crate::epoch_millis(event.at))
                .unwrap();
            output.str("event").unwrap().str(kind(event.kind)).unwrap();
            if let Some(reason) = &event.reason {
                output.str("reason").unwrap().str(reason).unwrap();
            }
            if let Some(kind) = event.message_kind {
                output.str("kind").unwrap().str(kind).unwrap();
            }
            if let Some(outcome) = event.outcome {
                output
                    .str("outcome")
                    .unwrap()
                    .str(outcome_name(outcome))
                    .unwrap();
            }
            output.str("missed").unwrap().u64(missed).unwrap();
            if counted {
                output.str("count").unwrap().u64(event.count).unwrap();
            }
            if let Some(operation) = event.operation {
                output.str("operation").unwrap().str(operation).unwrap();
            }
            if let Some(participant) = &event.participant {
                output.str("id").unwrap().str(participant.as_str()).unwrap();
            }
            if let Some(sender) = &event.sender_trace {
                output.str("sender_trace").unwrap();
                crate::sender::write(output, sender, event.earlier_session.as_ref(), None);
            }
            if let Some(target) = &event.owner_target {
                output.str("target").unwrap().str(target).unwrap();
            }
            if let Some(channel) = &event.channel {
                output
                    .str("channel")
                    .unwrap()
                    .str(channel.as_str())
                    .unwrap();
            }
            if let Some((creation, position)) = event.record {
                output
                    .str("creation")
                    .unwrap()
                    .str(&creation.to_string())
                    .unwrap();
                output.str("position").unwrap().u64(position.get()).unwrap();
            }
        },
        &[],
    )
    .map_err(|_| Error::unavailable("owner event does not fit"))
}

fn kind(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Owner => "owner",
        EventKind::Stored => "stored",
        EventKind::Read => "read",
        EventKind::Acked => "acked",
        EventKind::Trimmed => "trimmed",
        EventKind::StoreDamaged => "store_damaged",
        EventKind::TailCut => "tail_cut",
        EventKind::TailTampered => "tail_tampered",
        EventKind::Denied => "denied",
        EventKind::LimitHit => "limit_hit",
        _ => "activity",
    }
}
