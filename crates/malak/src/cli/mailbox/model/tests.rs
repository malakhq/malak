use super::*;
use serde_json::json;

pub fn status() -> Status {
    serde_json::from_value(json!({
        "run": "run",
        "participants": [
            {"id": "lead", "name": "lead", "connections": 1, "revoked": false},
            {"id": "a0123456", "name": "worker", "connections": 0, "revoked": false},
        ],
        "channels": [
            {"name": "lead", "creation": "first", "floor": 3, "head": 10, "readers": {"lead": 4}},
            {"name": "to/a0123456", "creation": "second", "floor": 1, "head": 1, "readers": {}},
        ]
    }))
    .unwrap()
}

pub fn event(event: &str, id: &str, channel: &str, creation: &str, position: u64) -> Event {
    serde_json::from_value(json!({
        "event": event, "id": id, "channel": channel, "creation": creation,
        "position": position, "missed": 0
    }))
    .unwrap()
}

#[test]
fn initial_backlog_starts_after_the_inbox_readers_ack() {
    let model = Model::new(status(), None);
    let waiting = model.due_inboxes();
    let positions = waiting[0]
        .addresses()
        .map(|address| address.position)
        .collect::<Vec<_>>();

    assert_eq!(positions, [5, 6, 7, 8, 9]);
}

#[test]
fn an_ack_from_the_inbox_reader_reduces_waiting() {
    let mut model = Model::new(status(), None);
    model.apply(event("acked", "lead", "lead", "first", 8));

    assert_eq!(model.lanes()[0].waiting, 1);
}

#[test]
fn another_reader_does_not_clear_an_inboxs_waiting_messages() {
    let mut model = Model::new(status(), None);
    model.apply(event("acked", "a0123456", "lead", "first", 8));

    assert_eq!(model.lanes()[0].waiting, 5);
}

#[test]
fn an_ack_for_an_earlier_creation_does_not_move_the_cursor() {
    let mut model = Model::new(status(), None);
    model.apply(event("acked", "lead", "lead", "retired", 9));

    assert_eq!(model.lanes()[0].waiting, 5);
}

#[test]
fn a_stale_snapshot_does_not_move_an_observed_ack_backward() {
    let mut model = Model::new(status(), None);
    model.apply(event("acked", "lead", "lead", "first", 8));

    model.update(status());

    assert_eq!(model.lanes()[0].waiting, 1);
}

#[test]
fn revocation_clears_the_waiting_inbox() {
    let mut snapshot = status();
    snapshot.channels[1].head = 2;
    let mut model = Model::new(snapshot.clone(), Some("worker".into()));
    assert_eq!(model.due_inboxes()[0].addresses().count(), 1);

    snapshot.participants[1].revoked = true;
    model.update(snapshot);

    assert!(model.due_inboxes().is_empty());
}

#[test]
fn a_revoked_recipient_changes_the_activity_delivery_status() {
    let mut snapshot = status();
    snapshot.participants[1].revoked = true;
    let mut model = Model::new(snapshot, None);
    model.enable_activity();
    let task = event("stored", "lead", "to/a0123456", "second", 1);

    assert_eq!(
        model.delivery(&task.address().unwrap()),
        activity::Delivery::Revoked
    );
}

#[test]
fn an_owner_revocation_does_not_move_a_participant_up_the_activity_order() {
    let mut snapshot = status();
    snapshot.participants.push(Participant {
        id: "b0123456".into(),
        name: "other".into(),
        ..snapshot.participants[1].clone()
    });
    let mut model = Model::new(snapshot.clone(), None);
    let mut older = event("stored", "a0123456", "lead", "first", 10);
    older.at_ms = Some(10);
    let mut newer = event("stored", "b0123456", "lead", "first", 11);
    newer.at_ms = Some(20);
    let mut revoke = event("owner", "a0123456", "lead", "first", 12);
    revoke.at_ms = Some(30);
    revoke.operation = Some("REVOKE".into());
    model.apply(older);
    model.apply(newer);
    model.apply(revoke);
    snapshot.participants[1].revoked = true;
    model.update(snapshot);

    let lanes = model.lanes_by_activity();
    assert_eq!(lanes[2].participant.name, "worker");
    assert_eq!(lanes[2].active_ms, Some(10));
}

#[test]
fn a_revoked_participants_row_has_no_waiting_count() {
    let mut snapshot = status();
    snapshot.channels[1].head = 4;
    snapshot.participants[1].revoked = true;
    let model = Model::new(snapshot, Some("worker".into()));

    assert_eq!(model.lanes()[0].waiting, 0);
}

#[test]
fn revocation_preserves_the_inbox_messages_in_activity() {
    let mut snapshot = status();
    let mut model = Model::new(snapshot.clone(), Some("worker".into()));
    let stored = event("stored", "lead", "to/a0123456", "second", 1);
    let address = stored.address().unwrap();
    model.apply(stored);

    snapshot.participants[1].revoked = true;
    model.update(snapshot);

    assert_eq!(model.feed().next().and_then(Event::address), Some(address));
}

#[test]
fn a_waiting_inbox_counts_its_entire_backlog() {
    let mut status = status();
    status.channels[0].head = 12;
    let model = Model::new(status, None);

    assert_eq!(model.due_inboxes()[0].addresses().count(), 7);
}

#[test]
fn participant_filter_includes_its_sends_and_its_inbox() {
    let mut model = Model::new(status(), Some("worker".into()));
    model.apply(event("stored", "a0123456", "lead", "first", 10));
    model.apply(event("stored", "lead", "lead", "first", 11));
    model.apply(event("stored", "lead", "to/a0123456", "second", 1));

    let seen = model
        .feed()
        .map(|event| event.position.unwrap())
        .collect::<Vec<_>>();

    assert_eq!(seen, [1, 10]);
}

#[test]
fn a_live_store_takes_precedence_over_historical_message_time() {
    let mut model = Model::new(status(), None);
    model.set_history(BTreeMap::from([("lead".into(), 300)]));
    let mut observed = event("stored", "lead", "lead", "first", 10);
    observed.at_ms = Some(200);

    model.apply(observed);

    assert_eq!(model.last_message("lead"), Some(200));
}

#[test]
fn a_reused_name_selects_the_active_participant() {
    let mut status = status();
    let mut former = status.participants[1].clone();
    former.id = "a0000000".into();
    former.revoked = true;
    status.participants.insert(0, former);

    let model = Model::new(status, Some("worker".into()));

    assert_eq!(model.selected_id(), Some("a0123456"));
    assert_eq!(model.lanes().len(), 1);
}

#[test]
fn participants_are_sorted_by_latest_activity_after_the_lead() {
    let mut status = status();
    status.participants.push(Participant {
        id: "b0123456".into(),
        name: "zulu".into(),
        ..status.participants[1].clone()
    });
    let mut model = Model::new(status, None);
    let mut older = event("stored", "a0123456", "lead", "first", 10);
    older.at_ms = Some(1_000);
    let mut newer = event("stored", "b0123456", "lead", "first", 11);
    newer.at_ms = Some(2_000);
    model.apply(older);
    model.apply(newer);

    let lanes = model.lanes_by_activity();

    let names = lanes.iter().map(|lane| lane.participant.name.as_str());
    assert_eq!(names.collect::<Vec<_>>(), ["lead", "zulu", "worker"]);
}

#[test]
fn participants_without_activity_stay_listed_after_those_with_activity() {
    let mut status = status();
    status.participants.push(Participant {
        id: "b0123456".into(),
        name: "alpha".into(),
        ..status.participants[1].clone()
    });
    let mut model = Model::new(status, None);
    let mut sent = event("stored", "a0123456", "lead", "first", 10);
    sent.at_ms = Some(1_000);
    model.apply(sent);

    let lanes = model.lanes_by_activity();

    let names = lanes.iter().map(|lane| lane.participant.name.as_str());
    assert_eq!(names.collect::<Vec<_>>(), ["lead", "worker", "alpha"]);
}
