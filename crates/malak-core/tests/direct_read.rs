//! Read and acknowledgement behavior through Direct handles.

mod common;

use common::{Probe, board, grant, limits};
use malak_core::{Action, Error, Handle, Outcome, Owner, ReadReply, Receipt, SendRequest};
use std::time::Duration;

fn send(handle: &Handle, channel: &str, id: &str, body: &[u8]) -> Receipt {
    handle.send(SendRequest::note(channel, id, body)).unwrap()
}

fn read(handle: &Handle, channels: &[&str], max: usize) -> ReadReply {
    handle.read(channels, max, Duration::ZERO).unwrap()
}

fn ack(handle: &Handle, channel: &str, receipt: Receipt) {
    handle
        .ack(channel, receipt.creation, receipt.position.get())
        .unwrap();
}

fn reader(owner: &Owner, name: &str) -> Handle {
    owner
        .enrol(name, grant("**", &[Action::Read, Action::Get, Action::Ack]))
        .unwrap()
}

fn refused_verbs(caller: &Handle, channel: &str, receipt: Receipt) -> [Error; 4] {
    [
        caller
            .send(SendRequest::note(channel, "refused", b"refused"))
            .unwrap_err(),
        caller.read(&[channel], 1, Duration::ZERO).unwrap_err(),
        caller.get(channel, receipt.position.get()).unwrap_err(),
        caller
            .ack(channel, receipt.creation, receipt.position.get())
            .unwrap_err(),
    ]
}

#[test]
fn every_denied_or_revoked_verb_leaves_the_store_untouched() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let record = send(&owner.lead(), "jobs", "stored", b"stored");
    let denied = owner
        .enrol("denied", grant("other", &[Action::Send]))
        .unwrap();
    let revoked = owner
        .enrol(
            "revoked",
            grant(
                "jobs",
                &[Action::Send, Action::Read, Action::Get, Action::Ack],
            ),
        )
        .unwrap();
    owner.revoke(revoked.id()).unwrap();
    let before = store.calls();

    for caller in [&denied, &revoked] {
        assert!(
            refused_verbs(caller, "jobs", record)
                .iter()
                .all(|error| error.outcome() == Outcome::Refused)
        );
    }

    assert_eq!(store.calls(), before);
}

#[test]
fn refused_verbs_do_not_disclose_whether_a_channel_exists() {
    let owner = board(limits());
    let record = send(&owner.lead(), "present", "stored", b"stored");
    let caller = owner
        .enrol("worker", grant("other", &[Action::Send]))
        .unwrap();

    let present = refused_verbs(&caller, "present", record);
    let absent = refused_verbs(&caller, "absent", record);

    assert_eq!(present, absent);
    assert!(
        present
            .iter()
            .all(|error| error.outcome() == Outcome::Refused)
    );
}

#[test]
fn read_repeats_unacknowledged_messages() {
    let owner = board(limits());
    let lead = owner.lead();
    let receipt = send(&lead, "lead", "one", b"work");
    for _ in 0..2 {
        let result = read(&lead, &["lead"], 1);
        assert_eq!(result.messages[0].receipt, receipt);
    }
}

#[test]
fn acknowledgement_advances_only_the_calling_reader() {
    let owner = board(limits());
    let lead = owner.lead();
    let first = reader(&owner, "first");
    let second = reader(&owner, "second");
    let receipt = send(&lead, "lead", "one", b"work");

    read(&first, &["lead"], 1);
    ack(&first, "lead", receipt);

    assert!(read(&first, &["lead"], 1).messages.is_empty());
    assert_eq!(read(&second, &["lead"], 1).messages[0].receipt, receipt);
}

#[test]
fn get_does_not_authorize_an_acknowledgement() {
    let owner = board(limits());
    let lead = owner.lead();
    let receipt = send(&lead, "lead", "one", b"work");
    lead.get("lead", receipt.position.get()).unwrap();
    assert_eq!(
        lead.ack("lead", receipt.creation, receipt.position.get())
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn acknowledgement_cannot_pass_the_highest_delivery() {
    let owner = board(limits());
    let lead = owner.lead();
    send(&lead, "lead", "one", b"one");
    let second = send(&lead, "lead", "two", b"two");
    read(&lead, &["lead"], 1);

    assert_eq!(
        lead.ack("lead", second.creation, second.position.get())
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn duplicate_acknowledgement_does_not_advance_further() {
    let owner = board(limits());
    let lead = owner.lead();
    let first = send(&lead, "lead", "one", b"one");
    let second = send(&lead, "lead", "two", b"two");
    read(&lead, &["lead"], 2);
    ack(&lead, "lead", first);
    ack(&lead, "lead", first);

    assert_eq!(read(&lead, &["lead"], 2).messages[0].receipt, second);
}

#[test]
fn acknowledgement_for_an_earlier_creation_is_refused() {
    let initial = board(limits());
    let previous = send(&initial.lead(), "lead", "one", b"one");
    let owner = board(limits());
    let lead = owner.lead();
    send(&lead, "lead", "one", b"one");
    read(&lead, &["lead"], 1);

    assert_eq!(
        lead.ack("lead", previous.creation, previous.position.get())
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn one_disallowed_channel_prevents_a_read_on_all_channels() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let participant = owner
        .enrol("limited", grant("lead", &[Action::Read]))
        .unwrap();
    send(&owner.lead(), "lead", "one", b"one");
    let before = store.calls();

    let result = participant.read(&["lead", "other"], 1, Duration::ZERO);

    assert_eq!(result.unwrap_err().outcome(), Outcome::Refused);
    assert_eq!(store.calls(), before);
}

#[test]
fn revoked_participants_cannot_read_get_or_ack() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let participant = reader(&owner, "limited");
    let receipt = send(&owner.lead(), "lead", "one", b"one");
    read(&participant, &["lead"], 1);
    owner.revoke(participant.id()).unwrap();
    let before = store.calls();

    for error in [
        participant.read(&["lead"], 1, Duration::ZERO).unwrap_err(),
        participant.get("lead", 1).unwrap_err(),
        participant.ack("lead", receipt.creation, 1).unwrap_err(),
    ] {
        assert_eq!(error.outcome(), Outcome::Refused);
    }
    assert_eq!(store.calls(), before);
}

#[test]
fn revocation_frees_a_place_for_a_new_reader() {
    let mut settings = limits();
    settings.channel_readers = 1;
    let owner = board(settings);
    let first = reader(&owner, "first");
    let second = reader(&owner, "second");
    read(&first, &["lead"], 1);
    assert_eq!(
        second
            .read(&["lead"], 1, Duration::ZERO)
            .unwrap_err()
            .outcome(),
        Outcome::Unavailable
    );

    owner.revoke(first.id()).unwrap();

    assert!(second.read(&["lead"], 1, Duration::ZERO).is_ok());
}

#[test]
fn one_message_reads_carry_the_turn_to_the_next_channel() {
    let owner = board(limits());
    let lead = owner.lead();
    send(&lead, "a", "one", b"one");
    send(&lead, "a", "two", b"two");
    let quiet = send(&lead, "b", "one", b"quiet");

    let first = read(&lead, &["a", "b"], 1);
    ack(&lead, "a", first.messages[0].receipt);

    assert_eq!(read(&lead, &["a", "b"], 1).messages[0].receipt, quiet);
}

#[test]
fn a_message_that_does_not_fit_keeps_its_channel_turn() {
    let mut settings = limits();
    settings.body_bytes = 20 * 1024;
    settings.read_reply_bytes = settings.body_bytes + malak_proto::MAX_HEADER + 6;
    let owner = board(settings);
    let lead = owner.lead();
    let body = vec![1; 20 * 1024];
    let first = send(&lead, "a", "one", &body);
    let second = send(&lead, "b", "one", &body);

    let reply = read(&lead, &["a", "b"], 2);
    assert_eq!(reply.messages.len(), 1);
    assert_eq!(reply.messages[0].receipt, first);
    ack(&lead, "a", first);

    assert_eq!(read(&lead, &["a", "b"], 2).messages[0].receipt, second);
}

#[test]
fn absent_channels_are_reported_separately() {
    let owner = board(limits());
    let lead = owner.lead();
    let receipt = send(&lead, "lead", "one", b"work");

    let reply = read(&lead, &["lead", "other"], 1);

    assert_eq!(reply.channels[0].creation, Some(receipt.creation));
    assert_eq!(reply.channels[1].creation, None);
}
