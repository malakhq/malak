use super::attach::{Attached, LocalParticipant};
use crate::board::{CallProof, Handle, read::ReadSink};
use crate::link::{Segments, Wire};
use crate::workers::Workers;
use crate::{Action, Error, GetReply, Message, ParticipantName};
use bytes::Bytes;
use malak_layers::Transport;
use malak_proto::{
    Connection, ConnectionError, Encrypted, Entrance, Input, MAX_CONTENT, Operation, Placement,
    Request, SegmentKind, Ticket,
};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod codec;

fn call_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Send => "send",
        Operation::Read => "read",
        Operation::Get => "get",
        Operation::Ack => "ack",
        _ => "data",
    }
}

const CHECK_AUTHORITY: Duration = Duration::from_secs(1);

impl<T: Transport + 'static> LocalParticipant<T>
where
    T::Connection: 'static,
{
    /// Serve protected participant requests until the link closes.
    ///
    /// The reader and writer run independently so control frames can be
    /// processed while a data request is waiting.
    pub fn serve(mut self) -> Result<(), Error> {
        self.check_valid()?;
        let Attached {
            mut link,
            connection,
            place: _place,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
                count: _count,
        } = self.attached.take().ok_or_else(closed)?;
        let protected = link.protected.take().ok_or_else(closed)?;
        let veto = (link.entrance == Entrance::General && link.settings.proves.has_veto)
            .then(|| link.settings.mechanism.clone());
        let session = Arc::new(Active {
            caller: Handle {
                board: link.board.clone(),
                id: self.identity.id.clone(),
                proof: Some(CallProof {
                    identity: Arc::new(self.identity),
                    veto,
                    peer: link.wire.sender().clone(),
                }),
            },
            connection: Mutex::new(connection),
            progress: Condvar::new(),
            wire: link.wire.clone(),
            encrypted: Mutex::new(protected.encrypted),
            opened: link.opened,
            requests: Workers::named("malak-call"),
            writes: Workers::named("malak-reply"),
        });

        std::thread::scope(|tasks| {
            let sending = session.clone();
            let writer = std::thread::Builder::new()
                .name("malak-writer".into())
                .spawn_scoped(tasks, move || {
                    let _ = sending.write();
                    sending.close();
                })
                .map_err(|_| Error::unavailable("could not start the reply writer"))?;

            let outcome = session.receive(protected.segments);
            session.close();
            writer
                .join()
                .map_err(|_| Error::unavailable("reply writer stopped"))?;
            outcome
        })
    }
}

struct Active<T: Transport> {
    caller: Handle,
    connection: Mutex<Connection>,
    progress: Condvar,
    wire: Arc<Wire<T>>,
    encrypted: Mutex<Encrypted>,
    opened: Instant,
    requests: Workers,
    writes: Workers,
}

impl<T: Transport + 'static> Active<T>
where
    T::Connection: 'static,
{
    fn close(&self) {
        self.connection.lock().close();
        self.wire.close();
        self.progress.notify_all();
        self.caller.board.changes.notify();
    }

    fn receive(self: &Arc<Self>, mut segments: Segments) -> Result<(), Error> {
        let mut buffer = Bytes::new();
        loop {
            let result = {
                let mut state = self.connection.lock();
                state.tick(self.opened.elapsed()).map_err(protocol)?;
                while !state.can_read() {
                    self.progress.wait(&mut state);
                    state.tick(self.opened.elapsed()).map_err(protocol)?;
                }
                state
                    .receive(&mut buffer, self.opened.elapsed())
                    .map_err(protocol)?
            };
            match result {
                Input::Paused => continue,
                Input::Partial => {
                    let segment =
                        segments.receive_with(&self.wire, SegmentKind::Protected, |at| {
                            self.connection
                                .lock()
                                .peer_progress(at.duration_since(self.opened))
                                .map_err(protocol)
                        })?;
                    buffer = Segments::decrypt(&mut self.encrypted.lock(), &segment)?;
                }
                Input::Unavailable(ticket) => {
                    let result = self.caller.call(
                        "data",
                        || None,
                        |_| {
                            Err(Error::unavailable("connection has no data request place")
                                .at_limit())
                        },
                    );
                    self.finish(ticket, result)?;
                }
                Input::Request(request) => match request.operation {
                    Operation::Ping => {
                        self.finish(request.ticket, codec::ok(request.ticket.rid()))?
                    }
                    Operation::Cancel => self.cancel(request)?,
                    _ => {
                        let active = self.clone();
                        let ticket = request.ticket;
                        let operation = call_name(request.operation);
                        if let Err(error) = self.requests.dispatch(move || {
                            let outcome = active.caller.call(
                                operation,
                                || codec::channel(&request.frame),
                                |name| active.run(name, &request),
                            );
                            if active.finish(request.ticket, outcome).is_err() {
                                active.close();
                            }
                        }) {
                            self.finish(
                                ticket,
                                self.caller.call(operation, || None, |_| Err(error)),
                            )?;
                        }
                    }
                },
            }
        }
    }

    fn run(&self, name: &ParticipantName, request: &Request) -> Result<Bytes, Error> {
        let rid = request.ticket.rid();
        match codec::decode(request.operation, &request.frame)? {
            codec::Call::Send(send) => {
                let stored =
                    self.caller
                        .send_counted(name, send.request(), send.agent_session_id)?;
                codec::sent(rid, stored)
            }
            codec::Call::Read {
                channels,
                limit,
                wait,
            } => {
                let mut output = Output {
                    active: self,
                    ticket: request.ticket,
                    prepared: Bytes::new(),
                    remaining: self.caller.board.limits.read_reply_bytes,
                };
                let reply = self
                    .caller
                    .read_counted(&channels, limit, wait, Some(&mut output))?;
                codec::read(rid, &reply)
            }
            codec::Call::Get { channel, position } => {
                let answer = self.caller.get_counted(channel, position)?;
                if let GetReply::Message(message) = &answer {
                    let frame = codec::message(rid, message, 0)?;
                    let mut table = self.caller.board.table.check();
                    self.caller.validate(&mut table.authority)?;
                    table
                        .authority
                        .allow(self.caller.id(), Action::Get, &message.channel)?;
                    let current = table
                        .channels
                        .get(&message.channel)
                        .is_some_and(|channel| channel.creation == message.receipt.creation);
                    if !current {
                        return Err(Error::unavailable("channel creation no longer matches"));
                    }
                    if !self.enqueue_message(request.ticket, frame)? {
                        return Err(
                            Error::unavailable("message does not fit the outgoing queue")
                                .at_limit(),
                        );
                    }
                }
                codec::got(rid, channel, position, &answer)
            }
            codec::Call::Ack {
                channel,
                creation,
                position,
            } => {
                self.caller.ack_counted(channel, creation, position)?;
                codec::ok(rid)
            }
        }
    }

    fn enqueue_message(&self, ticket: Ticket, frame: Bytes) -> Result<bool, Error> {
        let mut state = self.connection.lock();
        let placement = state
            .enqueue_message(ticket, frame, self.opened.elapsed())
            .map_err(protocol)?;
        if placement == Placement::Placed {
            self.progress.notify_all();
        }
        Ok(placement == Placement::Placed)
    }

    fn finish(&self, ticket: Ticket, result: Result<Bytes, Error>) -> Result<(), Error> {
        let frame = match result {
            Ok(reply) => reply,
            Err(error) => codec::error(ticket.rid(), &error)?,
        };
        let mut table = self.caller.board.table.check();
        self.caller.validate(&mut table.authority)?;
        let mut state = self.connection.lock();
        state
            .enqueue_closing(ticket, frame, self.opened.elapsed())
            .map_err(protocol)?;
        self.progress.notify_all();
        Ok(())
    }

    fn cancel(&self, request: Request) -> Result<(), Error> {
        let target = crate::frame_fields::Fields::new(&request.frame)
            .and_then(|fields| fields.number("target"))
            .and_then(|target| target.ok_or_else(|| Error::refused("cancel target is missing")));
        let target = match target {
            Ok(target) => target,
            Err(error) => return self.finish(request.ticket, Err(error)),
        };

        let mut table = self.caller.board.table.check();
        self.caller.validate(&mut table.authority)?;
        let mut connection = self.connection.lock();
        let read = connection.cancellable(target);
        if let Some(read) = read {
            let reply = codec::cancelled(read.rid(), "cancelled", true)?;
            connection
                .enqueue_closing(read, reply, self.opened.elapsed())
                .map_err(protocol)?;
        }
        let reply = codec::cancelled(request.ticket.rid(), "found", read.is_some())?;
        connection
            .enqueue_closing(request.ticket, reply, self.opened.elapsed())
            .map_err(protocol)?;
        self.progress.notify_all();
        drop(connection);
        drop(table);
        self.caller.board.changes.notify();
        Ok(())
    }

    fn write(self: &Arc<Self>) -> Result<(), Error> {
        loop {
            let plaintext = self.next_output()?;
            let packet = Segments::packet(&mut self.encrypted.lock(), &plaintext)?;
            let active = self.clone();
            let delivered = plaintext.len();
            self.writes.until_with(
                || self.opened + self.connection.lock().deadline(),
                move || {
                    active.wire.write_with(&packet, |written| {
                        active
                            .connection
                            .lock()
                            .wrote(0, written, active.opened.elapsed())
                            .map_err(protocol)
                    })?;
                    let mut connection = active.connection.lock();
                    connection
                        .wrote(delivered, delivered, active.opened.elapsed())
                        .map_err(protocol)?;
                    active.progress.notify_all();
                    drop(connection);
                    active.caller.board.changes.notify();
                    Ok(())
                },
            )?;
        }
    }

    fn next_output(&self) -> Result<Bytes, Error> {
        loop {
            {
                let mut table = self.caller.board.table.check();
                self.caller.validate(&mut table.authority)?;
                let mut connection = self.connection.lock();
                connection.tick(self.opened.elapsed()).map_err(protocol)?;
                if let Some(bytes) = connection.outgoing() {
                    return Ok(Bytes::copy_from_slice(
                        &bytes[..bytes.len().min(MAX_CONTENT)],
                    ));
                }
            }

            let mut connection = self.connection.lock();
            connection.tick(self.opened.elapsed()).map_err(protocol)?;
            if connection.outgoing().is_none() {
                let wake =
                    (self.opened + connection.deadline()).min(Instant::now() + CHECK_AUTHORITY);
                self.progress.wait_until(&mut connection, wake);
            }
        }
    }
}

struct Output<'a, T: Transport> {
    active: &'a Active<T>,
    ticket: Ticket,
    prepared: Bytes,
    remaining: usize,
}

impl<T: Transport + 'static> ReadSink for Output<'_, T>
where
    T::Connection: 'static,
{
    fn open(&self) -> bool {
        self.active.connection.lock().is_open(self.ticket)
    }

    fn prepare(&mut self, message: &Message, lost_before: u64) -> Result<(), Error> {
        self.prepared = codec::message(self.ticket.rid(), message, lost_before)?;
        Ok(())
    }

    fn give(&mut self, _: &Message) -> Result<bool, Error> {
        let size = self.prepared.len();
        if size > self.remaining {
            return Ok(false);
        }
        let placed = self
            .active
            .enqueue_message(self.ticket, std::mem::take(&mut self.prepared))?;
        if placed {
            self.remaining -= size;
        }
        Ok(placed)
    }
}

fn protocol(_: ConnectionError) -> Error {
    closed()
}

fn closed() -> Error {
    Error::unavailable("participant link is closed")
}
