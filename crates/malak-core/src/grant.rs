use crate::{Action, ChannelName, ChannelPattern};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use minicbor::Decoder;
use minicbor::Encoder;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

const ACTIONS: [Action; 5] = [
    Action::Send,
    Action::Read,
    Action::Get,
    Action::Ack,
    Action::Create,
];

pub(crate) const fn flag(action: Action) -> u8 {
    match action {
        Action::Send => 1 << 0,
        Action::Read => 1 << 1,
        Action::Get => 1 << 2,
        Action::Ack => 1 << 3,
        Action::Create => 1 << 4,
    }
}

/// One channel pattern and the actions granted on it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrantRule {
    pattern: ChannelPattern,
    actions: u8,
}

impl GrantRule {
    /// The set of channels covered by this rule.
    pub fn pattern(&self) -> &ChannelPattern {
        &self.pattern
    }

    /// The actions covered by this rule.
    pub fn actions(&self) -> impl Iterator<Item = Action> + '_ {
        ACTIONS.into_iter().filter(|&action| self.allows(action))
    }

    fn allows(&self, action: Action) -> bool {
        self.actions & flag(action) != 0
    }
}

/// The channel permissions and optional expiry the owner assigns to a participant.
///
/// An empty grant allows no actions. Matching rules are additive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Grant {
    rules: Vec<GrantRule>,
    ends: Option<SystemTime>,
}

impl Grant {
    /// Grant the given actions on channels matching this pattern.
    pub fn allow(&mut self, pattern: ChannelPattern, actions: impl IntoIterator<Item = Action>) {
        let actions = actions
            .into_iter()
            .map(flag)
            .fold(0, |left, right| left | right);
        if actions != 0 {
            self.rules.push(GrantRule { pattern, actions });
        }
    }

    /// The rules in this grant.
    pub fn rules(&self) -> &[GrantRule] {
        &self.rules
    }

    /// Set the time at which the grant expires, or clear its expiry.
    pub fn set_end(&mut self, ends: Option<SystemTime>) {
        self.ends = ends;
    }

    /// The time at which the grant expires, if any.
    pub fn ends(&self) -> Option<SystemTime> {
        self.ends
    }

    /// Whether the grant has ended at the supplied time.
    pub fn expired_at(&self, now: SystemTime) -> bool {
        self.ends.is_some_and(|end| now >= end)
    }

    /// Whether the rules cover this action on this channel.
    ///
    /// The call path checks expiry when checking the caller's validity.
    pub fn covers(&self, action: Action, channel: &ChannelName) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.allows(action) && rule.pattern.matches(channel))
    }
}

pub(crate) fn write_grant(encoder: &mut Encoder<Vec<u8>>, grant: &Grant) {
    encoder.array(2).unwrap();
    match grant.ends() {
        Some(ends) => write_time(encoder, ends),
        None => {
            encoder.null().unwrap();
        }
    }
    encoder.array(grant.rules().len() as u64).unwrap();
    for rule in grant.rules() {
        encoder
            .array(2)
            .unwrap()
            .str(rule.pattern().as_str())
            .unwrap();
        let actions = rule.actions().map(flag).fold(0, |all, action| all | action);
        encoder.u8(actions).unwrap();
    }
}

fn write_time(encoder: &mut Encoder<Vec<u8>>, time: SystemTime) {
    let span = time.duration_since(UNIX_EPOCH);
    let past = span.is_err();
    let length = span.unwrap_or_else(|error| error.duration());
    encoder
        .array(3)
        .unwrap()
        .bool(past)
        .unwrap()
        .u64(length.as_secs())
        .unwrap()
        .u32(length.subsec_nanos())
        .unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn read_grant(mut input: Decoder<'_>) -> Result<Grant, crate::Error> {
    decode_grant(&mut input).ok_or_else(|| crate::Error::refused("invalid grant"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn decode_grant(input: &mut Decoder<'_>) -> Option<Grant> {
    if input.array().ok()? != Some(2) {
        return None;
    }
    let mut grant = Grant::default();
    if input.datatype().ok()? == minicbor::data::Type::Null {
        input.null().ok()?;
    } else {
        if input.array().ok()? != Some(3) {
            return None;
        }
        let past = input.bool().ok()?;
        let seconds = input.u64().ok()?;
        let nanos = input.u32().ok()?;
        if nanos >= 1_000_000_000 {
            return None;
        }
        let span = Duration::new(seconds, nanos);
        grant.set_end(Some(if past {
            UNIX_EPOCH.checked_sub(span)?
        } else {
            UNIX_EPOCH.checked_add(span)?
        }));
    }
    for _ in 0..input.array().ok()?? {
        if input.array().ok()? != Some(2) {
            return None;
        }
        let pattern = input.str().ok()?.parse().ok()?;
        let actions = input.u8().ok()?;
        if actions == 0 || actions & !0b1_1111 != 0 {
            return None;
        }
        grant.allow(
            pattern,
            ACTIONS
                .into_iter()
                .filter(|&action| actions & flag(action) != 0),
        );
    }
    Some(grant)
}
