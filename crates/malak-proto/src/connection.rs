use crate::reply_queue::Replies;
use crate::{Frame, FrameDecoder, FramingError, MAX_HEADER, max_frame_bytes};
use bytes::Bytes;
use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

/// Resource and lifetime limits for a participant connection.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct ConnectionLimits {
    /// Largest accepted body in a frame.
    pub body_bytes: usize,
    /// Total bytes allowed from the caller while attaching.
    pub attach_bytes: usize,
    /// Most proof rounds, including the first attach request.
    pub attach_rounds: usize,
    /// Longest permitted time from link opening through attachment.
    pub attach_time: Duration,
    /// Total life from link opening.
    pub lifetime: Duration,
    /// Most open data requests at a time.
    pub open_data_requests: usize,
    /// Places held until their closing reply is fully written.
    pub reply_places: usize,
    /// Maximum untransmitted bytes in message frames.
    pub message_bytes: usize,
    /// How long a nonempty queue can make no progress on the wire.
    pub no_progress: Duration,
    /// How long an attached caller can send nothing while the board is reading.
    pub silence: Duration,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            body_bytes: 1024 * 1024,
            attach_bytes: 16 * 1024,
            attach_rounds: 4,
            attach_time: Duration::from_secs(5),
            lifetime: Duration::from_secs(24 * 60 * 60),
            open_data_requests: 8,
            reply_places: 16,
            message_bytes: 16 * 1024 * 1024,
            no_progress: Duration::from_secs(60),
            silence: Duration::from_secs(15 * 60),
        }
    }
}

/// Public phase of a participant connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// The link is open and the Noise exchange is in progress.
    Opening,
    /// The handshake is complete and attachment is in progress.
    Attaching,
    /// Data and control operations can be admitted.
    Attached,
    /// The connection has ended.
    Closed,
}

/// A known caller operation at a participant entrance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Claim an enrolled identity.
    Attach,
    /// Supply one further step of a mechanism exchange.
    Step,
    /// Write a message.
    Send,
    /// Wait for or read due messages.
    Read,
    /// Retrieve a message at a position.
    Get,
    /// Acknowledge delivery through a position.
    Ack,
    /// Cancel an open read.
    Cancel,
    /// Keep the link active.
    Ping,
}

impl Operation {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "ATTACH" => Self::Attach,
            "STEP" => Self::Step,
            "SEND" => Self::Send,
            "READ" => Self::Read,
            "GET" => Self::Get,
            "ACK" => Self::Ack,
            "CANCEL" => Self::Cancel,
            "PING" => Self::Ping,
            _ => return None,
        })
    }

    fn has_body(self) -> bool {
        matches!(self, Self::Attach | Self::Step | Self::Send)
    }

    fn data(self) -> bool {
        matches!(self, Self::Send | Self::Read | Self::Get | Self::Ack)
    }

    fn under_pressure(self) -> bool {
        matches!(self, Self::Send | Self::Read | Self::Get)
    }
}

/// Connection-local identity of one accepted or rejected request.
///
/// A request number can be reused after its closing reply is queued. The
/// ticket still distinguishes the old request from a newer one with that number.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Ticket {
    rid: u64,
    serial: u64,
    session: [u8; 16],
}

impl Ticket {
    /// The request number originally selected by the caller.
    pub const fn rid(self) -> u64 {
        self.rid
    }
}

/// A complete caller frame and its reserved reply place.
#[derive(Debug)]
pub struct Request {
    /// The known participant operation.
    pub operation: Operation,
    /// Reservation for this request's closing reply.
    pub ticket: Ticket,
    /// The validated frame.
    pub frame: Frame,
}

/// The result of feeding input to a participant connection.
#[derive(Debug)]
pub enum Input {
    /// More plaintext is needed to decode a whole frame.
    Partial,
    /// The driver must resume reading after a reply place is freed.
    Paused,
    /// The request was admitted for processing.
    Request(Request),
    /// Answer this request with an unavailable closing reply without running it.
    Unavailable(Ticket),
}

/// Result of placing a message frame in the outgoing queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Placement {
    /// The message is on the queue.
    Placed,
    /// The bounded queue cannot accept the message yet.
    Full,
    /// The request has already ended.
    Ended,
}

/// The board's answer to the current attachment round.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Attachment {
    /// A mechanism needs another step.
    More,
    /// The authority checks and connection admission succeeded.
    Accepted,
    /// Authority checks succeeded and all attached places are occupied.
    Unavailable,
}

/// Why a local connection transition failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionError {
    /// The frame could not be decoded.
    Framing(FramingError),
    /// The operation is unknown or forbidden in the connection's phase.
    Operation,
    /// A frame carried a body for an operation that takes none.
    Body,
    /// The request number is already in use.
    RequestInUse,
    /// The connection has reached a lifetime limit.
    Deadline,
    /// A continuously nonempty queue could not write any bytes in time.
    NoProgress,
    /// The peer sent nothing while the board was reading.
    Silence,
    /// A transition did not correspond to pending work.
    State,
    /// The connection has already ended.
    Closed,
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, writer: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framing(error) => error.fmt(writer),
            Self::Operation => writer.write_str("operation is not allowed in this state"),
            Self::Body => writer.write_str("operation does not take a body"),
            Self::RequestInUse => writer.write_str("request number is still in use"),
            Self::Deadline => writer.write_str("connection lifetime reached"),
            Self::NoProgress => writer.write_str("outgoing connection made no progress"),
            Self::Silence => writer.write_str("attached connection is silent"),
            Self::State => writer.write_str("unexpected connection transition"),
            Self::Closed => writer.write_str("connection is closed"),
        }
    }
}

impl std::error::Error for ConnectionError {}

enum State {
    Opening,
    Attaching(AttachStage),
    Attached,
    Closed,
}

enum AttachStage {
    First,
    Step,
    Reply(Ticket),
}

struct Open {
    ticket: Ticket,
    operation: Operation,
    data: bool,
    rejected: bool,
    gave_message: bool,
}

/// Participant admission and request lifetimes, driven by plaintext and elapsed time.
///
/// Drive each instance serially. The transport driver supplies elapsed time
/// from link opening. The board decides what proof is sufficient to attach.
pub struct Connection {
    limits: ConnectionLimits,
    state: State,
    frames: FrameDecoder,
    rounds: usize,
    serial: u64,
    session: [u8; 16],
    open_data: usize,
    open: HashMap<u64, Open>,
    replies: Replies,
    last_input: Duration,
    reading_paused: Option<Duration>,
}

impl Connection {
    /// Start in the opening phase with the given bounds.
    pub fn new(limits: ConnectionLimits) -> Result<Self, ConnectionError> {
        if limits.reply_places <= limits.open_data_requests
            || limits.attach_rounds == 0
            || !max_frame_bytes(limits.body_bytes).is_some_and(|size| size <= limits.message_bytes)
        {
            return Err(ConnectionError::State);
        }
        let frames = FrameDecoder::new(limits.body_bytes, Some(limits.attach_bytes))
            .map_err(ConnectionError::Framing)?;
        Ok(Self {
            limits,
            state: State::Opening,
            frames,
            rounds: 0,
            serial: 0,
            session: [0; 16],
            open_data: 0,
            open: HashMap::new(),
            replies: Replies::new(limits.message_bytes),
            last_input: Duration::ZERO,
            reading_paused: None,
        })
    }

    /// The current connection phase.
    pub const fn phase(&self) -> Phase {
        match self.state {
            State::Opening => Phase::Opening,
            State::Attaching(_) => Phase::Attaching,
            State::Attached => Phase::Attached,
            State::Closed => Phase::Closed,
        }
    }

    /// Whether the driver can read more protected data at the moment.
    pub fn can_read(&self) -> bool {
        match self.state {
            State::Attaching(AttachStage::First | AttachStage::Step) => true,
            State::Attached => self.open.len() + self.replies.closings() < self.limits.reply_places,
            _ => false,
        }
    }

    /// Advance after sending the successful Noise reply.
    ///
    /// Pass the final handshake hash to scope request tickets to this link.
    pub fn handshake_reply_sent(
        &mut self,
        handshake_hash: &[u8; 32],
        elapsed: Duration,
    ) -> Result<(), ConnectionError> {
        self.tick(elapsed)?;
        if !matches!(self.state, State::Opening) {
            return self.fail(ConnectionError::State);
        }
        self.session.copy_from_slice(&handshake_hash[..16]);
        self.state = State::Attaching(AttachStage::First);
        Ok(())
    }

    /// Close a link whose unproven or absolute lifetime has elapsed.
    pub fn tick(&mut self, elapsed: Duration) -> Result<(), ConnectionError> {
        if matches!(self.state, State::Closed) {
            return Err(ConnectionError::Closed);
        }
        if elapsed >= self.limits.lifetime
            || !matches!(self.state, State::Attached) && elapsed >= self.limits.attach_time
        {
            return self.fail(ConnectionError::Deadline);
        }
        if matches!(self.state, State::Attached) {
            if self.replies.stalled(elapsed, self.limits.no_progress) {
                return self.fail(ConnectionError::NoProgress);
            }
            if self.reading_paused.is_none()
                && elapsed.saturating_sub(self.last_input) >= self.limits.silence
            {
                return self.fail(ConnectionError::Silence);
            }
        }
        Ok(())
    }

    /// Read one complete request from the supplied authenticated plaintext.
    ///
    /// A proof refusal closes the connection without a reply. The board calls
    /// `attachment_reply_sent` only for the specified attachment replies.
    pub fn receive(
        &mut self,
        bytes: &mut Bytes,
        elapsed: Duration,
    ) -> Result<Input, ConnectionError> {
        self.tick(elapsed)?;
        if !self.can_read() {
            return match self.state {
                State::Attached => Ok(Input::Paused),
                State::Attaching(AttachStage::Reply(_)) if bytes.is_empty() => Ok(Input::Partial),
                _ => self.fail(ConnectionError::State),
            };
        }
        let frame = match self.frames.pull(bytes) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(Input::Partial),
            Err(error) => return self.fail(ConnectionError::Framing(error)),
        };
        let Some(operation) = Operation::parse(frame.op()) else {
            return self.fail(ConnectionError::Operation);
        };
        if !operation.has_body() && !frame.body().is_empty() {
            return self.fail(ConnectionError::Body);
        }
        let ticket = self.ticket(frame.rid())?;
        match self.state {
            State::Attaching(AttachStage::First) if operation == Operation::Attach => {
                self.attaching(ticket)
            }
            State::Attaching(AttachStage::Step) if operation == Operation::Step => {
                self.attaching(ticket)
            }
            State::Attached if !matches!(operation, Operation::Attach | Operation::Step) => {
                if self.open.contains_key(&ticket.rid) {
                    return self.fail(ConnectionError::RequestInUse);
                }
                let rejected = (operation.data()
                    && self.open_data >= self.limits.open_data_requests)
                    || (self.replies.pressure() && operation.under_pressure());
                let data = operation.data() && !rejected;
                self.open_data += usize::from(data);
                self.open.insert(
                    ticket.rid,
                    Open {
                        ticket,
                        operation,
                        data,
                        rejected,
                        gave_message: false,
                    },
                );
                if !self.can_read() {
                    self.reading_paused = Some(elapsed);
                }
                if rejected {
                    return Ok(Input::Unavailable(ticket));
                }
            }
            _ => return self.fail(ConnectionError::Operation),
        }
        Ok(Input::Request(Request {
            operation,
            ticket,
            frame,
        }))
    }

    fn ticket(&mut self, rid: u64) -> Result<Ticket, ConnectionError> {
        let Some(serial) = self.serial.checked_add(1) else {
            return self.fail(ConnectionError::Deadline);
        };
        self.serial = serial;
        Ok(Ticket {
            rid,
            serial,
            session: self.session,
        })
    }

    fn attaching(&mut self, ticket: Ticket) {
        self.rounds += 1;
        self.state = State::Attaching(AttachStage::Reply(ticket));
    }

    /// Advance after the current attachment reply has been sent.
    pub fn attachment_reply_sent(
        &mut self,
        request: Ticket,
        answer: Attachment,
        elapsed: Duration,
    ) -> Result<(), ConnectionError> {
        self.tick(elapsed)?;
        if !matches!(self.state, State::Attaching(AttachStage::Reply(ticket)) if ticket == request)
        {
            return self.fail(ConnectionError::State);
        }
        match answer {
            Attachment::More if self.rounds < self.limits.attach_rounds => {
                self.state = State::Attaching(AttachStage::Step);
            }
            Attachment::More => return self.fail(ConnectionError::Deadline),
            Attachment::Accepted => {
                self.frames = FrameDecoder::new(self.limits.body_bytes, None)
                    .map_err(ConnectionError::Framing)?;
                self.last_input = elapsed;
                self.state = State::Attached;
            }
            Attachment::Unavailable => self.close(),
        }
        Ok(())
    }

    /// Find an open read that a CANCEL on this connection can end.
    ///
    /// Hold the connection's lock from this decision through queuing the
    /// closing replies for the read and the cancel.
    pub fn cancellable(&self, rid: u64) -> Option<Ticket> {
        self.open
            .get(&rid)
            .filter(|open| open.operation == Operation::Read && !open.rejected)
            .map(|open| open.ticket)
    }

    /// Test whether this exact request is still awaiting its closing reply.
    pub fn is_open(&self, ticket: Ticket) -> bool {
        self.open
            .get(&ticket.rid)
            .is_some_and(|request| request.ticket == ticket)
    }

    /// Earliest timer deadline as elapsed time since opening.
    pub fn deadline(&self) -> Duration {
        match self.state {
            State::Closed => Duration::ZERO,
            State::Opening | State::Attaching(_) => {
                self.limits.lifetime.min(self.limits.attach_time)
            }
            State::Attached => {
                let mut deadline = self.limits.lifetime;
                if let Some(at) = self.replies.deadline(self.limits.no_progress) {
                    deadline = deadline.min(at);
                }
                if self.reading_paused.is_none() {
                    deadline = deadline.min(self.last_input.saturating_add(self.limits.silence));
                }
                deadline
            }
        }
    }

    /// Report that at least one byte arrived from the peer on the wire.
    ///
    /// Supply elapsed time from link opening, even for an incomplete segment.
    pub fn peer_progress(&mut self, elapsed: Duration) -> Result<(), ConnectionError> {
        self.tick(elapsed)?;
        if self.can_read() {
            self.last_input = elapsed;
        }
        Ok(())
    }

    /// The number of message-frame bytes still queued to write.
    pub fn message_load(&self) -> usize {
        self.replies.message_bytes()
    }

    /// Place a complete encoded message frame for an admitted GET or READ.
    pub fn enqueue_message(
        &mut self,
        ticket: Ticket,
        frame: Bytes,
        elapsed: Duration,
    ) -> Result<Placement, ConnectionError> {
        self.tick(elapsed)?;
        let Some(open) = self
            .open
            .get_mut(&ticket.rid)
            .filter(|open| open.ticket == ticket)
        else {
            return Ok(Placement::Ended);
        };
        if open.rejected
            || !matches!(open.operation, Operation::Get | Operation::Read)
            || open.operation == Operation::Get && open.gave_message
            || frame.is_empty()
        {
            return Err(ConnectionError::State);
        }
        if !self.replies.message(frame, elapsed) {
            return Ok(Placement::Full);
        }
        open.gave_message = true;
        Ok(Placement::Placed)
    }

    /// Place the closing frame in its reserved space.
    ///
    /// Returns false if the request already ended. The place is released by the
    /// writer only after transmitting the entire reply.
    pub fn enqueue_closing(
        &mut self,
        ticket: Ticket,
        frame: Bytes,
        elapsed: Duration,
    ) -> Result<bool, ConnectionError> {
        self.tick(elapsed)?;
        let Some(open) = self
            .open
            .get(&ticket.rid)
            .filter(|open| open.ticket == ticket)
        else {
            return Ok(false);
        };
        if !(7..=MAX_HEADER + 6).contains(&frame.len()) {
            return Err(ConnectionError::State);
        }
        self.open_data -= usize::from(open.data);
        self.open.remove(&ticket.rid);
        self.replies.closing(frame, elapsed);
        Ok(true)
    }

    /// Borrow the unwritten portion of the next frame, in queue order.
    pub fn outgoing(&self) -> Option<&[u8]> {
        self.replies.front()
    }

    /// Report wire progress and retire the corresponding bytes from the first frame.
    ///
    /// Transport framing and authentication tags can make progress without
    /// retiring any frame bytes. Pass zero frame bytes in that case.
    pub fn wrote(
        &mut self,
        frame_bytes: usize,
        wire_bytes: usize,
        elapsed: Duration,
    ) -> Result<(), ConnectionError> {
        self.tick(elapsed)?;
        if !self.replies.progress(frame_bytes, wire_bytes, elapsed) {
            return Err(ConnectionError::State);
        }
        if self.can_read()
            && let Some(paused) = self.reading_paused.take()
        {
            self.last_input = self
                .last_input
                .saturating_add(elapsed.saturating_sub(paused));
        }
        Ok(())
    }

    /// End the connection and discard all outstanding request reservations.
    pub fn close(&mut self) {
        self.state = State::Closed;
        self.open.clear();
        self.replies = Replies::new(self.limits.message_bytes);
        self.reading_paused = None;
        self.open_data = 0;
    }

    fn fail<T>(&mut self, error: ConnectionError) -> Result<T, ConnectionError> {
        self.close();
        Err(error)
    }
}
