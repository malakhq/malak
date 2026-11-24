use crate::{
    ChannelName, ChannelPattern, CreationId, Outcome, ParticipantId, Position, SenderTrace,
    SessionProcess,
};
use bytes::Bytes;
use std::time::{Duration, SystemTime};

/// The class of event emitted by the board.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EventKind {
    /// A link opened.
    Connected,
    /// A link closed.
    Disconnected,
    /// The board stored a message.
    Stored,
    /// The board returned a message.
    Read,
    /// The board saved an acknowledgement.
    Acked,
    /// The board advanced the retention boundary.
    Trimmed,
    /// A limit was reached.
    LimitHit,
    /// A call or connection was refused.
    Denied,
    /// A recognised owner operation ended.
    Owner,
    /// A record was missing or failed verification.
    StoreDamaged,
    /// Startup cut the uncommitted tail left by an interrupted write.
    TailCut,
    /// Startup cut an authentic record from an abandoned stamp.
    TailTampered,
    /// An observer fell behind and skipped events.
    Skipped,
}

/// Board activity visible to configured observers.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Event {
    /// Activity that occurred.
    pub kind: EventKind,
    /// Wall-clock time when this activity occurred.
    pub at: SystemTime,
    /// Message kind for a store event.
    pub message_kind: Option<&'static str>,
    /// Reason returned for a failed call, when available.
    pub reason: Option<String>,
    /// Operation name, where the event describes a call.
    pub operation: Option<&'static str>,
    /// Caller or affected participant, when applicable.
    pub participant: Option<ParticipantId>,
    /// How the message or owner action reached the board.
    pub sender_trace: Option<SenderTrace>,
    /// A different session observed on the participant's first send in this run, if known.
    pub earlier_session: Option<SessionProcess>,
    /// Validated owner target, when the target is a participant name or run ID.
    pub owner_target: Option<String>,
    /// Channel affected by the event, where applicable.
    pub channel: Option<ChannelName>,
    /// Creation and position in the affected channel, when applicable.
    pub record: Option<(CreationId, Position)>,
    /// Outcome for a finished operation.
    pub outcome: Option<Outcome>,
    /// Number of events represented or missed.
    pub count: u64,
    /// Message bytes, only for configured observers who request them.
    pub body: Option<Bytes>,
}

impl Event {
    /// Create an event of the given kind with no optional details.
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind,
            at: SystemTime::now(),
            message_kind: None,
            reason: None,
            operation: None,
            participant: None,
            sender_trace: None,
            earlier_session: None,
            owner_target: None,
            channel: None,
            record: None,
            outcome: None,
            count: 1,
            body: None,
        }
    }
}

/// The activity an observer wishes to receive.
#[derive(Clone, Debug, Default)]
pub struct Interest {
    /// Event classes to receive. An empty list selects every class.
    pub events: Vec<EventKind>,
    /// Channel selection, if restricted to a pattern.
    pub channel: Option<ChannelPattern>,
    /// Whether to request bodies when the board's configuration permits them.
    pub include_bodies: bool,
}

/// A sink for the board's live events, driven by the board outside the call path.
pub trait Observer: Send {
    /// Declare which events this observer wants.
    fn describe(&self) -> Interest;

    /// Deliver one matching event.
    fn on_event(&mut self, event: &Event);

    /// Deliver held events within the supplied time budget.
    fn flush(&mut self, deadline: Duration);
}
