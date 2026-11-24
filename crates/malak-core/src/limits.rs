use crate::{ChannelPattern, RetentionRule, SenderShareRule};
use malak_layers::Error;
use std::time::Duration;

/// Configurable limits on participant calls, storage, and replies.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum message body length in bytes.
    pub body_bytes: usize,
    /// Maximum number of user-supplied headers per message.
    pub header_entries: usize,
    /// Maximum combined size of user-supplied header keys and values.
    pub header_bytes: usize,
    /// Maximum number of channel names in one read.
    pub read_channels: usize,
    /// Maximum number of messages returned by one read.
    pub read_messages: usize,
    /// Maximum combined size of the message frames in one read reply.
    pub read_reply_bytes: usize,
    /// Maximum wait on a read.
    pub read_wait: Duration,
    /// Counted calls per participant per minute.
    pub calls_per_minute: usize,
    /// Counted calls per minute for the lead.
    pub lead_calls_per_minute: usize,
    /// Maximum bytes stored by one participant in one day.
    pub daily_storage_bytes: u64,
    /// Maximum total bytes of messages and their board records.
    pub board_storage_bytes: u64,
    /// Percentage of board storage reserved for the lead.
    pub lead_storage_percent: u8,
    /// Minimum free disk space for participants other than the lead to store.
    pub minimum_free_disk_bytes: u64,
    /// Maximum current channels on the board.
    pub board_channels: usize,
    /// Channel places reserved for the lead and owner.
    pub reserved_channels: usize,
    /// Maximum current channels created by one participant.
    pub participant_channels: usize,
    /// Maximum number of readers for one channel creation.
    pub channel_readers: usize,
    /// Maximum combined size of queued message frames on one connection.
    pub outgoing_message_bytes: usize,
    /// Maximum number of simultaneous data requests on one connection.
    pub open_data_requests: usize,
    /// Closing reply places on one connection.
    pub closing_reply_places: usize,
    /// Channel retention rules. The last matching rule takes precedence.
    pub retention: Vec<RetentionRule>,
    /// Opt-in per-sender shares and protected readers for matching channels.
    pub sender_shares: Vec<SenderShareRule>,
    pub(crate) default_inbox_limit: Option<InboxLimit>,
    pub(crate) inbox_overrides: Vec<(ChannelPattern, InboxLimit)>,
}

#[derive(Clone, Debug)]
pub(crate) struct InboxLimit {
    pub(crate) max_messages: usize,
    pub(crate) max_per_sender: Option<usize>,
}

impl InboxLimit {
    fn new(max_messages: usize, max_per_sender: Option<usize>) -> Result<Self, Error> {
        if max_messages == 0 || max_per_sender == Some(0) {
            return Err(Error::refused("inbox limits must be positive"));
        }
        Ok(Self {
            max_messages,
            max_per_sender,
        })
    }
}

impl Default for Limits {
    fn default() -> Self {
        const MIB: usize = 1024 * 1024;
        const GIB: u64 = 1024 * 1024 * 1024;

        let wire = malak_proto::ConnectionLimits::default();
        Self {
            body_bytes: wire.body_bytes,
            header_entries: 16,
            header_bytes: 2 * 1024,
            read_channels: 64,
            read_messages: 100,
            read_reply_bytes: 8 * MIB,
            read_wait: Duration::from_secs(3600),
            calls_per_minute: 600,
            lead_calls_per_minute: 6_000,
            daily_storage_bytes: 256 * MIB as u64,
            board_storage_bytes: 10 * GIB,
            lead_storage_percent: 5,
            minimum_free_disk_bytes: GIB,
            board_channels: 10_000,
            reserved_channels: 500,
            participant_channels: 100,
            channel_readers: 1_000,
            outgoing_message_bytes: wire.message_bytes,
            open_data_requests: wire.open_data_requests,
            closing_reply_places: wire.reply_places,
            retention: Vec::new(),
            sender_shares: Vec::new(),
            default_inbox_limit: None,
            inbox_overrides: Vec::new(),
        }
    }
}

impl Limits {
    /// Set the admission cap for `lead` and canonical `to/<id>` inboxes
    /// that have no matching override.
    ///
    /// A missing per-sender cap lets any one sender use the whole inbox.
    /// Returns `refused` for a zero cap.
    pub fn set_default_inbox_limit(
        &mut self,
        max_messages: usize,
        max_per_sender: Option<usize>,
    ) -> Result<(), Error> {
        self.default_inbox_limit = Some(InboxLimit::new(max_messages, max_per_sender)?);
        Ok(())
    }

    /// Cap unacknowledged messages in matching channels, independently of retention.
    ///
    /// The last matching override takes precedence. Each channel counts messages
    /// due to its inbox owner, or to the lead for channels without an inbox owner.
    /// A missing per-sender cap lets any one sender use the whole channel cap.
    /// Returns `refused` for a zero cap.
    pub fn limit_inbox(
        &mut self,
        pattern: ChannelPattern,
        max_messages: usize,
        max_per_sender: Option<usize>,
    ) -> Result<(), Error> {
        let limit = InboxLimit::new(max_messages, max_per_sender)?;
        self.inbox_overrides.push((pattern, limit));
        Ok(())
    }

    /// Verify that a largest allowed message can be queued and closed.
    ///
    /// Returns `refused` for inconsistent limits.
    pub fn validate(&self) -> Result<(), Error> {
        let frame_bytes = malak_proto::max_frame_bytes(self.body_bytes)
            .ok_or_else(|| Error::refused("largest message frame exceeds the size limit"))?;
        if frame_bytes > self.read_reply_bytes || self.read_messages == 0 {
            return Err(Error::refused("largest message must fit a read reply"));
        }
        if self.read_reply_bytes > self.outgoing_message_bytes {
            return Err(Error::refused("read reply must fit the outgoing queue"));
        }
        if self.closing_reply_places <= self.open_data_requests {
            return Err(Error::refused("not enough places for closing replies"));
        }
        if self.lead_storage_percent > 100 || self.reserved_channels > self.board_channels {
            return Err(Error::refused("lead reserves exceed board capacity"));
        }
        for rule in &self.sender_shares {
            rule.validate()?;
        }
        Ok(())
    }
}
