//! The four verbs over a participant connection.
//!
//! A [`ParticipantClient`] opens a connection with a handover file, performs the
//! handshake and attachment, and then offers `send`, `read`, `get` and `ack`
//! with the same types as an in-process [`Handle`](crate::Handle).
//! Inbox channels accept `to/NAME` or `to/ID`; the board resolves the active
//! name and checks the resolved ID grant. Replies retain the ID channel.

use crate::client::LocalClient;
use crate::{CreationId, Error, GetReply, ReadReply, Receipt, SendRequest};
use bytes::Bytes;
use malak_layers::{Mechanism, Transport};
use malak_proto::Frame;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod codec;

/// How long a call waits for the board to take its request or answer one that
/// has nothing to wait for.
const CALL_WAIT: Duration = Duration::from_secs(10);

/// The first request number after attachment, which uses the first.
const FIRST_REQUEST: u64 = 2;

/// A participant's attached connection to a served board.
///
/// Each call sends one request and reads its replies before it returns, so one
/// client serves one call at a time. A failed call closes the connection:
/// connect again to continue.
pub struct ParticipantClient<T: Transport> {
    link: LocalClient<T>,
    next: u64,
}

impl<T: Transport + 'static> ParticipantClient<T>
where
    T::Connection: 'static,
{
    /// Connect with the handover file at `path` and attach.
    ///
    /// Supply the mechanism the handover names for a general participant, and
    /// `None` for the lead. The file is read again on every attempt, so a
    /// handover that is replaced while the client connects is picked up. The
    /// call reports `unavailable` if it cannot attach within 30 seconds.
    pub fn connect(
        transport: Arc<T>,
        path: impl AsRef<Path>,
        mechanism: Option<Arc<dyn Mechanism>>,
    ) -> Result<Self, Error> {
        LocalClient::participant_from_file(transport, path, move |_| Ok(mechanism.clone()))
            .map(Self::attached)
    }

    fn attached(link: LocalClient<T>) -> Self {
        Self {
            link,
            next: FIRST_REQUEST,
        }
    }

    /// Store a message as this participant.
    ///
    /// Use the same message ID and content on a retry to obtain its original
    /// receipt.
    pub fn send(&mut self, request: SendRequest<'_>) -> Result<Receipt, Error> {
        let rid = self.rid();
        let frame = codec::send(rid, &request)?;
        let reply = self.call(frame, rid, CALL_WAIT, &mut |_| Err(unexpected()))?;
        codec::receipt(&reply)
    }

    /// Return due messages from the named channels, waiting up to `wait` for one.
    ///
    /// At most `max` messages come back. The board does not mark them
    /// acknowledged: call [`ParticipantClient::ack`] after acting on them.
    pub fn read(
        &mut self,
        channels: &[&str],
        max: usize,
        wait: Duration,
    ) -> Result<ReadReply, Error> {
        let rid = self.rid();
        let frame = codec::read(rid, channels, max, wait)?;
        let mut messages = Vec::new();
        let reply = self.call(frame, rid, wait + CALL_WAIT, &mut |message| {
            messages.push(codec::message(message)?);
            Ok(())
        })?;
        codec::read_reply(&reply, messages)
    }

    /// Get a message by position without advancing delivery or acknowledgement state.
    pub fn get(&mut self, channel: &str, position: u64) -> Result<GetReply, Error> {
        let rid = self.rid();
        let frame = codec::get(rid, channel, position)?;
        let mut found = None;
        let reply = self.call(frame, rid, CALL_WAIT, &mut |message| {
            found = Some(codec::message(message)?);
            Ok(())
        })?;
        codec::get_reply(&reply, found)
    }

    /// Acknowledge delivery through a position in the current channel creation.
    pub fn ack(&mut self, channel: &str, creation: CreationId, position: u64) -> Result<(), Error> {
        let rid = self.rid();
        let frame = codec::ack(rid, channel, creation, position)?;
        self.call(frame, rid, CALL_WAIT, &mut |_| Err(unexpected()))
            .map(drop)
    }

    fn rid(&mut self) -> u64 {
        let rid = self.next;
        self.next += 1;
        rid
    }

    /// Send a request and hand each `MSG` reply to `message` until its closing frame.
    fn call(
        &mut self,
        request: Bytes,
        rid: u64,
        wait: Duration,
        message: &mut dyn FnMut(&Frame) -> Result<(), Error>,
    ) -> Result<Frame, Error> {
        self.link.send_frame(request, CALL_WAIT)?;
        loop {
            let reply = self.link.read_frame(wait)?;
            if reply.rid() != rid {
                return Err(unexpected());
            }
            match reply.op() {
                "MSG" => message(&reply)?,
                "OK" => return Ok(reply),
                "ERR" => return Err(codec::failure(&reply)),
                _ => return Err(unexpected()),
            }
        }
    }
}

fn unexpected() -> Error {
    Error::unavailable("unexpected reply from the board")
}
