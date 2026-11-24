//! Schedule reminders from the stored labels already carried by history and tail.

use super::model::{Address, Event, Model, Participant, Status};
use crate::cli::output;
use malak::ParticipantId;
use std::collections::{BTreeMap, HashMap, HashSet};

const STUCK_AFTER_MS: u64 = 15 * 60 * 1_000;
const REPEAT_MS: u64 = 60 * 60 * 1_000;
const MARKER: &str = "stuck";

pub(super) struct Watch {
    pending: BTreeMap<Address, Work>,
    closed: HashMap<String, Option<Revocation>>,
}

struct Work {
    first_due: u64,
    next_due: u64,
    kind: String,
}

struct Revocation {
    at: u64,
    // Revocation removes the worker's reader cursor from STATUS. Keep the
    // cursor last observed before revocation for any history still loading.
    cursor: Option<(String, u64)>,
    reported: HashSet<(String, u64)>,
}

impl Watch {
    pub fn new(initial: &Status) -> Self {
        Self {
            pending: BTreeMap::new(),
            closed: initial
                .participants
                .iter()
                .filter(|person| person.revoked)
                .map(|person| (person.id.clone(), None))
                .collect(),
        }
    }

    pub fn stored(&mut self, address: Address, sender: &str, kind: &str, sent_at: u64) {
        if sender != "lead"
            || !address
                .channel
                .strip_prefix("to/")
                .and_then(|id| id.parse::<ParticipantId>().ok())
                .is_some_and(|id| !id.is_lead())
        {
            return;
        }
        let first_due = sent_at.saturating_add(STUCK_AFTER_MS);
        self.pending.entry(address).or_insert_with(|| Work {
            first_due,
            next_due: first_due,
            kind: kind.to_owned(),
        });
    }

    pub fn event(&mut self, model: &Model, event: &Event) {
        if let (Some(address), Some(sender), Some(kind), Some(at)) =
            (event.address(), &event.id, &event.kind, event.at_ms)
        {
            self.stored(address, sender, kind, at);
        }
        if event.operation.as_deref() == Some("REVOKE")
            && event.outcome.as_deref() == Some("done")
            && let (Some(id), Some(at)) = (&event.id, event.at_ms)
        {
            let revoked_at = model
                .participant(id)
                .and_then(|person| person.revoked_at_ms);
            self.revoke(model, id, revoked_at.unwrap_or(at));
        }
    }

    fn revoke(&mut self, model: &Model, id: &str, at: u64) {
        self.closed.entry(id.to_owned()).or_insert_with(|| {
            let cursor = model
                .worker_inbox(&format!("to/{id}"))
                .map(|(_, channel)| (channel.creation.clone(), channel.due_positions(id).start));
            Some(Revocation {
                at,
                cursor,
                reported: HashSet::new(),
            })
        });
    }

    pub fn report(&mut self, model: &Model, now: u64) -> (String, Option<u64>) {
        let mut blocks = String::new();
        let mut next = None;
        self.pending.retain(|address, work| {
            let id = address
                .channel
                .strip_prefix("to/")
                .expect("pending worker inbox");
            if let Some(closed) = self.closed.get_mut(id) {
                let Some(closed) = closed else { return false };
                if work.first_due >= closed.at
                    || closed.cursor.as_ref().is_some_and(|(creation, first)| {
                        address.creation != *creation || address.position < *first
                    })
                {
                    return false;
                }
                let Some(person) = model.participant(id) else {
                    // The tail can deliver an enrolment and revocation before
                    // the next status snapshot supplies the worker's name.
                    return true;
                };
                if closed
                    .reported
                    .insert((address.creation.clone(), address.position))
                {
                    block(&mut blocks, closed.at, person, address, work, "revoked");
                }
                return false;
            }
            let Some((person, channel)) = model.worker_inbox(&address.channel) else {
                return true;
            };
            if channel.creation != address.creation
                || !channel.due_positions(id).contains(&address.position)
            {
                return false;
            }
            if person.revoked {
                // Await the ordered revocation event so preceding ACK events
                // can still advance the cursor before it is used for closure.
                return true;
            }
            if now >= work.next_due {
                block(&mut blocks, now, person, address, work, "running");
                let elapsed = now - work.first_due;
                work.next_due = now.saturating_add(REPEAT_MS - elapsed % REPEAT_MS);
            }
            next = Some(next.map_or(work.next_due, |at: u64| at.min(work.next_due)));
            true
        });
        (blocks, next)
    }
}

fn block(
    out: &mut String,
    at: u64,
    person: &Participant,
    address: &Address,
    work: &Work,
    state: &str,
) {
    out.push_str(&output::event(
        Some(at),
        &format!(
            "{MARKER} {} {} #{} {} {state}",
            person.name, address.channel, address.position, work.kind
        ),
    ));
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::mailbox::model::tests::{event, status};
    use serde_json::json;

    const ID: &str = "a0123456";
    const INBOX: &str = "to/a0123456";
    const SENT: u64 = 10_000;
    const DUE: u64 = SENT + STUCK_AFTER_MS;
    const BLOCK: &str = "stuck worker to/a0123456 #1 task";

    fn setup(revoked: bool) -> (Model, Watch) {
        let mut snapshot = status();
        snapshot.participants[1].revoked = revoked;
        snapshot.channels[1].head = 3;
        let watch = Watch::new(&snapshot);
        let mut model = Model::new(snapshot, Some("lead".to_owned()));
        model.enable_lead_watch();
        (model, watch)
    }

    fn store(watch: &mut Watch, position: u64, at: u64) {
        let address = event("stored", "lead", INBOX, "second", position)
            .address()
            .unwrap();
        watch.stored(address, "lead", "task", at);
    }

    fn revoke(at: u64) -> Event {
        serde_json::from_value(json!({
            "event": "owner", "id": ID, "operation": "REVOKE",
            "outcome": "done", "at_ms": at, "missed": 0
        }))
        .unwrap()
    }

    fn check_block(block: &str, fields: &str) {
        let (timestamp, body) = block.split_once("  ").unwrap();
        assert!(timestamp.contains(':'), "{block}");
        assert_eq!(body, format!("{fields}\n\n"));
    }

    #[test]
    fn a_live_lead_message_is_stuck_after_fifteen_minutes_and_hourly() {
        let (mut model, mut watch) = setup(false);
        let mut stored = event("stored", "lead", INBOX, "second", 1);
        stored.kind = Some("task".into());
        stored.at_ms = Some(SENT);
        model.apply(stored.clone());
        watch.event(&model, &stored);

        assert_eq!(watch.report(&model, DUE - 1), (String::new(), Some(DUE)));
        let (first, next) = watch.report(&model, DUE);
        check_block(&first, &format!("{BLOCK} running"));
        assert_eq!(next, Some(DUE + REPEAT_MS));
        assert!(watch.report(&model, DUE + REPEAT_MS - 1).0.is_empty());
        let (second, _) = watch.report(&model, DUE + REPEAT_MS);
        check_block(&second, &format!("{BLOCK} running"));
    }

    #[test]
    fn restarting_prints_one_stuck_block_and_keeps_the_original_hourly_schedule() {
        let (model, mut watch) = setup(false);
        store(&mut watch, 1, SENT);
        let startup = DUE + 3 * REPEAT_MS + 250;

        let (block, next) = watch.report(&model, startup);
        check_block(&block, &format!("{BLOCK} running"));
        assert_eq!(next, Some(DUE + 4 * REPEAT_MS));
        assert!(watch.report(&model, startup).0.is_empty());
    }

    #[test]
    fn starting_skips_a_revoked_inbox() {
        let (model, mut watch) = setup(true);
        store(&mut watch, 1, SENT);

        assert_eq!(watch.report(&model, DUE), (String::new(), None));
    }

    #[test]
    fn revoking_reports_each_stuck_message_once_even_if_history_arrives_late() {
        let (model, mut watch) = setup(false);
        store(&mut watch, 1, SENT);
        assert!(watch.report(&model, DUE).0.contains("running"));
        let revoked = revoke(DUE + 50);

        watch.event(&model, &revoked);
        let (first, next) = watch.report(&model, DUE + 50);
        check_block(&first, &format!("{BLOCK} revoked"));
        assert_eq!(next, None);

        store(&mut watch, 1, SENT);
        store(&mut watch, 2, SENT);
        let (second, _) = watch.report(&model, DUE + 50);
        check_block(&second, "stuck worker to/a0123456 #2 task revoked");
        watch.event(&model, &revoked);
        assert_eq!(watch.report(&model, DUE + REPEAT_MS), (String::new(), None));
    }

    #[test]
    fn revoking_at_the_first_deadline_closes_the_work_silently() {
        let (model, mut watch) = setup(false);
        store(&mut watch, 1, SENT);

        watch.event(&model, &revoke(DUE));
        assert_eq!(watch.report(&model, DUE), (String::new(), None));
    }

    #[test]
    fn a_revocation_snapshot_does_not_overtake_an_ack_in_the_tail() {
        let (mut model, mut watch) = setup(false);
        store(&mut watch, 1, SENT);
        let mut snapshot = status();
        snapshot.participants[1].revoked = true;
        snapshot.channels[1].head = 3;

        model.update(snapshot);
        assert!(watch.report(&model, DUE).0.is_empty());
        model.apply(event("acked", ID, INBOX, "second", 1));
        watch.event(&model, &revoke(DUE));

        assert_eq!(watch.report(&model, DUE), (String::new(), None));
    }

    #[test]
    fn an_ack_stops_hourly_reminders() {
        let (mut model, mut watch) = setup(false);
        store(&mut watch, 1, SENT);
        assert!(watch.report(&model, DUE).0.contains("running"));

        model.apply(event("acked", ID, INBOX, "second", 1));
        assert_eq!(watch.report(&model, DUE + REPEAT_MS), (String::new(), None));
    }
}
