//! Implicit trace selection follows the current task delivered by read in a worker's inbox.

mod common;

use common::{Probe, board, grant, limits};
use malak_core::{
    Action, Handle, HeaderValue, Kind, Limits, Outcome, Owner, Receipt, SendRequest, TraceId,
};
use std::time::Duration;

fn worker(board: &Owner) -> Handle {
    board
        .enrol(
            "worker",
            grant(
                "**",
                &[Action::Send, Action::Read, Action::Get, Action::Ack],
            ),
        )
        .unwrap()
}

fn task(sender: &Handle, inbox: &str, id: &str, headers: &[(&str, HeaderValue<'_>)]) -> Receipt {
    sender
        .send(SendRequest {
            kind: Kind::Task,
            headers,
            ..SendRequest::note(inbox, id, b"work")
        })
        .unwrap()
}

fn reply_to(channel: &str, receipt: Receipt) -> (&str, HeaderValue<'_>) {
    (
        "reply_to",
        HeaderValue::ReplyTo {
            channel,
            creation: receipt.creation,
            position: receipt.position.get(),
        },
    )
}

fn read(worker: &Handle, inbox: &str, count: usize) {
    worker.read(&[inbox], count, Duration::ZERO).unwrap();
}

fn report(worker: &Handle, id: &str) -> Option<TraceId> {
    worker
        .send(SendRequest {
            kind: Kind::Report,
            ..SendRequest::note("lead", id, b"progress")
        })
        .unwrap()
        .trace
}

#[test]
fn an_undelivered_own_task_does_not_join_reports() {
    let board = board(limits());
    let worker = worker(&board);
    task(&board.lead(), worker.id().inbox().as_str(), "task", &[]);
    assert_eq!(report(&worker, "report"), None);
}

#[test]
fn a_get_is_not_a_task_delivery() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let task = task(&board.lead(), inbox.as_str(), "task", &[]);
    worker.get(inbox.as_str(), task.position.get()).unwrap();
    assert_eq!(report(&worker, "report"), None);
}

#[test]
fn a_different_reader_does_not_give_the_worker_its_task() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    task(&lead, inbox.as_str(), "task", &[]);
    read(&lead, inbox.as_str(), 1);
    assert_eq!(report(&worker, "report"), None);
}

#[test]
fn a_task_read_in_a_different_channel_does_not_join_reports() {
    let board = board(limits());
    let worker = worker(&board);
    task(&board.lead(), "lead", "task", &[]);
    read(&worker, "lead", 1);
    assert_eq!(report(&worker, "report"), None);
}

#[test]
fn the_newest_delivered_task_is_shared_across_handles_and_older_reads() {
    let board = board(limits());
    let worker = worker(&board);
    let second_handle = board.handle(worker.id()).unwrap();
    let inbox = worker.id().inbox();
    let lead = board.lead();
    let first = task(&lead, inbox.as_str(), "first", &[]);
    let second = task(&lead, inbox.as_str(), "second", &[]);
    assert_ne!(first.trace, second.trace);

    read(&worker, inbox.as_str(), 1);
    assert_eq!(report(&second_handle, "first-progress"), first.trace);
    read(&second_handle, inbox.as_str(), 2);
    read(&worker, inbox.as_str(), 1);
    assert_eq!(report(&worker, "second-progress"), second.trace);
}

#[test]
fn a_read_batch_ending_in_a_report_uses_its_highest_task() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    task(&lead, inbox.as_str(), "first", &[]);
    let second = task(&lead, inbox.as_str(), "second", &[]);
    lead.send(SendRequest {
        kind: Kind::Report,
        headers: &[("trace", HeaderValue::Text("00"))],
        ..SendRequest::note(inbox.as_str(), "after", b"report")
    })
    .unwrap();
    read(&worker, inbox.as_str(), 3);
    assert_eq!(report(&worker, "progress"), second.trace);
}

#[test]
fn ack_and_trim_keep_the_working_task_on_a_new_handle() {
    let board = board(limits());
    let worker = worker(&board);
    let id = worker.id().clone();
    let inbox = id.inbox();
    let task = task(&board.lead(), inbox.as_str(), "task", &[]);
    read(&worker, inbox.as_str(), 1);
    worker
        .ack(inbox.as_str(), task.creation, task.position.get())
        .unwrap();
    board
        .trim(inbox.as_str(), task.creation, task.position.get() + 1)
        .unwrap();
    drop(worker);
    assert_eq!(report(&board.handle(&id).unwrap(), "progress"), task.trace);
}

#[test]
fn removing_an_inbox_ends_the_working_task() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let task = task(&board.lead(), inbox.as_str(), "task", &[]);
    read(&worker, inbox.as_str(), 1);
    board.remove_channel(inbox.as_str(), task.creation).unwrap();
    assert_eq!(report(&worker, "without-inbox"), None);
    board.create_channel(inbox.as_str()).unwrap();
    assert_eq!(report(&worker, "with-recreated-inbox"), None);
}

#[test]
fn a_revoked_worker_cannot_read_or_report_using_the_working_task() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    task(&board.lead(), inbox.as_str(), "task", &[]);
    read(&worker, inbox.as_str(), 1);
    board.revoke(worker.id()).unwrap();
    assert_eq!(
        worker
            .read(&[inbox.as_str()], 1, Duration::ZERO)
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
    assert_eq!(
        worker
            .send(SendRequest::note("lead", "progress", b""))
            .unwrap_err()
            .outcome(),
        Outcome::Refused
    );
}

#[test]
fn a_traceless_working_task_keeps_both_reports_and_tasks_traceless() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    let first = task(&lead, inbox.as_str(), "first", &[]);
    let traceless = task(
        &lead,
        inbox.as_str(),
        "traceless",
        &[reply_to(inbox.as_str(), first)],
    );
    assert_eq!(traceless.trace, None);
    read(&worker, inbox.as_str(), 2);
    assert_ne!(first.trace, None);
    assert_eq!(report(&worker, "progress"), None);
    assert_eq!(task(&worker, "lead", "follow-up", &[]).trace, None);
}

#[test]
fn an_implicit_report_adds_no_store_read() {
    let store = Probe::default();
    let board = Owner::in_memory(store.clone(), limits()).unwrap();
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let task = task(&board.lead(), inbox.as_str(), "task", &[]);
    read(&worker, inbox.as_str(), 1);
    let before = store.calls();
    assert_eq!(report(&worker, "progress"), task.trace);
    assert_eq!(store.calls() - before, 1, "only the append is needed");
}

#[test]
fn a_send_that_uses_the_recipients_task_adds_no_store_read() {
    let store = Probe::default();
    let board = Owner::in_memory(store.clone(), limits()).unwrap();
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    let task = task(&lead, inbox.as_str(), "task", &[]);
    read(&worker, inbox.as_str(), 1);
    let before = store.calls();

    let note = lead
        .send(SendRequest::note(inbox.as_str(), "note", b"progress"))
        .unwrap();

    assert!(task.trace.is_some());
    assert_eq!(note.trace, task.trace);
    assert_eq!(store.calls() - before, 1, "only the append is needed");
}

#[test]
fn a_senders_working_task_takes_precedence_in_another_inbox() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    task(
        &lead,
        inbox.as_str(),
        "workers-task",
        &[("trace", HeaderValue::Text("A1"))],
    );
    let own = task(
        &lead,
        "lead",
        "leads-task",
        &[("trace", HeaderValue::Text("B2"))],
    );
    read(&worker, inbox.as_str(), 1);
    read(&lead, "lead", 1);

    let follow_up = task(&lead, inbox.as_str(), "follow-up", &[]);

    assert_eq!(follow_up.trace, own.trace);
}

#[test]
fn a_traceless_senders_task_takes_precedence_over_a_recipients_trace() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    let original = task(&lead, inbox.as_str(), "workers-task", &[]);
    task(
        &lead,
        "lead",
        "traceless",
        &[reply_to(inbox.as_str(), original)],
    );
    read(&worker, inbox.as_str(), 1);
    read(&lead, "lead", 1);

    assert_eq!(task(&lead, inbox.as_str(), "follow-up", &[]).trace, None);
}

#[test]
fn a_traceless_recipients_task_keeps_new_tasks_traceless() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    let original = task(&lead, inbox.as_str(), "original", &[]);
    task(
        &lead,
        inbox.as_str(),
        "traceless",
        &[reply_to(inbox.as_str(), original)],
    );
    read(&worker, inbox.as_str(), 2);

    assert_eq!(task(&lead, inbox.as_str(), "follow-up", &[]).trace, None);
}

#[test]
fn an_explicit_trace_takes_precedence_in_a_working_inbox() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    task(
        &lead,
        inbox.as_str(),
        "task",
        &[("trace", HeaderValue::Text("A1"))],
    );
    read(&worker, inbox.as_str(), 1);

    let explicit = task(
        &lead,
        inbox.as_str(),
        "explicit",
        &[("trace", HeaderValue::Text("B2"))],
    );

    assert_eq!(explicit.trace.unwrap().as_str(), "B2");
}

#[test]
fn an_undelivered_task_supplies_no_recipient_trace() {
    let board = board(limits());
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    let lead = board.lead();
    let original = task(&lead, inbox.as_str(), "unread", &[]);

    let note = lead
        .send(SendRequest::note(inbox.as_str(), "note", b""))
        .unwrap();
    let follow_up = task(&lead, inbox.as_str(), "follow-up", &[]);

    assert_eq!(note.trace, None);
    assert!(follow_up.trace.is_some());
    assert_ne!(follow_up.trace, original.trace);
}

#[test]
fn channels_without_recipient_fallback_keep_their_default_traces() {
    let board = board(limits());
    let worker = worker(&board);
    let lead = board.lead();
    let original = task(&lead, "lead", "task", &[]);
    read(&lead, "lead", 1);

    for channel in ["lead", "questions", "orders", "relay", "to/lead"] {
        board.create_channel(channel).unwrap();
        let note = worker
            .send(SendRequest::note(channel, "note", b""))
            .unwrap();
        let new_task = task(&worker, channel, "new-task", &[]);

        assert_eq!(note.trace, None, "{channel}");
        assert!(new_task.trace.is_some(), "{channel}");
        assert_ne!(new_task.trace, original.trace, "{channel}");
    }
}

fn implicit_trace_reaches_header_limit(limits: Limits) {
    let store = Probe::default();
    let board = Owner::in_memory(store.clone(), limits).unwrap();
    let worker = worker(&board);
    let inbox = worker.id().inbox();
    task(
        &board.lead(),
        inbox.as_str(),
        "task",
        &[("trace", HeaderValue::Text("12345678"))],
    );
    let request = SendRequest {
        headers: &[("x-comment", HeaderValue::Text("value"))],
        ..SendRequest::note("lead", "progress", b"")
    };
    let original = worker.send(request).unwrap();
    assert_eq!(original.trace, None);
    read(&worker, inbox.as_str(), 1);
    let before = store.calls();

    let duplicate = worker.send(request).unwrap();
    let mut expected = original;
    expected.duplicate = true;
    assert_eq!(duplicate, expected);
    let error = worker
        .send(SendRequest {
            id: "different",
            ..request
        })
        .unwrap_err();
    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(
        error
            .to_string()
            .contains("board-selected trace would exceed header limits")
    );
    assert_eq!(store.calls(), before);
}

#[test]
fn a_new_implicit_trace_counts_towards_the_header_entry_limit() {
    let mut limits = limits();
    limits.header_entries = 1;
    implicit_trace_reaches_header_limit(limits);
}

#[test]
fn a_new_implicit_trace_counts_towards_the_header_byte_limit() {
    let mut limits = limits();
    limits.header_bytes = 14;
    implicit_trace_reaches_header_limit(limits);
}
