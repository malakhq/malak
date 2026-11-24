//! Drive the synchronous owner operations independently of the link reader.

use super::{
    LocalEntrances, LocalOpening, cancel_target, closed, outcome_frame, owner_protocol_error,
    tail::{event_frame, watch},
};
use crate::Error;
use crate::board::events::Watcher;
use crate::link::{Segments, Wire};
use crate::workers::Workers;
use bytes::Bytes;
use malak_layers::Transport;
use malak_proto::{
    Encrypted, MAX_CONTENT, OwnerConnection, OwnerOperation, OwnerRequest, OwnerTicket, SegmentKind,
};
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

impl<T: Transport + 'static> LocalOpening<T>
where
    T::Connection: 'static,
{
    /// Serve the authenticated owner link until it closes.
    ///
    /// The reader continues to authenticate frames while an owner operation is
    /// in progress. The protocol rejects a second ordinary request during that
    /// time. An operation already admitted can finish after the link closes.
    pub fn serve_owner(self) -> Result<(), Error> {
        self.owner_requests()?.serve()
    }
}

impl<T: Transport + 'static> super::LocalOwner<T>
where
    T::Connection: 'static,
{
    fn serve(mut self) -> Result<(), Error> {
        let state = self.state.take().ok_or_else(closed)?;
        let protected = self.link.protected.take().ok_or_else(closed)?;
        let server = Arc::new(Server {
            operations: LocalEntrances {
                board: self.link.board.clone(),
                settings: self.link.settings.clone(),
            },
            wire: self.link.wire.clone(),
            opened: self.link.opened,
            encrypted: Mutex::new(protected.encrypted),
            protocol: Mutex::new(state.protocol),
            output: Mutex::new(()),
            closing_write: Mutex::new(()),
            wake: Condvar::new(),
            tail: Mutex::new(None),
            closed: AtomicBool::new(false),
            workers: Workers::named("malak-owner-operation"),
        });
        std::thread::scope(|tasks| {
            let timer = std::thread::Builder::new()
                .name("malak-owner-deadline".into())
                .spawn_scoped(tasks, || server.deadline())
                .map_err(|_| Error::unavailable("could not start the owner timer"))?;
            let result = server.receive(protected.segments, state.plaintext);
            server.close();
            timer
                .join()
                .map_err(|_| Error::unavailable("owner timer stopped"))?;
            result
        })
    }
}

struct Server<T: Transport> {
    operations: LocalEntrances,
    wire: Arc<Wire<T>>,
    opened: Instant,
    encrypted: Mutex<Encrypted>,
    protocol: Mutex<OwnerConnection>,
    output: Mutex<()>,
    // Serialize decoding with publication of a closing reply's final segment.
    closing_write: Mutex<()>,
    wake: Condvar,
    tail: Mutex<Option<Arc<Watcher>>>,
    closed: AtomicBool,
    workers: Workers,
}

impl<T: Transport + 'static> Server<T>
where
    T::Connection: 'static,
{
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.wire.close();
            self.end_tail();
            let _state = self.protocol.lock();
            self.wake.notify_all();
        }
    }

    fn deadline(&self) {
        let mut protocol = self.protocol.lock();
        while !self.closed.load(Ordering::Acquire) {
            if protocol.tick(self.opened.elapsed()).is_err() {
                drop(protocol);
                self.close();
                return;
            }
            let until = self.opened + protocol.deadline();
            self.wake.wait_until(&mut protocol, until);
        }
    }

    fn receive(
        self: &Arc<Self>,
        mut segments: Segments,
        mut plaintext: Bytes,
    ) -> Result<(), Error> {
        loop {
            let next = {
                let _closing = self.closing_write.lock();
                self.protocol
                    .lock()
                    .receive(&mut plaintext, self.opened.elapsed())
                    .map_err(owner_protocol_error)?
            };
            let Some(request) = next else {
                let segment =
                    segments.receive_with(&self.wire, SegmentKind::Protected, |arrival| {
                        let mut protocol = self.protocol.lock();
                        protocol
                            .peer_progress(arrival.duration_since(self.opened))
                            .map_err(owner_protocol_error)?;
                        self.wake.notify_one();
                        Ok(())
                    })?;
                plaintext = Segments::decrypt(&mut self.encrypted.lock(), &segment)?;
                continue;
            };
            match request.operation {
                OwnerOperation::Ping => {
                    let reply = self.operations.answer(&request, self.wire.sender()).0?;
                    self.reply(request.ticket, &reply)?;
                }
                OwnerOperation::Cancel => self.cancel(&request)?,
                OwnerOperation::Tail => self.begin_tail(&request)?,
                _ => {
                    let running = self.clone();
                    let ticket = request.ticket;
                    let request = Arc::new(request);
                    let work = request.clone();
                    if let Err(error) = self.workers.dispatch(move || {
                        let (result, stopping) =
                            running.operations.answer(&work, running.wire.sender());
                        let sent = result.and_then(|frame| running.reply(ticket, &frame));
                        if stopping {
                            running.operations.board.finish_stop();
                        }
                        if sent.is_err() {
                            running.close();
                        }
                    }) {
                        self.operations.record_operation(
                            &request,
                            Some(&error),
                            &self.wire.sender().trace,
                        );
                        self.reply(ticket, &outcome_frame(ticket, Err(error))?)?;
                    }
                }
            }
        }
    }

    fn reply(&self, ticket: OwnerTicket, frame: &[u8]) -> Result<(), Error> {
        let _output = self.output.lock();
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        self.protocol
            .lock()
            .check_reply(ticket)
            .map_err(owner_protocol_error)?;
        let mut chunks = frame.chunks(MAX_CONTENT).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let _closing = last.then(|| self.closing_write.lock());
            self.send(chunk)?;
            if last {
                let mut protocol = self.protocol.lock();
                protocol
                    .reply_sent(ticket, self.opened.elapsed())
                    .map_err(owner_protocol_error)?;
                self.wake.notify_one();
            }
        }
        Ok(())
    }

    fn send(&self, chunk: &[u8]) -> Result<(), Error> {
        let packet = Segments::packet(&mut self.encrypted.lock(), chunk)?;
        self.wire.write_with(&packet, |_| Ok(()))
    }

    fn begin_tail(self: &Arc<Self>, request: &OwnerRequest) -> Result<(), Error> {
        let ticket = request.ticket;
        let writer = match watch(&self.operations.board, request) {
            Ok(writer) => writer,
            Err(error) => return self.reply(ticket, &outcome_frame(ticket, Err(error))?),
        };
        let watcher = writer.watcher();
        {
            let mut protocol = self.protocol.lock();
            protocol.start_tail(ticket).map_err(owner_protocol_error)?;
            self.wake.notify_one();
        }
        {
            let mut tail = self.tail.lock();
            if self.closed.load(Ordering::Acquire) {
                return Err(closed());
            }
            *tail = Some(watcher.clone());
        }
        let running = self.clone();
        let spawned = self.workers.dispatch(move || {
            let result = writer.run(|event, missed| {
                event_frame(ticket, &event, missed).and_then(|frame| {
                    let _output = running.output.lock();
                    if running
                        .tail
                        .lock()
                        .as_ref()
                        .is_some_and(|active| Arc::ptr_eq(active, &watcher))
                    {
                        for chunk in frame.chunks(MAX_CONTENT) {
                            running.send(chunk)?;
                        }
                    }
                    Ok(())
                })
            });
            if result.is_err() {
                running.close();
            }
        });
        if let Err(error) = spawned {
            self.end_tail();
            self.reply(ticket, &outcome_frame(ticket, Err(error))?)?;
        }
        Ok(())
    }

    fn cancel(&self, request: &OwnerRequest) -> Result<(), Error> {
        match cancel_target(&request.frame) {
            Ok(target) => {
                let tail = self.protocol.lock().cancellable(target);
                if let Some(ticket) = tail {
                    self.end_tail();
                    self.reply(ticket, &outcome_frame(ticket, Ok(()))?)?;
                }
                self.reply(request.ticket, &outcome_frame(request.ticket, Ok(()))?)
            }
            Err(error) => self.reply(request.ticket, &outcome_frame(request.ticket, Err(error))?),
        }
    }

    fn end_tail(&self) {
        if let Some(watcher) = self.tail.lock().take() {
            watcher.close();
        }
    }
}
