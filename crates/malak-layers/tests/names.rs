//! Tests for names and IDs accepted by Malak.
use malak_layers::{
    ChannelName, ChannelPattern, CreationId, MessageId, ParticipantId, ParticipantName, Position,
};

#[test]
fn channel_names_reject_aliases_and_reserved_prefixes() {
    for name in [
        "",
        "/jobs",
        "jobs/",
        "a//b",
        ".",
        "a/../b",
        "a\\b",
        "_witness",
        "imported/reports",
        "a\nb",
        "A",
        "a/b/c/d/e/f/g/h/i",
    ] {
        assert!(name.parse::<ChannelName>().is_err(), "{name:?}");
    }
}

#[test]
fn message_ids_use_the_same_spelling_across_retries() {
    let id: MessageId = "CI.run_1:Job-2".parse().unwrap();
    assert_eq!(id.as_str(), "CI.run_1:Job-2");
}

#[test]
fn names_enforce_their_byte_limits() {
    assert!("x".repeat(128).parse::<ChannelName>().is_ok());
    assert!("x".repeat(129).parse::<ChannelName>().is_err());
    assert!("x".repeat(128).parse::<MessageId>().is_ok());
    assert!("x".repeat(129).parse::<MessageId>().is_err());
    assert!("x".repeat(64).parse::<ParticipantName>().is_ok());
    assert!("x".repeat(65).parse::<ParticipantName>().is_err());
}

#[test]
fn lead_is_the_only_id_outside_the_enrolment_alphabet_and_length() {
    for id in [
        "worker1",
        "Worker12",
        "worker_12",
        "a01234567890123456789012345678901b",
    ] {
        assert!(id.parse::<ParticipantId>().is_err(), "{id}");
    }
    assert_eq!(
        "lead".parse::<ParticipantId>().unwrap(),
        ParticipantId::lead()
    );
}

#[test]
fn inboxes_follow_participant_ids() {
    let worker: ParticipantId = "worker12".parse().unwrap();
    assert_eq!(worker.inbox().as_str(), "to/worker12");
    assert_eq!(ParticipantId::lead().inbox().as_str(), "lead");
}

#[test]
fn wildcard_matches_exactly_one_segment() {
    let pattern: ChannelPattern = "a/*".parse().unwrap();
    assert!(pattern.matches(&"a/b".parse().unwrap()));
    assert!(!pattern.matches(&"a".parse().unwrap()));
    assert!(!pattern.matches(&"a/b/c".parse().unwrap()));
}

#[test]
fn double_wildcard_requires_a_nonempty_suffix() {
    let pattern: ChannelPattern = "a/**".parse().unwrap();
    assert!(pattern.matches(&"a/b/c".parse().unwrap()));
    assert!(!pattern.matches(&"a".parse().unwrap()));
}

#[test]
fn patterns_reject_partial_and_nonterminal_double_wildcards() {
    for pattern in ["a*/b", "**/b", "a/**/b", "a/***", "a//b"] {
        assert!(pattern.parse::<ChannelPattern>().is_err(), "{pattern}");
    }
}

#[test]
fn creation_id_round_trips_in_lowercase_hex() {
    let id = CreationId::from_bytes([0xab; 16]);
    assert_eq!(id.to_string(), "ab".repeat(16));
    assert_eq!(id.to_string().parse::<CreationId>(), Ok(id));
    assert!("AB".repeat(16).parse::<CreationId>().is_err());
}

#[test]
fn position_is_one_based_and_signed_64_bit_bounded() {
    assert!(Position::try_from(0).is_err());
    assert_eq!(Position::try_from(1), Ok(Position::FIRST));
    assert!(Position::try_from(Position::MAX).is_ok());
    assert!(Position::try_from(Position::MAX + 1).is_err());
}
