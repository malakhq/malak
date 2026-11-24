//! Owner operation isolation, bounds, and request lifetimes.

use bytes::Bytes;
use malak_proto::{
    ConnectionError, FramingError, OwnerConnection, OwnerLimits, OwnerOperation, OwnerRequest,
    frame_prefix,
};
use minicbor::Encoder;
use std::time::Duration;

fn owner(hash: u8) -> OwnerConnection {
    OwnerConnection::new(OwnerLimits::default(), &[hash; 32], Duration::ZERO).unwrap()
}

fn frame(rid: u64, operation: &str, total: Option<usize>) -> Bytes {
    let mut header = Encoder::new(Vec::new());
    header
        .map(2)
        .unwrap()
        .str("op")
        .unwrap()
        .str(operation)
        .unwrap()
        .str("rid")
        .unwrap()
        .u64(rid)
        .unwrap();
    let header = header.into_writer();
    let prefix_bytes = 6;
    let body = total.map_or(0, |total| total - prefix_bytes - header.len());
    let prefix = frame_prefix(header.len(), body).unwrap();
    let mut frame = [prefix.as_slice(), &header].concat();
    frame.resize(frame.len() + body, 0);
    frame.into()
}

fn request(owner: &mut OwnerConnection, rid: u64, operation: &str) -> OwnerRequest {
    owner
        .receive(&mut frame(rid, operation, None), Duration::ZERO)
        .unwrap()
        .unwrap()
}

#[test]
fn an_owner_can_request_status_without_attaching() {
    let mut owner = owner(1);

    assert_eq!(
        request(&mut owner, 7, "STATUS").operation,
        OwnerOperation::Status
    );
}

#[test]
fn owner_protocol_has_no_participant_operations() {
    for operation in ["ATTACH", "STEP", "SEND", "READ", "GET", "ACK"] {
        let mut owner = owner(1);

        assert_eq!(
            request(&mut owner, 1, operation).operation,
            OwnerOperation::Unsupported
        );
    }
}

#[test]
fn the_owner_frame_limit_counts_every_byte() {
    let mut owner = owner(1);
    let size = OwnerLimits::default().frame_bytes;
    let mut bytes = frame(1, "STATUS", Some(size));

    assert!(owner.receive(&mut bytes, Duration::ZERO).unwrap().is_some());
    assert!(bytes.is_empty());
}

#[test]
fn an_oversized_owner_frame_is_refused_when_its_length_arrives() {
    let mut owner = owner(1);
    let mut length = frame(1, "STATUS", Some(64 * 1024 + 1)).slice(..4);

    assert!(matches!(
        owner.receive(&mut length, Duration::ZERO),
        Err(ConnectionError::Framing(FramingError::Length))
    ));
}

#[test]
fn a_second_data_request_closes_the_owner_link() {
    let mut owner = owner(1);
    request(&mut owner, 1, "REVOKE");

    assert!(matches!(
        owner.receive(&mut frame(2, "STATUS", None), Duration::ZERO),
        Err(ConnectionError::State)
    ));
}

#[test]
fn owner_ping_can_finish_while_a_change_is_open() {
    let mut owner = owner(1);
    let revoke = request(&mut owner, 1, "REVOKE");
    let ping = request(&mut owner, 2, "PING");

    assert_eq!(ping.operation, OwnerOperation::Ping);
    owner.reply_sent(ping.ticket, Duration::ZERO).unwrap();
    owner.reply_sent(revoke.ticket, Duration::ZERO).unwrap();
}

#[test]
fn cancelling_targets_a_tail_and_never_an_owner_change() {
    let mut owner = owner(1);
    let change = request(&mut owner, 1, "REVOKE");
    assert!(owner.cancellable(1).is_none());
    owner.reply_sent(change.ticket, Duration::ZERO).unwrap();
    let tail = request(&mut owner, 2, "TAIL");
    owner.start_tail(tail.ticket).unwrap();

    assert_eq!(owner.cancellable(2), Some(tail.ticket));
}

#[test]
fn a_running_tail_uses_the_connection_lifetime_instead_of_silence() {
    let mut owner = owner(1);
    let tail = request(&mut owner, 1, "TAIL");
    owner.start_tail(tail.ticket).unwrap();

    assert!(owner.tick(OwnerLimits::default().silence).is_ok());
}

#[test]
fn partial_input_keeps_an_owner_link_from_going_silent() {
    let mut owner = owner(1);
    let silence = OwnerLimits::default().silence;
    owner
        .peer_progress(silence - Duration::from_secs(1))
        .unwrap();

    assert!(owner.tick(silence).is_ok());
}

#[test]
fn a_silent_owner_link_closes() {
    let mut owner = owner(1);

    assert_eq!(
        owner.tick(OwnerLimits::default().silence),
        Err(ConnectionError::Silence)
    );
}

#[test]
fn an_owner_reply_is_scoped_to_the_link_that_admitted_it() {
    let mut first = owner(1);
    let mut second = owner(2);
    let status = request(&mut first, 1, "STATUS");
    request(&mut second, 1, "STATUS");

    assert_eq!(
        second.reply_sent(status.ticket, Duration::ZERO),
        Err(ConnectionError::State)
    );
}
