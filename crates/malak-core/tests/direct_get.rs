//! Read-back verification through the in-process get verb.

mod common;

use common::{Probe, board, grant, limits, publisher};
use malak_core::{
    Action, CreationId, GetReply, Grade, HeaderValue, Kind, Outcome, Owner, SendRequest,
};

#[test]
fn get_preserves_opaque_bytes_and_board_assigned_fields() {
    let owner = board(limits());
    let sender = publisher(&owner, "worker");
    let fields = [("trace", HeaderValue::Text("1234abcd"))];
    let request = SendRequest {
        kind: Kind::Report,
        headers: &fields,
        ..SendRequest::note("lead", "work-1", b"\xff\x00")
    };
    let receipt = sender.send(request).unwrap();

    let GetReply::Message(message) = owner.lead().get("lead", receipt.position.get()).unwrap()
    else {
        panic!("expected a message");
    };
    assert_eq!(&message.sender, sender.id());
    assert_eq!(message.sender_name.as_str(), "worker");
    assert_eq!(message.grade, Grade::Proven);
    assert_eq!(message.receipt, receipt);
    assert_eq!(message.kind, Kind::Report);
    assert_eq!(message.headers[0].key(), "trace");
    assert_eq!(&message.body[..], request.body);
}

#[test]
fn reply_to_round_trips_with_its_creation_id() {
    let owner = board(limits());
    let lead = owner.lead();
    let creation = CreationId::from_bytes([5; 16]);
    let fields = [(
        "reply_to",
        HeaderValue::ReplyTo {
            channel: "jobs",
            creation,
            position: 19,
        },
    )];
    let receipt = lead
        .send(SendRequest {
            headers: &fields,
            ..SendRequest::note("lead", "id", b"")
        })
        .unwrap();

    let GetReply::Message(message) = lead.get("lead", receipt.position.get()).unwrap() else {
        panic!("expected a message");
    };
    assert!(
        matches!(&message.headers[0], malak_core::Header::ReplyTo { creation: id, position, .. }
        if *id == creation && position.get() == 19)
    );
}

#[test]
fn get_distinguishes_absence_from_a_never_written_position() {
    let owner = board(limits());
    let lead = owner.lead();
    assert_eq!(lead.get("absent", 1).unwrap(), GetReply::Absent);
    assert!(matches!(
        lead.get("lead", 1).unwrap(),
        GetReply::NeverWritten { .. }
    ));
}

#[test]
fn get_permission_is_separate_from_read_permission() {
    let store = Probe::default();
    let owner = Owner::in_memory(store.clone(), limits()).unwrap();
    let worker = owner
        .enrol("worker", grant("lead", &[Action::Read]))
        .unwrap();
    owner
        .lead()
        .send(SendRequest::note("lead", "id", b"known message"))
        .unwrap();
    let before = store.calls();

    assert_eq!(
        worker.get("lead", 1).unwrap_err().outcome(),
        Outcome::Refused
    );
    assert_eq!(store.calls(), before);
}

#[test]
fn metadata_larger_than_inline_capacity_round_trips() {
    let owner = board(limits());
    let lead = owner.lead();
    let value = "x".repeat(512);
    let fields = [("x-long", HeaderValue::Text(&value))];
    lead.send(SendRequest {
        headers: &fields,
        ..SendRequest::note("lead", "one", b"body")
    })
    .unwrap();

    let GetReply::Message(message) = lead.get("lead", 1).unwrap() else {
        panic!("expected a message");
    };

    assert!(
        matches!(&message.headers[0], malak_core::Header::Extension { value: text, .. }
        if text.as_ref() == value)
    );
}
