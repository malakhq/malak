//! Participant inbox addresses are resolved by the board.

mod common;

use common::{board, grant, limits};
use malak_core::{
    Action, CreationId, GetReply, Grant, Handle, Header, HeaderValue, Outcome, Owner, SendRequest,
};
use std::time::Duration;

fn worker(owner: &Owner) -> Handle {
    let worker = owner.enrol("reviewer", Grant::default()).unwrap();
    owner
        .replace_grant(
            worker.id(),
            grant(
                worker.id().inbox().as_str(),
                &[Action::Read, Action::Get, Action::Ack],
            ),
        )
        .unwrap();
    worker
}

fn deliver(owner: &Owner) -> malak_core::Receipt {
    deliver_result(owner).unwrap()
}

#[test]
fn sending_by_name_stores_the_message_in_the_participants_id_inbox() {
    let owner = board(limits());
    let worker = owner.enrol("reviewer", Grant::default()).unwrap();
    owner
        .lead()
        .send(SendRequest::note("to/reviewer", "one", b"review"))
        .unwrap();

    let GetReply::Message(message) = owner.lead().get(worker.id().inbox().as_str(), 1).unwrap()
    else {
        panic!("expected the named delivery in the ID inbox");
    };
    assert_eq!(message.channel, worker.id().inbox());
}

#[test]
fn an_unknown_inbox_is_refused_with_a_nearby_name() {
    let owner = board(limits());
    owner.enrol("reviewer", Grant::default()).unwrap();
    let error = owner
        .lead()
        .send(SendRequest::note("to/revewer", "one", b"review"))
        .unwrap_err();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(error.to_string().contains("reviewer"), "{error}");
}

#[test]
fn an_unknown_inbox_does_not_disclose_other_participants_to_an_ungranted_worker() {
    let owner = board(limits());
    owner.enrol("hidden-reviewer", Grant::default()).unwrap();
    let sender = owner.enrol("sender", Grant::default()).unwrap();

    let error = sender
        .send(SendRequest::note("to/hidden-revewer", "one", b"review"))
        .unwrap_err();
    assert_eq!(error.to_string(), "unknown inbox to/hidden-revewer");
}

#[test]
fn inbox_suggestions_include_only_participants_covered_by_the_callers_grants() {
    let owner = board(limits());
    let allowed = owner.enrol("visible-reviewer", Grant::default()).unwrap();
    let hidden = owner.enrol("hidden-reviewer", Grant::default()).unwrap();
    let sender = owner
        .enrol(
            "sender",
            grant(allowed.id().inbox().as_str(), &[Action::Send]),
        )
        .unwrap();

    let error = sender.get("to/hidden-revewer", 1).unwrap_err().to_string();
    assert!(error.contains("visible-reviewer"), "{error}");
    assert!(!error.contains("hidden-reviewer"), "{error}");
    assert!(!error.contains(hidden.id().as_str()), "{error}");
}

#[test]
fn explicit_channel_creation_cannot_create_an_unknown_inbox() {
    let owner = board(limits());
    assert_eq!(
        owner.create_channel("to/missing").unwrap_err().outcome(),
        Outcome::Refused
    );
}

#[test]
fn reading_by_name_uses_the_id_grant_and_returns_the_id_channel() {
    let owner = board(limits());
    let worker = worker(&owner);
    deliver(&owner);

    let reply = worker.read(&["to/reviewer"], 1, Duration::ZERO).unwrap();
    assert_eq!(reply.messages.len(), 1);
    assert_eq!(reply.channels[0].channel, worker.id().inbox());
    assert_eq!(reply.messages[0].channel, worker.id().inbox());
}

#[test]
fn getting_by_name_uses_the_id_grant() {
    let owner = board(limits());
    let worker = worker(&owner);
    deliver(&owner);

    assert!(matches!(
        worker.get("to/reviewer", 1).unwrap(),
        GetReply::Message(_)
    ));
}

#[test]
fn acknowledging_by_name_advances_the_same_id_inbox() {
    let owner = board(limits());
    let worker = worker(&owner);
    let receipt = deliver(&owner);
    worker
        .read(&[worker.id().inbox().as_str()], 1, Duration::ZERO)
        .unwrap();

    worker.ack("to/reviewer", receipt.creation, 1).unwrap();
    assert!(
        worker
            .read(&[worker.id().inbox().as_str()], 1, Duration::ZERO)
            .unwrap()
            .messages
            .is_empty()
    );
}

#[test]
fn sending_by_name_uses_the_resolved_id_grant() {
    let owner = board(limits());
    let recipient = worker(&owner);
    let sender = owner
        .enrol(
            "sender",
            grant(recipient.id().inbox().as_str(), &[Action::Send]),
        )
        .unwrap();

    sender
        .send(SendRequest::note("to/reviewer", "one", b"review"))
        .unwrap();
    assert!(matches!(
        recipient.get("to/reviewer", 1).unwrap(),
        GetReply::Message(_)
    ));
}

#[test]
fn a_name_grant_does_not_grant_access_to_the_id_inbox() {
    let owner = board(limits());
    worker(&owner);
    let sender = owner
        .enrol(
            "sender",
            grant("to/reviewer", &[Action::Send, Action::Create]),
        )
        .unwrap();

    assert_eq!(
        sender
            .send(SendRequest::note("to/reviewer", "one", b"review"))
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn a_name_cannot_bypass_the_read_grant() {
    let owner = board(limits());
    let worker = owner
        .enrol("reviewer", grant("to/reviewer", &[Action::Read]))
        .unwrap();
    deliver(&owner);

    assert_eq!(
        worker
            .read(&["to/reviewer"], 1, Duration::ZERO)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn a_name_cannot_bypass_the_get_grant() {
    let owner = board(limits());
    let worker = owner
        .enrol("reviewer", grant("to/reviewer", &[Action::Get]))
        .unwrap();

    assert_eq!(
        worker.get("to/reviewer", 1).unwrap_err().outcome(),
        Outcome::Refused
    );
}

#[test]
fn a_name_cannot_bypass_the_ack_grant() {
    let owner = board(limits());
    let worker = owner
        .enrol("reviewer", grant("to/reviewer", &[Action::Ack]))
        .unwrap();

    assert_eq!(
        worker
            .ack("to/reviewer", CreationId::from_bytes([1; 16]), 1)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn retrying_by_id_after_sending_by_name_returns_the_original_receipt() {
    let owner = board(limits());
    let worker = worker(&owner);
    let receipt = deliver(&owner);

    let retry = owner
        .lead()
        .send(SendRequest::note(
            worker.id().inbox().as_str(),
            "one",
            b"review",
        ))
        .unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.creation, receipt.creation);
    assert_eq!(retry.position, receipt.position);
}

#[test]
fn a_reply_header_by_name_is_stored_with_the_resolved_id() {
    let owner = board(limits());
    let worker = worker(&owner);
    let receipt = deliver(&owner);
    let headers = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: "to/reviewer",
            creation: receipt.creation,
            position: 1,
        },
    )];
    owner
        .lead()
        .send(SendRequest {
            headers: &headers,
            ..SendRequest::note("lead", "reply", b"done")
        })
        .unwrap();

    let GetReply::Message(message) = owner.lead().get("lead", 1).unwrap() else {
        panic!("missing reply");
    };
    assert!(
        matches!(&message.headers[0], Header::ReplyTo { channel, .. } if channel == &worker.id().inbox())
    );
}

#[test]
fn an_unknown_reply_inbox_is_refused_before_delivery() {
    let owner = board(limits());
    let headers = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: "to/missing",
            creation: CreationId::from_bytes([1; 16]),
            position: 1,
        },
    )];

    assert_eq!(
        owner
            .lead()
            .send(SendRequest {
                headers: &headers,
                ..SendRequest::note("lead", "reply", b"done")
            })
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
    assert!(matches!(
        owner.lead().get("lead", 1).unwrap(),
        GetReply::NeverWritten { .. }
    ));
}

#[test]
fn an_unknown_reply_inbox_does_not_create_a_new_destination_channel() {
    let owner = board(limits());
    let headers = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: "to/missing",
            creation: CreationId::from_bytes([1; 16]),
            position: 1,
        },
    )];

    assert!(
        owner
            .lead()
            .send(SendRequest {
                headers: &headers,
                ..SendRequest::note("new-jobs", "reply", b"done")
            })
            .is_err()
    );
    assert_eq!(owner.lead().get("new-jobs", 1).unwrap(), GetReply::Absent);
}

#[test]
fn retrying_a_canonical_reply_with_its_name_header_returns_the_original_receipt() {
    let owner = board(limits());
    let worker = worker(&owner);
    let task = deliver(&owner);
    let inbox = worker.id().inbox();
    let headers = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: inbox.as_str(),
            creation: task.creation,
            position: 1,
        },
    )];
    let receipt = owner
        .lead()
        .send(SendRequest {
            headers: &headers,
            ..SendRequest::note("lead", "reply", b"done")
        })
        .unwrap();
    let alias = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: "to/reviewer",
            creation: task.creation,
            position: 1,
        },
    )];

    let retry = owner
        .lead()
        .send(SendRequest {
            headers: &alias,
            ..SendRequest::note("lead", "reply", b"done")
        })
        .unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.creation, receipt.creation);
    assert_eq!(retry.position, receipt.position);
}

#[test]
fn a_revoked_name_is_refused() {
    let owner = board(limits());
    let worker = worker(&owner);
    owner.revoke(worker.id()).unwrap();

    assert_eq!(
        deliver_result(&owner).unwrap_err().outcome(),
        Outcome::Refused
    );
}

fn deliver_result(owner: &Owner) -> Result<malak_core::Receipt, malak_core::Error> {
    owner
        .lead()
        .send(SendRequest::note("to/reviewer", "one", b"review"))
}

#[test]
fn a_reused_name_delivers_to_the_new_enrolment() {
    let owner = board(limits());
    let old = worker(&owner);
    deliver(&owner);
    owner.revoke(old.id()).unwrap();
    let current = worker(&owner);
    deliver(&owner);

    assert_ne!(old.id(), current.id());
    assert!(matches!(
        current.get("to/reviewer", 1).unwrap(),
        GetReply::Message(_)
    ));
    assert!(matches!(
        owner.lead().get(old.id().inbox().as_str(), 1).unwrap(),
        GetReply::Message(_)
    ));
}

#[test]
fn owner_channel_creation_by_name_returns_the_real_inbox() {
    let owner = board(limits());
    let worker = worker(&owner);
    let (expected, _) = owner.create_channel(worker.id().inbox().as_str()).unwrap();

    assert_eq!(
        owner.create_channel("to/reviewer").unwrap(),
        (expected, false)
    );
}

#[test]
fn reading_an_unknown_inbox_is_refused() {
    let owner = board(limits());
    assert_eq!(
        owner
            .lead()
            .read(&["to/missing"], 0, Duration::ZERO)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn getting_an_unknown_inbox_is_refused() {
    let owner = board(limits());
    assert_eq!(
        owner.lead().get("to/missing", 1).unwrap_err().outcome(),
        Outcome::Refused
    );
}

#[test]
fn acknowledging_an_unknown_inbox_is_refused() {
    let owner = board(limits());
    let error = owner
        .lead()
        .ack("to/missing", CreationId::from_bytes([1; 16]), 1)
        .unwrap_err();
    assert!(error.to_string().contains("unknown inbox"));
}

#[test]
fn to_lead_addresses_the_existing_lead_inbox() {
    let owner = board(limits());
    owner
        .lead()
        .send(SendRequest::note("to/lead", "one", b"review"))
        .unwrap();
    assert!(matches!(
        owner.lead().get("lead", 1).unwrap(),
        GetReply::Message(_)
    ));
}

#[test]
fn new_participant_names_cannot_have_the_length_of_a_generated_id() {
    let owner = board(limits());
    let error = owner
        .enrol("abcdef0123456789abcdef0123456789", Grant::default())
        .err()
        .unwrap();
    assert!(error.to_string().contains("1 to 24"));
}

fn refused_name(name: &str) {
    let owner = board(limits());
    let error = owner.enrol(name, Grant::default()).err().unwrap();
    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(error.to_string().contains("1 to 24"), "{error}");
}

#[test]
fn a_new_name_of_twenty_four_characters_is_accepted() {
    let owner = board(limits());
    assert!(owner.enrol(&"a".repeat(24), Grant::default()).is_ok());
}

#[test]
fn a_new_name_of_twenty_five_characters_is_refused() {
    refused_name(&"a".repeat(25));
}

#[test]
fn a_new_name_starting_with_a_digit_is_refused() {
    refused_name("1-reviewer");
}

#[test]
fn a_new_name_with_an_underscore_is_refused() {
    refused_name("old_reviewer");
}

#[test]
fn a_new_name_with_a_double_hyphen_is_refused() {
    refused_name("review--worker");
}

#[test]
fn a_new_name_with_a_trailing_hyphen_is_refused() {
    refused_name("reviewer-");
}

#[test]
fn a_new_name_outside_the_legacy_alphabet_gets_the_current_rule() {
    refused_name("Reviewer");
}

#[test]
fn resolving_a_reply_cannot_exceed_the_header_byte_limit() {
    let mut limits = limits();
    limits.header_bytes = "reply_to".len() + "to/reviewer".len() + 16 + 8;
    let owner = board(limits);
    worker(&owner);
    let headers = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: "to/reviewer",
            creation: CreationId::from_bytes([1; 16]),
            position: 1,
        },
    )];

    let error = owner
        .lead()
        .send(SendRequest {
            headers: &headers,
            ..SendRequest::note("lead", "reply", b"done")
        })
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("resolved reply address exceeds header limits")
    );
}

#[test]
fn a_task_read_by_name_still_supplies_the_workers_implicit_trace() {
    let owner = board(limits());
    let worker = worker(&owner);
    owner
        .replace_grant(worker.id(), grant("**", &[Action::Read, Action::Send]))
        .unwrap();
    let receipt = owner
        .lead()
        .send(SendRequest {
            kind: malak_core::Kind::Task,
            ..SendRequest::note("to/reviewer", "task", b"review")
        })
        .unwrap();
    worker.read(&["to/reviewer"], 1, Duration::ZERO).unwrap();

    assert_eq!(
        worker
            .send(SendRequest::note("lead", "report", b"done"))
            .unwrap()
            .trace,
        receipt.trace
    );
}

#[test]
fn removing_a_reused_name_with_its_old_creation_is_refused() {
    let owner = board(limits());
    let old = worker(&owner);
    let receipt = deliver(&owner);
    owner.revoke(old.id()).unwrap();
    let current = worker(&owner);
    let current_receipt = deliver(&owner);

    let error = owner
        .remove_channel("to/reviewer", receipt.creation)
        .unwrap_err();
    assert!(error.to_string().contains(current.id().as_str()));
    assert!(error.to_string().contains(&receipt.creation.to_string()));
    assert!(
        error
            .to_string()
            .contains(&current_receipt.creation.to_string())
    );
    assert!(matches!(
        current.get("to/reviewer", 1).unwrap(),
        GetReply::Message(_)
    ));
}

#[test]
fn trimming_a_reused_name_with_its_old_creation_is_refused() {
    let owner = board(limits());
    let old = worker(&owner);
    let receipt = deliver(&owner);
    owner.revoke(old.id()).unwrap();
    let current = worker(&owner);
    let current_receipt = deliver(&owner);

    let error = owner.trim("to/reviewer", receipt.creation, 2).unwrap_err();
    assert!(error.to_string().contains(current.id().as_str()));
    assert!(error.to_string().contains(&receipt.creation.to_string()));
    assert!(
        error
            .to_string()
            .contains(&current_receipt.creation.to_string())
    );
    assert!(matches!(
        current.get("to/reviewer", 1).unwrap(),
        GetReply::Message(_)
    ));
}
