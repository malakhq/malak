use crate::Limits;
use malak_layers::{ChannelName, CreationId, Durability, Error, Grade, MessageId, Position};
use std::str::FromStr;

/// The sender's requested kind of message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Kind {
    /// A task. Only callers with proven identity can send tasks.
    Task,
    /// A report.
    Report,
    /// A general note.
    #[default]
    Note,
}

impl Kind {
    /// The stable spelling used by the wire and CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Report => "report",
            Self::Note => "note",
        }
    }
}

impl FromStr for Kind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "task" => Ok(Self::Task),
            "report" => Ok(Self::Report),
            "note" => Ok(Self::Note),
            _ => Err(Error::refused("invalid message kind")),
        }
    }
}

/// The wire spelling of a durability.
pub(crate) const fn durability_name(value: Durability) -> &'static str {
    match value {
        Durability::Memory => "memory",
        Durability::Restart => "restart",
        Durability::Synced => "synced",
    }
}

/// Read a durability from its wire spelling.
pub(crate) fn parse_durability(value: &str) -> Option<Durability> {
    match value {
        "memory" => Some(Durability::Memory),
        "restart" => Some(Durability::Restart),
        "synced" => Some(Durability::Synced),
        _ => None,
    }
}

/// A header value supplied with a send.
#[derive(Clone, Copy, Debug)]
pub enum HeaderValue<'a> {
    /// The value of a content type, trace or extension header.
    Text(&'a str),
    /// A reference to a message being answered.
    ReplyTo {
        /// The referenced channel's name.
        channel: &'a str,
        /// The referenced channel's creation.
        creation: CreationId,
        /// The referenced message's position.
        position: u64,
    },
}

/// A validated message header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Header {
    /// An unchecked reference to where an earlier message was stored.
    ReplyTo {
        /// The referenced channel's validated name.
        channel: ChannelName,
        /// The referenced channel's creation.
        creation: CreationId,
        /// The referenced message's position.
        position: Position,
    },
    /// A media type carried with the body.
    ContentType(Box<str>),
    /// A hexadecimal trace label.
    Trace(Box<str>),
    /// A printable extension value.
    Extension {
        /// The header key, including the `x-` prefix.
        key: Box<str>,
        /// The header's text.
        value: Box<str>,
    },
}

impl Header {
    /// The spelling of the header key.
    pub fn key(&self) -> &str {
        match self {
            Self::ReplyTo { .. } => "reply_to",
            Self::ContentType(_) => "content_type",
            Self::Trace(_) => "trace",
            Self::Extension { key, .. } => key,
        }
    }

    pub(crate) fn parse(key: &str, value: HeaderValue<'_>) -> Result<Self, Error> {
        match (key, value) {
            (
                "reply_to",
                HeaderValue::ReplyTo {
                    channel,
                    creation,
                    position,
                },
            ) => Ok(Self::ReplyTo {
                channel: channel.parse()?,
                creation,
                position: position.try_into()?,
            }),
            ("content_type", HeaderValue::Text(text)) if media_type(text) => {
                Ok(Self::ContentType(text.into()))
            }
            ("trace", HeaderValue::Text(text))
                if text.len() <= 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Ok(Self::Trace(text.into()))
            }
            (key, HeaderValue::Text(value))
                if key.starts_with("x-") && key.len() > 2 && printable(key) && printable(value) =>
            {
                Ok(Self::Extension {
                    key: key.into(),
                    value: value.into(),
                })
            }
            _ => Err(Error::refused("invalid message header")),
        }
    }
}

fn media_type(value: &str) -> bool {
    printable(value)
        && value
            .parse::<mime::Mime>()
            .is_ok_and(|media| media.type_() != mime::STAR && media.subtype() != mime::STAR)
}

fn printable(value: &str) -> bool {
    if value.is_ascii() {
        return value
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic());
    }
    let mut escaped = value.escape_debug();
    while let Some(character) = escaped.next() {
        // Debug escaping preserves printable Unicode and escapes literal backslashes.
        if character == '\\' && matches!(escaped.next(), Some('u' | 't' | 'r' | 'n' | '0')) {
            return false;
        }
    }
    true
}

/// The fields a caller can supply to `send`.
#[derive(Clone, Copy, Debug)]
pub struct SendRequest<'a> {
    /// Channel name, validated by the board after counting the call.
    pub channel: &'a str,
    /// Sender-chosen ID, reused when retrying the same message.
    pub id: &'a str,
    /// Message kind. Use `Kind::default()` for a note.
    pub kind: Kind,
    /// Optional message headers.
    pub headers: &'a [(&'a str, HeaderValue<'a>)],
    /// Opaque message bytes.
    pub body: &'a [u8],
    /// Minimum durability the caller will accept.
    pub minimum_durability: Option<Durability>,
}

impl<'a> SendRequest<'a> {
    /// Make a note with no headers or minimum durability requirement.
    pub fn note(channel: &'a str, id: &'a str, body: &'a [u8]) -> Self {
        Self {
            channel,
            id,
            kind: Kind::Note,
            headers: &[],
            body,
            minimum_durability: None,
        }
    }

    /// Check the send shape and return validated message fields.
    ///
    /// The grade must come from the board's identity check. Returns `refused` if
    /// the shape or kind is invalid for that grade.
    #[doc(hidden)]
    pub fn validate(self, limits: &Limits, grade: Grade) -> Result<SendContent<'a>, Error> {
        if self.body.len() > limits.body_bytes || self.headers.len() > limits.header_entries {
            return Err(Error::refused("message exceeds size limits").at_limit());
        }
        if self.kind == Kind::Task && grade == Grade::LinkOnly {
            return Err(Error::refused("task requires proven identity"));
        }

        let channel = self.channel.parse()?;
        let id = self.id.parse()?;
        let mut size_left = limits.header_bytes;
        let mut headers = Vec::with_capacity(self.headers.len());
        for &(key, value) in self.headers {
            let value_len = match value {
                HeaderValue::Text(text) => text.len(),
                HeaderValue::ReplyTo { channel, .. } => channel.len().saturating_add(16 + 8),
            };
            size_left = size_left
                .checked_sub(key.len().saturating_add(value_len))
                .ok_or_else(|| Error::refused("headers exceed size limit").at_limit())?;
            headers.push(Header::parse(key, value)?);
        }
        headers.sort_unstable_by(|left, right| left.key().cmp(right.key()));
        if headers
            .windows(2)
            .any(|pair| pair[0].key() == pair[1].key())
        {
            return Err(Error::refused("duplicate message header"));
        }

        Ok(SendContent {
            channel,
            id,
            kind: self.kind,
            headers,
            body: self.body,
            minimum_durability: self.minimum_durability,
            header_bytes_left: size_left,
        })
    }
}

/// Validated caller fields. The body still borrows the input buffer.
#[derive(Debug)]
#[non_exhaustive]
pub struct SendContent<'a> {
    /// The channel being sent to.
    pub channel: ChannelName,
    /// The ID used for retries.
    pub id: MessageId,
    /// The kind of message.
    pub kind: Kind,
    /// Headers in key order.
    pub headers: Vec<Header>,
    /// The unchanged opaque body.
    pub body: &'a [u8],
    /// The requested minimum durability.
    pub minimum_durability: Option<Durability>,
    header_bytes_left: usize,
}

impl SendContent<'_> {
    pub(crate) fn add_trace(&mut self, trace: TraceId, limits: &Limits) -> Result<(), Error> {
        if self.headers.len() >= limits.header_entries
            || self.header_bytes_left < "trace".len() + trace.len()
        {
            return Err(
                Error::refused("board-selected trace would exceed header limits").at_limit(),
            );
        }
        let index = self
            .headers
            .partition_point(|header| header.key() < "trace");
        self.headers
            .insert(index, Header::Trace(trace.as_str().into()));
        Ok(())
    }
}

/// The original hexadecimal trace text, including its case.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct TraceId {
    bytes: [u8; 64],
    len: u8,
}

impl TraceId {
    pub(crate) fn fresh() -> Result<Self, Error> {
        loop {
            let number = u128::from_be_bytes(crate::random()?);
            if number != 0 {
                return Ok(Self::parse(&format!("{number:032x}")).expect("generated hexadecimal"));
            }
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        if text.is_empty() || text.len() > 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let mut bytes = [0; 64];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Some(Self {
            bytes,
            len: text.len() as u8,
        })
    }

    pub(crate) fn len(self) -> usize {
        usize::from(self.len)
    }

    /// Return the original trace text.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len()]).expect("validated hexadecimal")
    }
}

impl std::fmt::Debug for TraceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// Evidence of where and how durably a message was stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Receipt {
    /// Position assigned by the board.
    pub position: Position,
    /// Channel creation the position belongs to.
    pub creation: CreationId,
    /// Durability reached by the original write.
    pub durability: Durability,
    /// Whether this receipt was returned from the retry index.
    pub duplicate: bool,
    /// The original message's trace, when retained by its send receipt.
    pub trace: Option<TraceId>,
}

/// A record whose contents and location were checked by the board.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Message {
    /// Sender-chosen message identifier.
    pub id: MessageId,
    /// Channel containing this message.
    pub channel: ChannelName,
    /// Kind requested by the sender.
    pub kind: Kind,
    /// Sender-supplied headers.
    pub headers: Vec<Header>,
    /// Opaque bytes carried without interpretation.
    pub body: bytes::Bytes,
    /// Participant the board identified as the sender.
    pub sender: malak_layers::ParticipantId,
    /// The sender's label when the message was stored.
    pub sender_name: malak_layers::ParticipantName,
    /// What the board observed about the sender's connection, if recorded.
    pub sender_trace: Option<crate::SenderTrace>,
    pub(crate) agent_session_id: Option<Box<str>>,
    /// A differing session from the participant's first send in this board run, if known.
    pub earlier_session: Option<crate::SessionProcess>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) record_run: [u8; 16],
    /// The sender's grade when the message was stored.
    pub grade: Grade,
    /// Board clock at signing, for display.
    pub sent_at: std::time::SystemTime,
    /// The original assigned position, creation and durability.
    pub receipt: Receipt,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Message {
    pub(crate) fn lead_session(&self) -> Option<crate::LeadSession> {
        if !self.sender.is_lead() {
            return None;
        }
        Some(crate::LeadSession {
            process: self.sender_trace.as_ref()?.session.clone()?,
            agent_session_id: self.agent_session_id.clone(),
        })
    }
}
