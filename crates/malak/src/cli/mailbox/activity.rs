//! Delivery state and refusal counts for the live Activity rows.

use super::model::{Address, Channel, Event, EventKey};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) fn is_self_test(channel: &str) -> bool {
    channel.starts_with("selftest/")
}

pub(super) fn shows(event: &Event) -> bool {
    match event.event.as_str() {
        "stored" => !event.channel.as_deref().is_some_and(is_self_test),
        "denied" | "limit_hit" | "owner" | "trimmed" => true,
        _ => false,
    }
}

#[derive(Default)]
pub(super) struct Refusals(HashMap<Refusal, RefusalRow>);

#[derive(Eq, Hash, PartialEq)]
struct Refusal {
    participant: Option<String>,
    verb: Option<String>,
    channel: Option<String>,
    reason: Option<String>,
}

#[derive(Default)]
struct RefusalRow {
    key: Option<EventKey>,
    denied: u64,
    limited: u64,
}

impl Refusals {
    pub fn fold(&mut self, key: &EventKey, event: &mut Event) -> Option<EventKey> {
        if !matches!(event.event.as_str(), "denied" | "limit_hit") {
            return None;
        }
        let row = self
            .0
            .entry(Refusal {
                participant: event.id.clone(),
                verb: event.operation.clone(),
                channel: event.channel.clone(),
                reason: event.reason.clone(),
            })
            .or_default();
        let count = if event.event == "denied" {
            &mut row.denied
        } else {
            &mut row.limited
        };
        *count = count.saturating_add(event.count.unwrap_or(1));
        // A refusal at a limit can be emitted in both streams.
        event.count = Some(row.denied.max(row.limited));
        row.key.replace(key.clone())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Delivery {
    Waiting,
    Read(u64),
    Acknowledged(Option<u64>),
    Revoked,
}

#[derive(Default)]
pub(super) struct Receipts {
    reads: HashMap<Address, u64>,
    // A channel creation identifies one acknowledgement cursor, even if its name is reused.
    acknowledgements: HashMap<String, Acknowledgements>,
    revoked: HashSet<String>,
}

#[derive(Default)]
struct Acknowledgements {
    at_open: u64,
    confirmed: u64,
    observed: BTreeMap<u64, u64>,
}

impl Receipts {
    pub fn new<'a>(channels: impl Iterator<Item = &'a Channel>) -> Self {
        let mut receipts = Self::default();
        receipts.refresh(channels);
        for acks in receipts.acknowledgements.values_mut() {
            acks.at_open = acks.confirmed;
        }
        receipts
    }

    pub fn refresh<'a>(&mut self, channels: impl Iterator<Item = &'a Channel>) {
        for channel in channels {
            if let Some(position) = channel.readers().map(|(_, position)| position).max() {
                let acks = self
                    .acknowledgements
                    .entry(channel.creation.clone())
                    .or_default();
                acks.confirmed = acks.confirmed.max(position);
            }
        }
    }

    pub fn observe(&mut self, event: &Event) {
        if event.operation.as_deref() == Some("REVOKE")
            && event.event == "owner"
            && event.outcome.as_deref() == Some("done")
            && let Some(id) = &event.id
        {
            self.revoked.insert(id.clone());
        }
        if !matches!(event.event.as_str(), "read" | "acked") {
            return;
        }
        let (Some(address), Some(at)) = (event.record_address(), event.at_ms) else {
            return;
        };
        if is_self_test(&address.channel) {
            return;
        }
        if event.event == "read" {
            self.reads
                .entry(address)
                .and_modify(|previous| *previous = (*previous).max(at))
                .or_insert(at);
        } else {
            let acks = self.acknowledgements.entry(address.creation).or_default();
            // ACK is cumulative. An earlier endpoint can already have been covered
            // by another reader's ACK without changing the message's delivery state.
            if acks.observed.range(address.position..).next().is_none() {
                acks.observed.insert(address.position, at);
            }
        }
    }

    pub fn delivery(&self, address: &Address) -> Delivery {
        if address
            .channel
            .strip_prefix("to/")
            .is_some_and(|id| self.revoked.contains(id))
        {
            return Delivery::Revoked;
        }
        if let Some(acks) = self.acknowledgements.get(&address.creation) {
            if let Some((&through, &at)) = acks.observed.range(address.position..).next() {
                // Preserve an observed event for its exact endpoint if it raced
                // the initial status. Other already-confirmed positions have no age.
                let dated = address.position > acks.at_open || address.position == through;
                return Delivery::Acknowledged(dated.then_some(at));
            }
            if address.position <= acks.confirmed {
                return Delivery::Acknowledged(None);
            }
        }
        self.reads
            .get(address)
            .map_or(Delivery::Waiting, |&at| Delivery::Read(at))
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::tests::{event, status};
    use super::*;

    fn observe(receipts: &mut Receipts, kind: &str, creation: &str, position: u64, at: u64) {
        let mut receipt = event(kind, "lead", "lead", creation, position);
        receipt.at_ms = Some(at);
        receipts.observe(&receipt);
    }

    fn message(creation: &str, position: u64) -> Address {
        event("stored", "lead", "lead", creation, position)
            .address()
            .unwrap()
    }

    #[test]
    fn a_cumulative_ack_covers_earlier_positions() {
        let mut receipts = Receipts::default();
        observe(&mut receipts, "acked", "first", 3, 60_000);

        assert_eq!(
            receipts.delivery(&message("first", 2)),
            Delivery::Acknowledged(Some(60_000))
        );
        assert_eq!(receipts.delivery(&message("first", 4)), Delivery::Waiting);
    }

    #[test]
    fn a_recreated_channel_does_not_inherit_receipts() {
        let mut receipts = Receipts::default();
        observe(&mut receipts, "acked", "first", 3, 60_000);

        assert_eq!(
            receipts.delivery(&message("replacement", 3)),
            Delivery::Waiting
        );
    }

    #[test]
    fn a_startup_acknowledgement_has_no_invented_age() {
        let mut receipts = Receipts::new(status().channels.iter());
        observe(&mut receipts, "acked", "first", 6, 60_000);

        assert_eq!(
            receipts.delivery(&message("first", 4)),
            Delivery::Acknowledged(None)
        );
    }
}
