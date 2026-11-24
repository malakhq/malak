use crate::{ChannelName, ChannelPattern};
use std::time::Duration;

/// Retention limits for every channel matching one pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionRule {
    /// Channels covered by this rule.
    pub pattern: ChannelPattern,
    /// Maximum number of messages to keep in a channel.
    pub messages: usize,
    /// Maximum age of an untrimmed message.
    pub age: Duration,
}

#[derive(Clone, Copy)]
pub(crate) struct Retention {
    pub(crate) messages: usize,
    pub(crate) age: Duration,
}

impl Retention {
    pub(crate) fn for_channel(rules: &[RetentionRule], channel: &ChannelName) -> Self {
        rules
            .iter()
            .rev()
            .find(|rule| rule.pattern.matches(channel))
            .map_or(
                Self {
                    messages: 100_000,
                    age: Duration::from_secs(30 * 24 * 60 * 60),
                },
                |rule| Self {
                    messages: rule.messages,
                    age: rule.age,
                },
            )
    }
}
