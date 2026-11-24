//! Tests for configuration limits shared by calls and the wire protocol.

mod common;

use common::{board, limits, publisher};
use malak_core::{Limits, Outcome, SendRequest};
use std::time::Duration;

#[test]
fn default_limits_fit_together() {
    assert!(Limits::default().validate().is_ok());
}

#[test]
fn largest_message_must_fit_a_read_reply() {
    let mut limits = Limits::default();
    limits.read_reply_bytes = limits.body_bytes + 16 * 1024 + 5;
    assert!(limits.validate().is_err());
}

#[test]
fn read_reply_must_fit_the_outgoing_queue() {
    let mut limits = Limits::default();
    limits.outgoing_message_bytes = limits.read_reply_bytes - 1;
    assert!(limits.validate().is_err());
}

#[test]
fn open_requests_cannot_use_all_closing_places() {
    let mut limits = Limits::default();
    limits.closing_reply_places = limits.open_data_requests;
    assert!(limits.validate().is_err());
}

#[test]
fn excess_read_channels_are_refused_repeatedly() {
    let owner = board(limits());
    let lead = owner.lead();
    let names: Vec<_> = (0..65).map(|n| format!("channel-{n}")).collect();
    let names: Vec<_> = names.iter().map(String::as_str).collect();

    assert!(lead.read(&names[..64], 1, Duration::ZERO).is_ok());
    for _ in 0..32 {
        let error = lead.read(&names, 1, Duration::ZERO).unwrap_err();
        assert_eq!(error.outcome(), Outcome::Refused);
        assert!(error.is_limit());
    }
}

#[test]
fn a_large_read_request_stops_at_the_message_budget() {
    let owner = board(limits());
    let lead = owner.lead();
    for n in 0..120 {
        lead.send(SendRequest::note("lead", &n.to_string(), b"record"))
            .unwrap();
    }

    let first = lead.read(&["lead"], usize::MAX, Duration::ZERO).unwrap();
    assert_eq!(first.messages.len(), 100);
    let last = first.messages.last().unwrap().receipt;
    lead.ack("lead", last.creation, last.position.get())
        .unwrap();

    let rest = lead.read(&["lead"], usize::MAX, Duration::ZERO).unwrap();
    assert_eq!(rest.messages.len(), 20);
    assert_eq!(rest.messages[0].receipt.position.get(), 101);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn longest_read_wait_is_clamped() {
    let mut settings = limits();
    settings.read_wait = Duration::from_millis(10);
    let owner = board(settings);
    let lead = owner.lead();
    malak_testkit::tasks::spawn(move || {
        let reply = lead.read(&["lead"], 1, Duration::from_secs(3600)).unwrap();
        assert!(reply.messages.is_empty());
    })
    .wait(Duration::from_secs(5))
    .expect("the board did not clamp the requested wait");
}

#[test]
fn exhausted_disk_budget_refuses_worker_sends_but_preserves_lead_access() {
    let mut settings = limits();
    // Force a low-space decision without filling the host filesystem.
    settings.minimum_free_disk_bytes = u64::MAX;
    let owner = board(settings);
    let worker = publisher(&owner, "publisher");

    for n in 0..32 {
        let error = worker
            .send(SendRequest::note("lead", &n.to_string(), b"record"))
            .unwrap_err();
        assert_eq!(error.outcome(), Outcome::Unavailable);
        assert!(error.is_limit());
    }
    assert_eq!(
        owner
            .lead()
            .send(SendRequest::note("lead", "lead", b"stored"))
            .unwrap()
            .position
            .get(),
        1
    );
}
