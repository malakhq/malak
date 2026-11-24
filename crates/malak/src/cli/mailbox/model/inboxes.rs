//! Participant activity and waiting inboxes derived from the mailbox model.

use super::{Address, Channel, Model, Participant, Preview, Selection, inbox_recipient, involves};

pub(in crate::cli::mailbox) struct Inbox<'a> {
    model: &'a Model,
    pub channel: &'a Channel,
    pub name: &'a str,
    pub alias: bool,
    pub first_due: u64,
    pub oldest_ms: Option<u64>,
}

impl Inbox<'_> {
    pub fn is_worker(&self) -> bool {
        !self.alias && self.channel.name != "lead"
    }

    pub fn addresses(&self) -> impl Iterator<Item = Address> + '_ {
        let worker = self.is_worker();
        (self.first_due..self.channel.head)
            .map(|position| self.channel.address(position))
            .filter(move |address| !worker || self.model.possible_lead_work(address))
    }
}

pub(in crate::cli::mailbox) struct Lane<'a> {
    pub participant: &'a Participant,
    pub inbox: String,
    pub exists: bool,
    pub waiting: u64,
    pub active_ms: Option<u64>,
}

impl Model {
    pub fn lanes(&self) -> Vec<Lane<'_>> {
        let mut lanes = Vec::new();
        for person in self.people() {
            let inbox = person.inbox();
            let channel = self.channels.get(&inbox);
            let acked = channel.map_or(0, |channel| channel.acked_by(&person.id));
            let waiting = channel
                .filter(|_| !person.revoked)
                .map_or(0, |channel| channel.waiting(acked));
            let active_ms = self
                .history
                .values()
                .rev()
                .filter(|event| event.event != "owner" && involves(event, person, &inbox))
                .find_map(|event| event.at_ms)
                .max(self.last_message(&person.id));
            lanes.push(Lane {
                participant: person,
                inbox,
                exists: channel.is_some(),
                waiting,
                active_ms,
            });
        }
        lanes
    }

    pub fn lanes_by_activity(&self) -> Vec<Lane<'_>> {
        let mut lanes = self.lanes();
        lanes.sort_by_key(|lane| {
            (
                lane.participant.id != "lead",
                std::cmp::Reverse(lane.active_ms),
            )
        });
        lanes
    }

    pub fn due_inboxes(&self) -> Vec<Inbox<'_>> {
        let mut inboxes = self
            .channels
            .values()
            .filter_map(|channel| {
                let person = if channel.name == "lead" {
                    self.participants.iter().find(|person| person.id == "lead")
                } else {
                    channel.name.strip_prefix("to/")?;
                    inbox_recipient(&self.participants, &channel.name)
                };
                let included = person.map_or(matches!(self.selection, Selection::All), |person| {
                    !person.revoked && self.selection.includes(person)
                });
                let acked = person.map_or_else(
                    || channel.slowest_ack(),
                    |person| channel.acked_by(&person.id),
                );
                let first_due = channel.first_due(acked);
                if !included || first_due >= channel.head {
                    return None;
                }
                let alias = person.is_none();
                let name = person.map_or_else(
                    || {
                        let name = channel.name.strip_prefix("to/").unwrap_or(&channel.name);
                        if name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                            "Unknown inbox"
                        } else {
                            name
                        }
                    },
                    |person| person.name.as_str(),
                );
                let mut inbox = Inbox {
                    model: self,
                    channel,
                    name,
                    alias,
                    first_due,
                    oldest_ms: None,
                };
                let address = inbox.addresses().next()?;
                inbox.oldest_ms = self
                    .stored(&address)
                    .and_then(|event| event.at_ms)
                    .or_else(|| self.fetched(&address).map(Preview::at_ms));
                Some(inbox)
            })
            .collect::<Vec<_>>();
        inboxes.sort_by_key(|inbox| (inbox.oldest_ms, inbox.name, inbox.channel.name.as_str()));
        inboxes
    }

    fn possible_lead_work(&self, address: &Address) -> bool {
        // A status can arrive before the retained label. Keep showing that
        // position until the activity or its preview identifies its sender.
        if let Some(sender) = self.stored(address).and_then(|event| event.id.as_deref()) {
            return sender == "lead";
        }
        self.resolved_preview(address)
            .is_none_or(|preview| preview.is_some_and(|message| message.sender == "lead"))
    }
}
