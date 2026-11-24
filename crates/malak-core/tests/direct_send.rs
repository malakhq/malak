//! Identity and call rules on the in-process send path.

mod common;

use common::{Probe, board, grant, limits, publisher};
use malak_core::{Action, Durability, Grant, Outcome, Owner, Position, SendRequest};
use std::sync::{Barrier, OnceLock};
use std::thread;
use std::time::UNIX_EPOCH;

const NOTE: SendRequest<'_> = SendRequest {
    channel: "lead",
    id: "mark",
    kind: malak_core::Kind::Note,
    headers: &[],
    body: b"sent",
    minimum_durability: None,
};

#[test]
fn revoked_participant_cannot_send() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let worker = publisher(&owner, "worker");
    owner.revoke(worker.id()).unwrap();

    assert_eq!(worker.send(NOTE).unwrap_err().outcome(), Outcome::Refused);
    assert_eq!(store.calls(), 0);
}

#[test]
fn an_expired_grant_makes_a_handle_invalid() {
    let owner = board(limits());
    let mut permissions = grant("lead", &[Action::Send]);
    permissions.set_end(Some(UNIX_EPOCH));
    let worker = owner.enrol("worker", permissions).unwrap();

    assert_eq!(worker.send(NOTE).unwrap_err().outcome(), Outcome::Refused);
}

#[test]
fn replacing_the_grant_changes_existing_handles() {
    let owner = board(limits());
    let worker = publisher(&owner, "worker");
    owner.replace_grant(worker.id(), Grant::default()).unwrap();

    assert_eq!(worker.send(NOTE).unwrap_err().outcome(), Outcome::Refused);
}

#[test]
fn denied_send_reveals_nothing_about_channel_existence() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let worker = owner.enrol("worker", Grant::default()).unwrap();
    let absent = SendRequest {
        channel: "absent",
        ..NOTE
    };

    assert_eq!(
        worker.send(NOTE).unwrap_err(),
        worker.send(absent).unwrap_err()
    );
    assert_eq!(store.calls(), 0);
}

#[test]
fn malformed_calls_use_the_participant_rate_not_the_leads() {
    let mut settings = limits();
    settings.calls_per_minute = 1;
    let owner = board(settings);
    let first = publisher(&owner, "worker");
    let second = owner.handle(first.id()).unwrap();

    let malformed = SendRequest {
        channel: "/",
        ..NOTE
    };
    assert_eq!(
        first.send(malformed).unwrap_err().outcome(),
        Outcome::Refused
    );
    assert_eq!(
        second.send(NOTE).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    assert!(owner.lead().send(NOTE).is_ok());
}

#[test]
fn sending_requires_create_only_on_absence() {
    let owner = board(limits());
    let worker = owner.enrol("worker", grant("**", &[Action::Send])).unwrap();

    assert!(worker.send(NOTE).is_ok());
    assert_eq!(
        worker
            .send(SendRequest {
                channel: "absent",
                ..NOTE
            })
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn unsupported_durability_reaches_no_store() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let call = SendRequest {
        minimum_durability: Some(Durability::Synced),
        ..NOTE
    };
    assert_eq!(
        owner.lead().send(call).unwrap_err().outcome(),
        Outcome::NotSupported
    );
    assert_eq!(store.calls(), 0);
}

#[test]
fn send_retries_racing_on_several_handles_store_once() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let worker = publisher(&owner, "worker");
    let start = Barrier::new(8);
    let creation = OnceLock::new();
    thread::scope(|scope| {
        for _ in 0..8 {
            let handle = owner.handle(worker.id()).unwrap();
            let start = &start;
            let creation = &creation;
            scope.spawn(move || {
                start.wait();
                for _ in 0..125 {
                    match handle.send(NOTE) {
                        Ok(receipt) => {
                            assert_eq!(receipt.position, Position::FIRST);
                            assert_eq!(
                                creation.get_or_init(|| receipt.creation),
                                &receipt.creation
                            );
                            assert_eq!(receipt.durability, Durability::Memory);
                        }
                        Err(error) => assert_eq!(error.outcome(), Outcome::Unavailable),
                    }
                }
            });
        }
    });
    assert_eq!(store.calls(), 1);
}

#[test]
fn a_retry_with_changed_body_is_refused() {
    let owner = board(limits());
    let lead = owner.lead();
    lead.send(NOTE).unwrap();
    let changed = SendRequest {
        body: b"changed",
        ..NOTE
    };
    assert_eq!(lead.send(changed).unwrap_err().outcome(), Outcome::Refused);
}

#[test]
fn two_senders_can_use_the_same_message_id() {
    let owner = board(limits());
    let first = publisher(&owner, "first").send(NOTE).unwrap();
    let second = publisher(&owner, "second").send(NOTE).unwrap();

    assert_eq!(first.creation, second.creation);
    assert_eq!(second.position.get(), first.position.get() + 1);
}

#[test]
fn retry_lookup_precedes_the_daily_allowance() {
    let mut settings = limits();
    settings.daily_storage_bytes = 300;
    let owner = board(settings);
    let worker = publisher(&owner, "worker");
    let original = worker.send(NOTE).unwrap();

    assert_eq!(
        worker
            .send(SendRequest { id: "new", ..NOTE })
            .unwrap_err()
            .outcome(),
        Outcome::Unavailable,
    );
    let retry = worker.send(NOTE).unwrap();
    assert_eq!(
        (retry.position, retry.creation, retry.durability),
        (original.position, original.creation, original.durability)
    );
    assert!(retry.duplicate);
}

#[test]
fn participants_cannot_take_the_leads_reserved_bytes() {
    let mut settings = limits();
    settings.board_storage_bytes = 800;
    settings.lead_storage_percent = 40;
    let owner = board(settings);
    let worker = publisher(&owner, "worker");
    worker.send(NOTE).unwrap();
    worker
        .send(SendRequest {
            id: "second",
            ..NOTE
        })
        .unwrap();
    assert_eq!(
        worker
            .send(SendRequest {
                id: "third",
                ..NOTE
            })
            .unwrap_err()
            .outcome(),
        Outcome::Unavailable
    );
    assert!(owner.lead().send(NOTE).is_ok());
}

#[test]
fn participants_cannot_take_the_leads_channel_places() {
    let mut settings = limits();
    settings.board_channels = 4;
    settings.reserved_channels = 1;
    let owner = board(settings);
    let worker = publisher(&owner, "worker");

    worker
        .send(SendRequest {
            channel: "third",
            ..NOTE
        })
        .unwrap();
    let last = SendRequest {
        channel: "last",
        ..NOTE
    };
    assert_eq!(worker.send(last).unwrap_err().outcome(), Outcome::Refused);
    assert!(owner.lead().send(last).is_ok());
}

#[test]
fn racing_new_channel_names_cannot_spend_other_channel_places() {
    let mut settings = limits();
    settings.board_channels = 7;
    settings.reserved_channels = 1;
    settings.participant_channels = 2;
    let owner = board(settings);
    let worker = publisher(&owner, "worker");
    let other = publisher(&owner, "other");
    let first = Barrier::new(12);

    let admitted = thread::scope(|scope| {
        (0..12)
            .map(|number| {
                let worker = &worker;
                let first = &first;
                scope.spawn(move || {
                    let name = format!("work-{number}");
                    first.wait();
                    match worker.send(SendRequest::note(&name, "message", b"")) {
                        Ok(receipt) => {
                            assert_eq!(receipt.position, Position::FIRST);
                            true
                        }
                        Err(error) => {
                            assert_eq!(error.outcome(), Outcome::Refused);
                            assert_eq!(error.to_string(), "channel allowance exceeded");
                            false
                        }
                    }
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|call| call.join().unwrap().then_some(()))
            .count()
    });

    assert_eq!(admitted, 2);
    assert!(
        other
            .send(SendRequest::note("other-work", "message", b""))
            .is_ok()
    );
    let reserved = SendRequest::note("reserved", "message", b"");
    assert_eq!(
        other.send(reserved).unwrap_err().outcome(),
        Outcome::Refused
    );
    assert!(
        owner
            .status(None, Some(&"reserved".parse().unwrap()))
            .channels
            .is_empty()
    );
    assert!(owner.lead().send(reserved).is_ok());
}

#[test]
fn enrolment_names_are_unique_even_when_enrolments_race() {
    let owner = board(limits());
    thread::scope(|scope| {
        let first = scope.spawn(|| owner.enrol("worker", Grant::default()));
        let second = scope.spawn(|| owner.enrol("worker", Grant::default()));
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    });
}

#[test]
fn an_empty_trace_retry_candidate_cannot_create_a_channel() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();

    let result = owner.lead().send(SendRequest {
        channel: "absent",
        headers: &[("trace", malak_core::HeaderValue::Text(""))],
        ..NOTE
    });

    assert_eq!(result.unwrap_err().outcome(), Outcome::Refused);
    assert_eq!(store.calls(), 0);
}

#[test]
fn an_empty_trace_cannot_retry_a_trace_aware_entry() {
    let owner = board(limits());
    let lead = owner.lead();
    lead.send(NOTE).unwrap();

    let retry = lead.send(SendRequest {
        headers: &[("trace", malak_core::HeaderValue::Text(""))],
        ..NOTE
    });

    assert_eq!(retry.unwrap_err().outcome(), Outcome::Refused);
}
