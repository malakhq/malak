use super::{Channel, History};
use crate::{ChannelName, ChannelPattern, Error, Limits, ParticipantId, Position};
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound::{Excluded, Included};

/// A per-sender share and protected reader for matching channels.
///
/// The last matching rule takes precedence. For an inbox, the protected
/// reader is its owner. Other channels require an explicit reader.
#[derive(Clone, Debug)]
pub struct SenderShareRule {
    /// Names covered by the rule.
    pub pattern: ChannelPattern,
    /// Protected reader for channels other than inboxes.
    pub reader: Option<ParticipantId>,
    /// Maximum retained messages before the channel needs to trim.
    pub channel_messages: usize,
    /// Maximum messages from one sender still due to the designated reader.
    pub sender_messages: usize,
    /// Maximum record bytes from one sender still due to the designated reader.
    pub sender_bytes: u64,
}

impl SenderShareRule {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.sender_messages == 0
            || self.sender_messages >= self.channel_messages
            || self.sender_bytes == 0
        {
            return Err(Error::refused(
                "sender shares must leave channel room for another sender",
            ));
        }
        if self.reader.is_none()
            && !matches!(self.pattern.as_str(), "lead" | "to/*")
            && inbox_owner(self.pattern.as_str()).is_none()
        {
            return Err(Error::refused("sender share requires a designated reader"));
        }
        Ok(())
    }
}

pub(super) struct Policy {
    reader: ParticipantId,
    sender_messages: Option<usize>,
    sender_bytes: Option<u64>,
    max_waiting: Option<usize>,
}

impl Policy {
    pub(super) fn for_inbox(limits: &Limits, channel: &ChannelName) -> Option<Self> {
        let override_limit = limits
            .inbox_overrides
            .iter()
            .rev()
            .find_map(|(pattern, limit)| pattern.matches(channel).then_some(limit));
        let (limit, reader) = if let Some(limit) = override_limit {
            (
                limit,
                inbox_owner(channel.as_str()).unwrap_or_else(ParticipantId::lead),
            )
        } else {
            (
                limits.default_inbox_limit.as_ref()?,
                inbox_owner(channel.as_str())?,
            )
        };
        Some(Self {
            reader,
            sender_messages: limit.max_per_sender,
            sender_bytes: None,
            max_waiting: Some(limit.max_messages),
        })
    }

    pub(super) fn for_channel(
        rules: &[SenderShareRule],
        channel: &ChannelName,
    ) -> Option<(Self, usize)> {
        let rule = rules
            .iter()
            .rev()
            .find(|rule| rule.pattern.matches(channel))?;
        let reader = inbox_owner(channel.as_str()).or_else(|| rule.reader.clone())?;
        Some((
            Self {
                reader,
                sender_messages: Some(rule.sender_messages),
                sender_bytes: Some(rule.sender_bytes),
                max_waiting: None,
            },
            rule.channel_messages,
        ))
    }

    pub(super) fn protects_retention(&self) -> bool {
        self.max_waiting.is_none()
    }
}

fn inbox_owner(channel: &str) -> Option<ParticipantId> {
    if channel == "lead" {
        return Some(ParticipantId::lead());
    }
    let id: ParticipantId = channel.strip_prefix("to/")?.parse().ok()?;
    (!id.is_lead()).then_some(id)
}

#[derive(Default)]
pub(super) struct Ledger {
    positions: BTreeMap<u64, (ParticipantId, u64)>,
    senders: HashMap<ParticipantId, Usage>,
    saved: u64,
    waiting: usize,
}

#[derive(Clone, Copy, Default)]
struct Usage {
    messages: usize,
    bytes: u64,
}

impl Ledger {
    pub(super) fn record(&mut self, at: u64, sender: &ParticipantId, bytes: u64) {
        self.positions.insert(at, (sender.clone(), bytes));
        if at > self.saved {
            self.waiting += 1;
            let usage = self.senders.entry(sender.clone()).or_default();
            usage.messages += 1;
            usage.bytes += bytes;
        }
    }

    pub(super) fn remove(&mut self, at: u64) {
        if let Some((sender, bytes)) = self.positions.remove(&at)
            && at > self.saved
        {
            Self::release(&mut self.senders, &mut self.waiting, &sender, bytes);
        }
    }

    pub(super) fn permits_trim_through(&self, floor: u64) -> u64 {
        floor.min(self.saved + 1)
    }

    fn acknowledge(&mut self, through: u64) {
        if through > self.saved {
            for (_, (sender, bytes)) in self
                .positions
                .range((Excluded(self.saved), Included(through)))
            {
                Self::release(&mut self.senders, &mut self.waiting, sender, *bytes);
            }
            self.saved = through;
        }
    }

    fn release(
        senders: &mut HashMap<ParticipantId, Usage>,
        waiting: &mut usize,
        sender: &ParticipantId,
        bytes: u64,
    ) {
        *waiting -= 1;
        let usage = senders.get_mut(sender).expect("tracked sender");
        usage.messages -= 1;
        usage.bytes -= bytes;
        if usage.messages == 0 {
            senders.remove(sender);
        }
    }
}

impl History {
    pub(crate) fn admit(
        &self,
        channel: &Channel,
        sender: &ParticipantId,
        bytes: u64,
    ) -> Result<(), Error> {
        let (Some(policy), Some(ledger)) = (&channel.share, &self.shares) else {
            return Ok(());
        };
        if let Some(limit) = policy.sender_messages {
            let usage = ledger.senders.get(sender).copied().unwrap_or_default();
            if usage.messages >= limit
                || policy
                    .sender_bytes
                    .is_some_and(|limit| usage.bytes.saturating_add(bytes) > limit)
            {
                let byte_usage = policy.sender_bytes.map_or_else(String::new, |limit| {
                    format!(", {} of {limit} sender bytes", usage.bytes)
                });
                return Err(Error::unavailable(format!(
                    "share_full ({} of {limit} sender places{byte_usage}, {} messages waiting for the reader). Wait for the reader, then retry with the same message ID.",
                    usage.messages, ledger.waiting,
                )));
            }
        }
        let (occupancy, limit, full) = if let Some(limit) = policy.max_waiting {
            (ledger.waiting, limit, ledger.waiting >= limit)
        } else {
            let occupancy = ledger.positions.len();
            let limit = channel.retention.messages;
            let over = (occupancy + 1).saturating_sub(limit);
            let count_trim_blocked = over > occupancy
                || over
                    .checked_sub(1)
                    .and_then(|last| ledger.positions.keys().nth(last))
                    .is_some_and(|position| *position > ledger.saved);
            (occupancy, limit, count_trim_blocked)
        };
        if full {
            return Err(Error::unavailable(format!(
                "inbox_full ({occupancy} of {limit} channel places, {} messages waiting for the reader). Wait for the reader, then retry with the same message ID.",
                ledger.waiting,
            )));
        }
        Ok(())
    }

    pub(crate) fn note_ack(
        &mut self,
        channel: &Channel,
        reader: &ParticipantId,
        position: Position,
    ) {
        if let (Some(policy), Some(ledger)) = (&channel.share, &mut self.shares)
            && &policy.reader == reader
        {
            ledger.acknowledge(position.get());
        }
    }

    pub(super) fn keep_record(
        &mut self,
        sender: &ParticipantId,
        bytes: u64,
        stored: std::time::Instant,
    ) {
        if let Some(ledger) = &mut self.shares {
            ledger.record(self.head, sender, bytes);
        }
        self.head += 1;
        self.retained.push_back(super::Kept {
            stored,
            bytes,
            lead: sender.is_lead(),
        });
    }

    pub(super) fn release_prefix(&mut self, count: usize, quota: &mut crate::quota::Quota) {
        for (index, record) in self.retained.drain(..count).enumerate() {
            if let Some(ledger) = &mut self.shares {
                ledger.remove(self.floor + index as u64);
            }
            quota.release_space(record.lead, record.bytes);
        }
    }
}
