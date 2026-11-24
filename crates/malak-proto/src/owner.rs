use crate::encoding::{Field, encode};
use crate::{ConnectionError, Frame, FrameDecoder, FramingError};
use bytes::Bytes;
use std::time::Duration;

/// Limits on the protected owner request stream.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct OwnerLimits {
    /// Maximum complete request bytes, including both lengths and the header.
    pub frame_bytes: usize,
    /// Maximum silence when no tail is running.
    pub silence: Duration,
    /// Maximum elapsed lifetime of the link.
    pub lifetime: Duration,
}

impl Default for OwnerLimits {
    fn default() -> Self {
        Self {
            frame_bytes: 64 * 1024,
            silence: Duration::from_secs(15 * 60),
            lifetime: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// Kind of request recognised at the owner door.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnerOperation {
    /// Inspect the board.
    Status,
    /// Enrol a participant.
    Enrol,
    /// Replace one participant's key.
    Rekey,
    /// Revoke a participant.
    Revoke,
    /// Replace a grant.
    Grant,
    /// Create a channel.
    ChannelCreate,
    /// Remove a channel creation.
    ChannelRemove,
    /// Trim a channel creation.
    Trim,
    /// Stream events.
    Tail,
    /// Stop this board run.
    Stop,
    /// Post the fixed Malak upgrade note to this run's lead inbox.
    UpgradeNotice,
    /// Check that the connection is responsive.
    Ping,
    /// End a tail on this connection.
    Cancel,
    /// Reply with `not_supported`. This includes participant-only operations.
    Unsupported,
}

impl OwnerOperation {
    fn parse(op: &str) -> Self {
        match op {
            "STATUS" => Self::Status,
            "ENROL" => Self::Enrol,
            "REKEY" => Self::Rekey,
            "REVOKE" => Self::Revoke,
            "GRANT" => Self::Grant,
            "CHANNEL_CREATE" => Self::ChannelCreate,
            "CHANNEL_REMOVE" => Self::ChannelRemove,
            "TRIM" => Self::Trim,
            "TAIL" => Self::Tail,
            "STOP" => Self::Stop,
            "UPGRADE_NOTICE" => Self::UpgradeNotice,
            "PING" => Self::Ping,
            "CANCEL" => Self::Cancel,
            _ => Self::Unsupported,
        }
    }

    fn control(self) -> bool {
        matches!(self, Self::Ping | Self::Cancel)
    }
}

/// The attachment-free identity of one open owner request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnerTicket {
    rid: u64,
    serial: u64,
    session: [u8; 16],
}

impl OwnerTicket {
    /// Return the request number chosen by the caller.
    pub const fn rid(self) -> u64 {
        self.rid
    }
}

/// A complete protected frame received at the owner door.
#[derive(Debug)]
pub struct OwnerRequest {
    /// The operation to dispatch or reject as unsupported.
    pub operation: OwnerOperation,
    /// The ticket to complete when the reply is sent.
    pub ticket: OwnerTicket,
    /// The frame for operation-specific validation.
    pub frame: Frame,
}

/// A closing reply containing only the owner operation's outcome.
pub enum OwnerOutcome<'a> {
    /// The requested action completed.
    Done,
    /// The requested action failed.
    Error {
        /// A protocol outcome such as `refused` or `not_supported`.
        outcome: &'a str,
        /// The public reason for this outcome.
        reason: &'a str,
    },
}

/// Encode an owner reply containing no operation-specific fields.
pub fn owner_outcome(rid: u64, outcome: OwnerOutcome<'_>) -> Result<Bytes, FramingError> {
    match outcome {
        OwnerOutcome::Done => encode(rid, "OK", &[], &[]),
        OwnerOutcome::Error { outcome, reason } => encode(
            rid,
            "ERR",
            &[
                ("outcome", Field::Text(outcome)),
                ("reason", Field::Text(reason)),
            ],
            &[],
        ),
    }
}

/// Framing, timers, and request admission after the owner handshake.
///
/// Supply only authenticated plaintext. Drive an instance serially and give
/// times as elapsed time from the original link opening.
pub struct OwnerConnection {
    limits: OwnerLimits,
    frames: FrameDecoder,
    session: [u8; 16],
    serial: u64,
    outstanding: Option<(OwnerTicket, OwnerOperation)>,
    control: Option<OwnerTicket>,
    last_input: Duration,
    tailing: bool,
    closed: bool,
}

impl OwnerConnection {
    /// Start accepting frames once the owner's Noise reply has been sent.
    pub fn new(
        limits: OwnerLimits,
        handshake_hash: &[u8; 32],
        elapsed: Duration,
    ) -> Result<Self, ConnectionError> {
        let frames =
            FrameDecoder::with_frame_limit(limits.frame_bytes).map_err(ConnectionError::Framing)?;
        let mut connection = Self {
            limits,
            frames,
            session: handshake_hash[..16].try_into().expect("hash length"),
            serial: 0,
            outstanding: None,
            control: None,
            last_input: elapsed,
            tailing: false,
            closed: false,
        };
        connection.tick(elapsed)?;
        Ok(connection)
    }

    /// Return the next deadline for the authenticated owner link.
    pub fn deadline(&self) -> Duration {
        if self.tailing {
            self.limits.lifetime
        } else {
            self.limits
                .lifetime
                .min(self.last_input.saturating_add(self.limits.silence))
        }
    }

    /// Close the link if its deadline has been reached.
    pub fn tick(&mut self, elapsed: Duration) -> Result<(), ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        if elapsed >= self.limits.lifetime {
            return self.fail(ConnectionError::Deadline);
        }
        if elapsed >= self.deadline() {
            return self.fail(ConnectionError::Silence);
        }
        Ok(())
    }

    /// Report input progress, including bytes of an incomplete segment.
    pub fn peer_progress(&mut self, elapsed: Duration) -> Result<(), ConnectionError> {
        self.tick(elapsed)?;
        self.last_input = elapsed;
        Ok(())
    }

    /// Admit one request from the given authenticated bytes.
    ///
    /// Decode order and ticket checks close the stream on error. Dispatch a
    /// control request before supplying further input.
    pub fn receive(
        &mut self,
        input: &mut Bytes,
        elapsed: Duration,
    ) -> Result<Option<OwnerRequest>, ConnectionError> {
        self.tick(elapsed)?;
        if self.control.is_some() {
            return self.fail(ConnectionError::State);
        }
        let frame = match self.frames.pull(input) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(None),
            Err(error) => return self.fail(ConnectionError::Framing(error)),
        };
        if self
            .outstanding
            .is_some_and(|(ticket, _)| ticket.rid == frame.rid())
        {
            return self.fail(ConnectionError::RequestInUse);
        }
        let operation = OwnerOperation::parse(frame.op());
        if !operation.control() && self.outstanding.is_some() {
            return self.fail(ConnectionError::State);
        }
        let Some(serial) = self.serial.checked_add(1) else {
            return self.fail(ConnectionError::Deadline);
        };
        self.serial = serial;
        let ticket = OwnerTicket {
            rid: frame.rid(),
            serial,
            session: self.session,
        };
        if operation.control() {
            self.control = Some(ticket);
        } else {
            self.outstanding = Some((ticket, operation));
        }
        Ok(Some(OwnerRequest {
            operation,
            ticket,
            frame,
        }))
    }

    /// Record that the request's tail has started streaming events.
    pub fn start_tail(&mut self, ticket: OwnerTicket) -> Result<(), ConnectionError> {
        if self.closed || self.outstanding != Some((ticket, OwnerOperation::Tail)) {
            return self.fail(ConnectionError::State);
        }
        self.tailing = true;
        Ok(())
    }

    /// Find a running tail with this request number on this connection.
    pub fn cancellable(&self, rid: u64) -> Option<OwnerTicket> {
        let (ticket, _) = self.outstanding?;
        (self.tailing && ticket.rid == rid).then_some(ticket)
    }

    /// Confirm a reply still belongs to an open request on this link.
    pub fn check_reply(&self, ticket: OwnerTicket) -> Result<(), ConnectionError> {
        if self.closed {
            return Err(ConnectionError::Closed);
        }
        if self.control == Some(ticket)
            || self
                .outstanding
                .is_some_and(|(pending, _)| pending == ticket)
        {
            Ok(())
        } else {
            Err(ConnectionError::State)
        }
    }

    /// Release a request after sending its closing reply.
    pub fn reply_sent(
        &mut self,
        ticket: OwnerTicket,
        elapsed: Duration,
    ) -> Result<(), ConnectionError> {
        self.tick(elapsed)?;
        if let Err(error) = self.check_reply(ticket) {
            return self.fail(error);
        }
        if self.control == Some(ticket) {
            self.control = None;
        } else {
            self.outstanding = None;
            if self.tailing {
                self.last_input = elapsed;
                self.tailing = false;
            }
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: ConnectionError) -> Result<T, ConnectionError> {
        self.closed = true;
        self.outstanding = None;
        self.control = None;
        self.tailing = false;
        Err(error)
    }
}
