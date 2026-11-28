//! Inbox aliases belong to board authority, before grants are checked.

use super::Authority;
use crate::{Action, ChannelName, Error, ParticipantId};
use strsim::levenshtein;

pub(super) fn check_new_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > 24
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || name.contains("--")
        || name.ends_with('-')
    {
        return Err(Error::refused(
            "new participant names must be 1 to 24 lowercase letters, digits or hyphens, start with a letter, and have no double or trailing hyphen",
        ));
    }
    Ok(())
}

impl Authority {
    pub(crate) fn resolve_channel(
        &self,
        channel: &ChannelName,
        viewer: Option<&ParticipantId>,
    ) -> Result<ChannelName, Error> {
        let Some(address) = channel.as_str().strip_prefix("to/") else {
            return Ok(channel.clone());
        };
        // IDs, including retired enrolments, keep their exact historical meaning.
        if let Some((id, _)) = self.participants.get_key_value(address) {
            return Ok(id.inbox());
        }
        if let Some(id) = self.names.get(address) {
            return Ok(id.inbox());
        }
        let nearest = self
            .participants
            .iter()
            .filter(|(id, _)| {
                viewer.is_none_or(|viewer| {
                    let inbox = id.inbox();
                    [Action::Send, Action::Read, Action::Get, Action::Ack]
                        .into_iter()
                        .any(|action| self.allow(viewer, action, &inbox).is_ok())
                })
            })
            .flat_map(|(id, participant)| {
                let by_id = (levenshtein(address, id.as_str()), id.inbox().to_string());
                let by_name = (!participant.revoked).then(|| {
                    (
                        levenshtein(address, participant.name.as_str()),
                        format!("to/{} ({})", participant.name, id.inbox()),
                    )
                });
                std::iter::once(by_id).chain(by_name)
            })
            .min();
        let suggestion = nearest.map_or_else(String::new, |(_, inbox)| {
            format!("; no enrolled participant named {address:?} is active and no participant ID matches; nearest inbox: {inbox}")
        });
        Err(Error::refused(format!(
            "unknown inbox {channel}{suggestion}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Grant, ParticipantId};

    fn legacy_name_resolves(name: &str) {
        let mut authority = Authority::new(ParticipantId::lead(), None);
        let id: ParticipantId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        authority.insert(id.clone(), name.parse().unwrap(), Grant::default(), None);

        assert_eq!(
            authority
                .resolve_channel(&format!("to/{name}").parse().unwrap(), None)
                .unwrap(),
            id.inbox()
        );
    }

    #[test]
    fn a_grandfathered_underscore_name_still_resolves() {
        legacy_name_resolves("old_reviewer");
    }

    #[test]
    fn a_grandfathered_long_name_still_resolves() {
        legacy_name_resolves(&"a".repeat(64));
    }

    #[test]
    fn an_existing_id_wins_over_a_legacy_participant_name() {
        let mut authority = Authority::new(ParticipantId::lead(), None);
        let original: ParticipantId = "0123456789abcdef0123456789abcdef".parse().unwrap();
        authority.insert(
            original.clone(),
            "first".parse().unwrap(),
            Grant::default(),
            None,
        );
        authority.insert(
            "fedcba9876543210fedcba9876543210".parse().unwrap(),
            original.as_str().parse().unwrap(),
            Grant::default(),
            None,
        );

        assert_eq!(
            authority.resolve_channel(&original.inbox(), None).unwrap(),
            original.inbox()
        );
    }
}
