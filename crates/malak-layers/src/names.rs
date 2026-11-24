use crate::Error;
use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

const LEAD: &str = "lead";
const MAX_CHANNEL_BYTES: usize = 128;
const MAX_SEGMENTS: usize = 8;

fn lower_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

fn segment(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(lower_alphanumeric)
        && bytes.all(|byte| lower_alphanumeric(byte) || matches!(byte, b'_' | b'-'))
}

macro_rules! name {
    ($(#[$attribute:meta])* $name:ident, $reason:literal, $valid:expr) => {
        $(#[$attribute])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);

        impl $name {
            /// Return the validated ASCII spelling.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if ($valid)(value) {
                    Ok(Self(value.into()))
                } else {
                    Err(Error::refused($reason))
                }
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
                output.write_str(self.as_str())
            }
        }
    };
}

name!(
    /// A board-assigned participant identifier, including the reserved `lead`.
    ParticipantId, "invalid participant ID",
    |value: &str| value == LEAD
        || ((8..=32).contains(&value.len()) && value.bytes().all(lower_alphanumeric))
);

impl ParticipantId {
    /// The reserved identity of the board's lead.
    pub fn lead() -> Self {
        Self(LEAD.into())
    }

    /// Whether this is the board's lead.
    pub fn is_lead(&self) -> bool {
        self.as_str() == LEAD
    }

    /// The participant's inbox. The lead's inbox is `lead`.
    pub fn inbox(&self) -> ChannelName {
        if self.is_lead() {
            return ChannelName(self.0.clone());
        }
        ChannelName(format!("to/{self}").into())
    }
}

name!(
    /// A human-readable participant label. The board enforces uniqueness.
    ParticipantName, "invalid participant name",
    |value: &str| value.len() <= 64 && segment(value)
);

name!(
    /// A channel name available to participants and ordinary owner operations.
    ChannelName, "invalid or reserved channel name",
    |value: &str| !value.starts_with("imported/")
        && channel_segments(value, |part, _| segment(part))
);

name!(
    /// A channel pattern with whole-segment wildcards.
    ChannelPattern, "invalid channel pattern",
    |value: &str| channel_segments(value, |part, last| {
        part == "*" || (last && part == "**") || segment(part)
    })
);

fn channel_segments(value: &str, valid: impl Fn(&str, bool) -> bool) -> bool {
    if value.len() > MAX_CHANNEL_BYTES {
        return false;
    }
    let mut segments = value.split('/').enumerate().peekable();
    while let Some((index, part)) = segments.next() {
        if index == MAX_SEGMENTS || !valid(part, segments.peek().is_none()) {
            return false;
        }
    }
    true
}

impl ChannelPattern {
    /// Check a public channel against this pattern. A trailing `**` needs at least one segment.
    pub fn matches(&self, channel: &ChannelName) -> bool {
        let mut names = channel.as_str().split('/');
        for part in self.as_str().split('/') {
            let Some(name) = names.next() else {
                return false;
            };
            match part {
                "**" => return true,
                "*" => {}
                literal if literal == name => {}
                _ => return false,
            }
        }
        names.next().is_none()
    }
}

name!(
    /// A sender-chosen identifier scoped to the sender and channel creation.
    MessageId, "invalid message ID",
    |value: &str| (1..=128).contains(&value.len()) && value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
    })
);
