use super::*;
use malak_core::{Handle, HeaderValue, Kind, Outcome, Receipt, TraceId};
use std::time::{Duration, Instant};

fn lead_task(lead: &Handle, id: &str, headers: &[(&str, HeaderValue<'_>)]) -> Receipt {
    lead.send(SendRequest {
        kind: Kind::Task,
        headers,
        ..SendRequest::note("lead", id, b"work")
    })
    .unwrap()
}

fn read(lead: &Handle, count: usize) {
    lead.read(&["lead"], count, Duration::ZERO).unwrap();
}

fn ack(lead: &Handle, receipt: Receipt) {
    lead.ack("lead", receipt.creation, receipt.position.get())
        .unwrap();
}

fn send_after_restore(lead: &Handle, request: SendRequest<'_>) -> Receipt {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match lead.send(request) {
            Ok(receipt) => return receipt,
            Err(error) if error.outcome() == Outcome::Unavailable && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("could not send after restart: {error}"),
        }
    }
}

#[test]
fn restart_uses_the_saved_inbox_task_until_the_newer_task_is_given_again() {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let lead = board.lead();
    let older = lead_task(&lead, "older", &[("trace", HeaderValue::Text("A1"))]);
    let newer = lead_task(&lead, "newer", &[("trace", HeaderValue::Text("B2"))]);
    read(&lead, 2);
    ack(&lead, older);
    drop((board, lead));

    let lead = fixture.restart().lead();
    let first = send_after_restore(&lead, SendRequest::note("lead", "progress", b"first"));
    assert_eq!(first.trace, older.trace);
    read(&lead, 1);
    let later = lead
        .send(SendRequest::note("lead", "after", b"second"))
        .unwrap();
    assert_eq!(later.trace, newer.trace);
}

#[test]
fn restart_uses_a_traceless_acknowledged_task_over_an_older_trace() {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let lead = board.lead();
    let older = lead_task(&lead, "older", &[]);
    let reply = HeaderValue::ReplyTo {
        channel: "lead",
        creation: older.creation,
        position: 3,
    };
    let traceless = lead_task(&lead, "traceless", &[("reply_to", reply)]);
    read(&lead, 2);
    ack(&lead, traceless);
    drop((board, lead));

    let lead = fixture.restart().lead();
    let request = SendRequest {
        kind: Kind::Task,
        ..SendRequest::note("lead", "new", b"")
    };
    assert_eq!(send_after_restore(&lead, request).trace, None);
}

fn trace_after_restart_without_an_eligible_task(ack_and_trim: bool) -> Option<TraceId> {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let lead = board.lead();
    let task = lead_task(&lead, "task", &[]);
    read(&lead, 1);
    if ack_and_trim {
        ack(&lead, task);
        board.trim("lead", task.creation, 2).unwrap();
    }
    drop((board, lead));
    let lead = fixture.restart().lead();
    send_after_restore(&lead, SendRequest::note("lead", "progress", b"")).trace
}

#[test]
fn restart_cannot_restore_a_task_trimmed_from_the_inbox() {
    assert_eq!(trace_after_restart_without_an_eligible_task(true), None);
}

#[test]
fn restart_cannot_restore_a_task_that_was_never_acknowledged() {
    assert_eq!(trace_after_restart_without_an_eligible_task(false), None);
}

#[test]
fn restart_does_not_restore_a_task_whose_record_fails_verification() {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let lead = board.lead();
    let task = lead_task(&lead, "task", &[]);
    read(&lead, 1);
    ack(&lead, task);
    board.stop(board.status(None, None).run).unwrap();
    drop((board, lead));

    let path = fixture.record_file(b"\nwork\n");
    let bytes = fs::read_to_string(&path).unwrap();
    let sealed = bytes.lines().next().unwrap().split(' ').nth(6).unwrap();
    let mut damaged = sealed.as_bytes().to_vec();
    let index = damaged.len() - 8;
    damaged[index] = if damaged[index] == b'A' { b'B' } else { b'A' };
    let damaged = String::from_utf8(damaged).unwrap();
    fs::write(path, bytes.replacen(sealed, &damaged, 1)).unwrap();
    let lead = fixture.restart().lead();

    let report = send_after_restore(&lead, SendRequest::note("reports", "progress", b""));
    assert_eq!(report.trace, None);
}

#[test]
fn a_retry_keeps_its_original_implicit_trace_after_a_new_task_and_restart() {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let lead = board.lead();
    let first = lead_task(&lead, "first", &[("trace", HeaderValue::Text("A1"))]);
    let later = lead_task(&lead, "later", &[("trace", HeaderValue::Text("B2"))]);
    let request = SendRequest::note("lead", "progress", b"work");
    read(&lead, 1);
    let original = lead.send(request).unwrap();
    assert_eq!(original.trace, first.trace);
    read(&lead, 2);
    ack(&lead, later);
    drop((board, lead));

    let lead = fixture.restart().lead();
    let mut expected = original;
    expected.duplicate = true;
    let changed = SendRequest {
        headers: &[("trace", HeaderValue::Text("FF"))],
        ..request
    };
    assert_eq!(send_after_restore(&lead, changed), expected);
    assert_eq!(send_after_restore(&lead, request), expected);
}
