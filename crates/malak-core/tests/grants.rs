//! Grant shape and matching from the call and authority contracts.

use malak_core::{Action, Grant};
use std::time::{Duration, UNIX_EPOCH};

#[test]
fn empty_grant_allows_nothing() {
    assert!(!Grant::default().covers(Action::Send, &"lead".parse().unwrap()));
}

#[test]
fn read_permission_does_not_grant_get() {
    let mut grant = Grant::default();
    grant.allow("work/*".parse().unwrap(), [Action::Read]);

    let channel = "work/reports".parse().unwrap();
    assert!(grant.covers(Action::Read, &channel));
    assert!(!grant.covers(Action::Get, &channel));
}

#[test]
fn matching_rules_are_additive() {
    let mut grant = Grant::default();
    grant.allow("work/*".parse().unwrap(), [Action::Read]);
    grant.allow("work/reports".parse().unwrap(), [Action::Ack]);

    let channel = "work/reports".parse().unwrap();
    assert!(grant.covers(Action::Read, &channel));
    assert!(grant.covers(Action::Ack, &channel));
}

#[test]
fn a_grant_expires_at_its_deadline() {
    let mut grant = Grant::default();
    let end = UNIX_EPOCH + Duration::from_secs(1);
    grant.set_end(Some(end));

    assert!(!grant.expired_at(UNIX_EPOCH));
    assert!(grant.expired_at(end));
}
