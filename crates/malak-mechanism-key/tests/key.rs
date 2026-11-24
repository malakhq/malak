//! Key enrolment and proof tests. The board owns identity admission.

use malak_layers::{Enrolment, Grade, Handover, Mechanism, ParticipantId, Session, Side, Step};
use malak_mechanism_key::Key;
use malak_proto::{Binding, Entrance, Initiator, KeyPair, Responder};

fn enrolled() -> (ParticipantId, Handover) {
    let who = "worker01".parse().unwrap();
    let hints = Enrolment {
        allowed_place: None,
        account: None,
    };
    let handover = Key.enrol(&who, &hints).unwrap();
    (who, handover)
}

#[test]
fn key_declares_that_it_registers_at_enrolment() {
    let descriptor = Key.describe();
    assert!(descriptor.key_at_enrolment);
    assert_eq!(descriptor.highest_grade, Grade::Proven);
    assert!(!descriptor.has_veto);
}

#[test]
fn issued_private_key_matches_the_key_to_register() {
    let (_, handover) = enrolled();
    let board = KeyPair::generate().unwrap();
    let binding = Binding {
        version: 1,
        entrance: Entrance::General,
    };
    let (_, opening) = Initiator::open(
        binding,
        handover.private_key.as_ref().unwrap(),
        board.public(),
    )
    .unwrap();

    let responder = Responder::read(binding, board.private(), board.public(), &opening).unwrap();

    assert_eq!(Some(*responder.caller_key()), handover.public_key);
}

#[test]
fn subsequent_key_generation_produces_a_new_pair() {
    let (who, first) = enrolled();
    let hints = Enrolment {
        allowed_place: None,
        account: None,
    };
    let second = Key.enrol(&who, &hints).unwrap();
    assert_ne!(first.public_key, second.public_key);
}

#[test]
fn key_does_not_request_a_challenge_or_authority_change() {
    let (who, handover) = enrolled();
    for side in [Side::Participant, Side::Board] {
        let session = Session {
            side,
            public_key: handover.public_key.as_ref().unwrap(),
            binding: &[10; 32],
            source: None,
            claimed: &who,
            record: None,
            handover: Some(&handover.contents),
        };

        assert!(matches!(
            Key.step(&session, b"").unwrap(),
            Step::Proven {
                ends: None,
                register_key: false,
                record: None
            },
        ));
    }
}
