//! Participant connection states, cancellation, and traffic limits.

use bytes::Bytes;
use malak_proto::{
    Attachment, Connection, ConnectionError, ConnectionLimits, FramingError, Input, Operation,
    Phase, Placement, Request, frame_prefix,
};
use minicbor::Encoder;
use std::time::Duration;

fn frame(op: &str, rid: u64, body: &[u8]) -> Bytes {
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(2)
        .unwrap()
        .str("op")
        .unwrap()
        .str(op)
        .unwrap()
        .str("rid")
        .unwrap()
        .u64(rid)
        .unwrap();
    let header = encoder.into_writer();
    let mut bytes = Vec::from(frame_prefix(header.len(), body.len()).unwrap());
    bytes.extend_from_slice(&header);
    bytes.extend_from_slice(body);
    bytes.into()
}

fn input(connection: &mut Connection, op: &str, rid: u64) -> Result<Input, ConnectionError> {
    connection.receive(&mut frame(op, rid, b""), Duration::ZERO)
}

fn request(connection: &mut Connection, op: &str, rid: u64) -> Request {
    let Input::Request(request) = input(connection, op, rid).unwrap() else {
        panic!("expected admitted request");
    };
    request
}

fn attaching(limits: ConnectionLimits) -> Connection {
    let mut connection = Connection::new(limits).unwrap();
    connection
        .handshake_reply_sent(&[1; 32], Duration::ZERO)
        .unwrap();
    connection
}

fn attach(connection: &mut Connection) {
    let first = request(connection, "ATTACH", 0);
    connection
        .attachment_reply_sent(first.ticket, Attachment::Accepted, Duration::ZERO)
        .unwrap();
}

fn attached() -> Connection {
    let mut connection = attaching(ConnectionLimits::default());
    attach(&mut connection);
    connection
}

#[test]
fn data_cannot_attach_a_connection() {
    let mut connection = attaching(ConnectionLimits::default());
    assert!(matches!(
        input(&mut connection, "SEND", 1),
        Err(ConnectionError::Operation)
    ));
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn attach_remains_pending_until_its_ok_is_sent() {
    let mut connection = attaching(ConnectionLimits::default());
    let first = request(&mut connection, "ATTACH", 1);
    assert_eq!(connection.phase(), Phase::Attaching);
    connection
        .attachment_reply_sent(first.ticket, Attachment::Accepted, Duration::ZERO)
        .unwrap();
    assert_eq!(connection.phase(), Phase::Attached);
}

#[test]
fn multi_round_attach_accepts_step_after_each_reply() {
    let mut connection = attaching(ConnectionLimits::default());
    let first = request(&mut connection, "ATTACH", 1);
    connection
        .attachment_reply_sent(first.ticket, Attachment::More, Duration::ZERO)
        .unwrap();

    let next = request(&mut connection, "STEP", 2);
    connection
        .attachment_reply_sent(next.ticket, Attachment::Accepted, Duration::ZERO)
        .unwrap();

    assert_eq!(connection.phase(), Phase::Attached);
}

#[test]
fn step_cannot_arrive_before_the_previous_reply() {
    let mut connection = attaching(ConnectionLimits::default());
    request(&mut connection, "ATTACH", 1);

    assert!(matches!(
        input(&mut connection, "STEP", 2),
        Err(ConnectionError::State)
    ));
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn a_fifth_attach_round_cannot_begin() {
    let mut connection = attaching(ConnectionLimits::default());
    let first = request(&mut connection, "ATTACH", 1);
    let mut ticket = first.ticket;
    for rid in 2..=4 {
        connection
            .attachment_reply_sent(ticket, Attachment::More, Duration::ZERO)
            .unwrap();
        ticket = request(&mut connection, "STEP", rid).ticket;
    }

    assert_eq!(
        connection.attachment_reply_sent(ticket, Attachment::More, Duration::ZERO),
        Err(ConnectionError::Deadline)
    );
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn attach_byte_budget_counts_length_bytes_and_all_rounds() {
    let mut limits = ConnectionLimits::default();
    limits.attach_bytes = frame("ATTACH", 1, b"").len() + frame("STEP", 2, b"").len() - 1;
    let mut connection = attaching(limits);
    let first = request(&mut connection, "ATTACH", 1);
    connection
        .attachment_reply_sent(first.ticket, Attachment::More, Duration::ZERO)
        .unwrap();

    assert!(matches!(
        input(&mut connection, "STEP", 2),
        Err(ConnectionError::Framing(FramingError::Budget))
    ));
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn opening_and_attach_share_the_deadline() {
    let mut connection = Connection::new(ConnectionLimits::default()).unwrap();
    connection
        .handshake_reply_sent(&[1; 32], Duration::from_secs(4))
        .unwrap();
    let mut late = frame("ATTACH", 1, b"");

    assert!(matches!(
        connection.receive(&mut late, Duration::from_secs(5)),
        Err(ConnectionError::Deadline)
    ));
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn rejected_attach_has_no_data_phase() {
    let mut connection = attaching(ConnectionLimits::default());
    let first = request(&mut connection, "ATTACH", 1);

    connection
        .attachment_reply_sent(first.ticket, Attachment::Unavailable, Duration::ZERO)
        .unwrap();

    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn unexpected_operations_close_the_attached_connection() {
    for op in [
        "ATTACH",
        "STEP",
        "STATUS",
        "ENROL",
        "OK",
        "ERR",
        "MSG",
        "UNDEFINED",
    ] {
        let mut connection = attached();
        assert!(
            matches!(
                input(&mut connection, op, 1),
                Err(ConnectionError::Operation)
            ),
            "{op}"
        );
        assert_eq!(connection.phase(), Phase::Closed);
    }
}

#[test]
fn a_body_on_a_bodyless_operation_closes_the_connection() {
    let mut connection = attached();
    let mut invalid = frame("PING", 1, b"unexpected");
    assert!(matches!(
        connection.receive(&mut invalid, Duration::ZERO),
        Err(ConnectionError::Body)
    ));
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn an_open_request_number_cannot_be_reused() {
    let mut connection = attached();
    request(&mut connection, "READ", 1);
    assert!(matches!(
        input(&mut connection, "PING", 1),
        Err(ConnectionError::RequestInUse)
    ));
    assert_eq!(connection.phase(), Phase::Closed);
}

#[test]
fn old_ticket_cannot_close_a_request_that_reuses_the_number() {
    let mut connection = attached();
    let original = request(&mut connection, "READ", 1);
    assert!(
        connection
            .enqueue_closing(original.ticket, frame("OK", 1, b""), Duration::ZERO)
            .unwrap()
    );
    let replacement = request(&mut connection, "READ", 1);

    assert!(
        !connection
            .enqueue_closing(original.ticket, frame("OK", 1, b""), Duration::ZERO)
            .unwrap()
    );
    assert_eq!(connection.cancellable(1), Some(replacement.ticket));
}

#[test]
fn eight_waiting_reads_leave_control_requests_admissible() {
    let mut connection = attached();
    for rid in 1..=8 {
        request(&mut connection, "READ", rid);
    }

    assert!(matches!(
        input(&mut connection, "GET", 9),
        Ok(Input::Unavailable(_))
    ));
    assert_eq!(
        request(&mut connection, "CANCEL", 10).operation,
        Operation::Cancel
    );
    assert_eq!(
        request(&mut connection, "PING", 11).operation,
        Operation::Ping
    );
}

#[test]
fn only_an_open_read_can_be_cancelled() {
    let mut connection = attached();
    let read = request(&mut connection, "READ", 1);
    request(&mut connection, "SEND", 2);
    assert!(connection.cancellable(1).is_some());
    assert!(connection.cancellable(2).is_none());
    assert!(
        connection
            .enqueue_closing(read.ticket, frame("OK", 1, b""), Duration::ZERO)
            .unwrap()
    );
    assert!(connection.cancellable(1).is_none());
}

#[test]
fn queued_closing_reply_keeps_its_place_until_written() {
    let mut limits = ConnectionLimits::default();
    limits.reply_places = 2;
    limits.open_data_requests = 1;
    let mut connection = attaching(limits);
    attach(&mut connection);
    let first = request(&mut connection, "PING", 1);
    request(&mut connection, "PING", 2);
    assert!(
        connection
            .enqueue_closing(first.ticket, frame("OK", 1, b""), Duration::ZERO)
            .unwrap()
    );

    let mut next = frame("PING", 3, b"");
    let unconsumed = next.clone();
    assert!(matches!(
        connection.receive(&mut next, Duration::ZERO),
        Ok(Input::Paused)
    ));
    assert_eq!(next, unconsumed);

    let reply = connection.outgoing().unwrap().len();
    connection.wrote(reply, reply, Duration::ZERO).unwrap();
    assert!(matches!(
        connection.receive(&mut next, Duration::ZERO),
        Ok(Input::Request(_))
    ));
}

#[test]
fn pressure_refuses_data_requests_but_not_acknowledgement() {
    let mut connection = small_queue();
    pressure(&mut connection);
    for (rid, op) in [(2, "READ"), (3, "GET"), (4, "SEND")] {
        assert!(matches!(
            input(&mut connection, op, rid),
            Ok(Input::Unavailable(_))
        ));
    }
    assert_eq!(request(&mut connection, "ACK", 5).operation, Operation::Ack);
}

fn small_queue() -> Connection {
    let mut limits = ConnectionLimits::default();
    limits.body_bytes = 0;
    limits.message_bytes = 20_000;
    let mut connection = attaching(limits);
    attach(&mut connection);
    connection
}

fn pressure(connection: &mut Connection) -> malak_proto::Ticket {
    let read = request(connection, "READ", 1);
    let message = || Bytes::from(vec![0; 12_000]);
    assert_eq!(
        connection
            .enqueue_message(read.ticket, message(), Duration::ZERO)
            .unwrap(),
        Placement::Placed
    );
    assert_eq!(
        connection
            .enqueue_message(read.ticket, message(), Duration::ZERO)
            .unwrap(),
        Placement::Full
    );
    read.ticket
}

#[test]
fn message_load_counts_only_unwritten_frame_bytes() {
    let mut connection = small_queue();
    let waiting = pressure(&mut connection);

    connection.wrote(2_000, 2_010, Duration::ZERO).unwrap();

    assert_eq!(connection.message_load(), 10_000);
    let smaller = Bytes::from_static(b"message");
    assert_eq!(
        connection
            .enqueue_message(waiting, smaller, Duration::ZERO)
            .unwrap(),
        Placement::Placed
    );
}

#[test]
fn a_control_reply_queues_after_a_full_messages_queue() {
    let mut connection = small_queue();
    pressure(&mut connection);
    let ping = request(&mut connection, "PING", 2);
    let reply = frame("OK", 2, b"");
    assert!(
        connection
            .enqueue_closing(ping.ticket, reply.clone(), Duration::ZERO)
            .unwrap()
    );

    connection.wrote(12_000, 12_000, Duration::ZERO).unwrap();

    assert_eq!(connection.outgoing(), Some(reply.as_ref()));
}

#[test]
fn ready_read_and_cancel_can_queue_only_one_closing() {
    let mut connection = small_queue();
    let waiting = pressure(&mut connection);
    request(&mut connection, "CANCEL", 2);
    let reported = connection.cancellable(1).unwrap();
    assert_eq!(waiting, reported);

    assert!(
        connection
            .enqueue_closing(reported, frame("OK", 1, b""), Duration::ZERO)
            .unwrap()
    );
    assert!(
        !connection
            .enqueue_closing(waiting, frame("OK", 1, b""), Duration::ZERO)
            .unwrap()
    );
    assert!(connection.cancellable(1).is_none());
}

#[test]
fn an_unwritten_queue_closes_at_sixty_seconds() {
    let mut connection = small_queue();
    pressure(&mut connection);

    assert_eq!(
        connection.tick(Duration::from_secs(60)),
        Err(ConnectionError::NoProgress)
    );
    assert!(connection.outgoing().is_none());
}

#[test]
fn one_wire_byte_resets_the_no_progress_clock() {
    let mut connection = small_queue();
    pressure(&mut connection);

    connection.wrote(0, 1, Duration::from_secs(59)).unwrap();

    assert!(connection.tick(Duration::from_secs(118)).is_ok());
}

#[test]
fn any_received_byte_resets_the_silence_clock() {
    let mut connection = attached();
    connection.peer_progress(Duration::from_secs(899)).unwrap();

    assert!(connection.tick(Duration::from_secs(1798)).is_ok());
    assert_eq!(
        connection.tick(Duration::from_secs(1799)),
        Err(ConnectionError::Silence)
    );
}

#[test]
fn silence_clock_pauses_while_the_board_cannot_read() {
    let mut limits = ConnectionLimits::default();
    limits.reply_places = 1;
    limits.open_data_requests = 0;
    let mut connection = attaching(limits);
    attach(&mut connection);

    let late = Duration::from_secs(895);
    connection.peer_progress(late).unwrap();
    let Input::Request(ping) = connection
        .receive(&mut frame("PING", 1, b""), late)
        .unwrap()
    else {
        panic!("ping was not admitted");
    };
    assert!(!connection.can_read());
    assert!(connection.tick(Duration::from_secs(2000)).is_ok());
    let reply = frame("OK", 1, b"");
    let bytes = reply.len();
    connection
        .enqueue_closing(ping.ticket, reply, Duration::from_secs(2000))
        .unwrap();
    connection
        .wrote(bytes, bytes, Duration::from_secs(2000))
        .unwrap();

    assert!(connection.tick(Duration::from_secs(2899)).is_ok());
    assert_eq!(
        connection.tick(Duration::from_secs(2900)),
        Err(ConnectionError::Silence)
    );
}

#[test]
fn a_ticket_from_a_different_link_cannot_close_a_request() {
    let mut first = attached();
    let original = request(&mut first, "READ", 1);
    let mut second = Connection::new(ConnectionLimits::default()).unwrap();
    second
        .handshake_reply_sent(&[2; 32], Duration::ZERO)
        .unwrap();
    attach(&mut second);
    let local = request(&mut second, "READ", 1);

    let foreign = second
        .enqueue_closing(original.ticket, frame("OK", 1, b""), Duration::ZERO)
        .unwrap();

    assert!(!foreign);
    assert_eq!(second.cancellable(1), Some(local.ticket));
}

#[test]
fn an_attached_connection_closes_at_the_lifetime_budget() {
    let mut limits = ConnectionLimits::default();
    limits.silence = Duration::from_secs(25 * 3600);
    let mut connection = attaching(limits);
    attach(&mut connection);

    assert!(connection.tick(Duration::from_secs(24 * 3600 - 1)).is_ok());
    assert_eq!(
        connection.tick(Duration::from_secs(24 * 3600)),
        Err(ConnectionError::Deadline)
    );
    assert_eq!(connection.phase(), Phase::Closed);
}
