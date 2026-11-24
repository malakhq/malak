use crate::{CreationId, Message};

/// The answer to a request for one position.
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
#[expect(
    clippy::large_enum_variant,
    reason = "Keep verified messages inline on the get path"
)]
pub enum GetReply {
    /// A verified message occupying that position.
    Message(Message),
    /// Retention removed the position.
    Trimmed {
        /// Channel creation that was queried.
        creation: CreationId,
        /// First position retention has not removed.
        before: u64,
    },
    /// The owner marked this position void.
    Void {
        /// Channel creation that was queried.
        creation: CreationId,
    },
    /// The channel has never assigned this position.
    NeverWritten {
        /// Channel creation that was queried.
        creation: CreationId,
    },
    /// No creation of the channel exists.
    Absent,
}

/// The due messages and channel boundaries returned by a read.
#[derive(Debug)]
#[non_exhaustive]
pub struct ReadReply {
    /// Messages in delivery order, oldest first within each channel.
    pub messages: Vec<Message>,
    /// The named channels and their reported boundaries.
    pub channels: Vec<ReadChannel>,
    /// The grant changed before the board could return all due messages.
    pub held_back: bool,
}

/// The channel state reported with a read.
#[derive(Debug)]
#[non_exhaustive]
pub struct ReadChannel {
    /// Name the reader gave.
    pub channel: crate::ChannelName,
    /// Current creation, or `None` for an absent channel.
    pub creation: Option<CreationId>,
    /// The retention boundary, when unread positions below it were lost.
    pub trimmed_before: Option<u64>,
    /// Number of void positions passed in this read.
    pub voids: u64,
}
