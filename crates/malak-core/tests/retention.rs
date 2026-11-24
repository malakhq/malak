//! Direct retention boundaries and what a trim leaves behind.

mod common;

use common::{board, limits, publisher};
use malak_core::{GetReply, Outcome, RetentionRule, SendRequest};
use std::time::Duration;

#[test]
fn retention_removes_only_the_oldest_messages() {
    let mut settings = limits();
    settings.retention.push(RetentionRule {
        pattern: "work/*".parse().unwrap(),
        messages: 1,
        age: Duration::from_secs(3_600),
    });
    let owner = board(settings);
    let lead = owner.lead();
    let old = lead
        .send(SendRequest::note("work/report", "old", b"old"))
        .unwrap();
    let latest = lead
        .send(SendRequest::note("work/report", "new", b"new"))
        .unwrap();

    assert_eq!(
        lead.get("work/report", old.position.get()).unwrap(),
        GetReply::Trimmed {
            creation: old.creation,
            before: latest.position.get()
        },
    );
    assert!(matches!(
        lead.get("work/report", latest.position.get()),
        Ok(GetReply::Message(_))
    ));
}

#[test]
fn unread_trimmed_positions_are_reported_at_the_boundary() {
    let owner = board(limits());
    let lead = owner.lead();
    let first = lead.send(SendRequest::note("lead", "one", b"one")).unwrap();
    let second = lead.send(SendRequest::note("lead", "two", b"two")).unwrap();
    lead.read(&["lead"], 1, Duration::ZERO).unwrap();

    owner
        .trim("lead", first.creation, second.position.get())
        .unwrap();

    let due = lead.read(&["lead"], 1, Duration::ZERO).unwrap();
    assert_eq!(due.channels[0].trimmed_before, Some(second.position.get()));
    assert_eq!(due.messages[0].receipt, second);
}

#[test]
fn trim_does_not_shorten_the_retry_window() {
    let owner = board(limits());
    let lead = owner.lead();
    let request = SendRequest::note("lead", "one", b"one");
    let before = lead.send(request).unwrap();
    owner
        .trim("lead", before.creation, before.position.get() + 1)
        .unwrap();

    let retry = lead.send(request).unwrap();

    assert_eq!(
        (retry.creation, retry.position, retry.durability),
        (before.creation, before.position, before.durability)
    );
    assert!(retry.duplicate);
}

#[test]
fn trim_does_not_refund_the_daily_allowance() {
    let mut settings = limits();
    settings.daily_storage_bytes = 300;
    let owner = board(settings);
    let worker = publisher(&owner, "worker");
    let receipt = worker
        .send(SendRequest::note("lead", "one", b"one"))
        .unwrap();
    owner.trim("lead", receipt.creation, 2).unwrap();

    let result = worker.send(SendRequest::note("lead", "two", b"two"));

    assert_eq!(result.unwrap_err().outcome(), Outcome::Unavailable);
}

#[test]
fn zero_maximum_age_trims_at_the_next_history_check() {
    let mut settings = limits();
    settings.retention.push(RetentionRule {
        pattern: "lead".parse().unwrap(),
        messages: 100,
        age: Duration::ZERO,
    });
    let owner = board(settings);
    let lead = owner.lead();
    let receipt = lead.send(SendRequest::note("lead", "one", b"old")).unwrap();
    assert_eq!(
        lead.get("lead", 1).unwrap(),
        GetReply::Trimmed {
            creation: receipt.creation,
            before: 2
        }
    );
}
