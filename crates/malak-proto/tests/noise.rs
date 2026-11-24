//! Noise IK handshake and protected frame boundaries.

use malak_proto::{Binding, Encrypted, Entrance, HandshakeError, Initiator, KeyPair, Responder};

const GENERAL: Binding = Binding {
    version: 1,
    entrance: Entrance::General,
};
const TEST_CONTENT: &[u8] = b"synthetic frame contents";

struct Keys {
    caller: KeyPair,
    board: KeyPair,
}

impl Keys {
    fn new() -> Self {
        Self {
            caller: KeyPair::generate().unwrap(),
            board: KeyPair::generate().unwrap(),
        }
    }

    fn initiate(&self) -> (Initiator, [u8; malak_proto::OPENING_BYTES]) {
        Initiator::open(GENERAL, self.caller.private(), self.board.public()).unwrap()
    }

    fn respond(&self, binding: Binding, opening: &[u8]) -> Result<Responder, HandshakeError> {
        Responder::read(binding, self.board.private(), self.board.public(), opening)
    }

    fn exchange(&self) -> (Encrypted, Encrypted) {
        let (client, opening) = self.initiate();
        let (board, reply) = self.respond(GENERAL, &opening).unwrap().reply().unwrap();
        (client.finish(&reply).unwrap(), board)
    }
}

fn seal(state: &mut Encrypted, content: &[u8]) -> Vec<u8> {
    let mut segment = vec![0; content.len() + malak_proto::TAG_BYTES];
    let length = state.seal(content, &mut segment).unwrap();
    segment.truncate(length);
    segment
}

#[test]
fn the_board_learns_the_caller_key_before_replying() {
    let keys = Keys::new();
    let (_, opening) = keys.initiate();
    let state = keys.respond(GENERAL, &opening).unwrap();
    assert_eq!(state.caller_key(), keys.caller.public());
}

#[test]
fn both_sides_obtain_the_same_connection_binding() {
    let (caller, board) = Keys::new().exchange();
    assert_eq!(caller.handshake_hash(), board.handshake_hash());
}

#[test]
fn a_new_handshake_has_a_new_connection_binding() {
    let keys = Keys::new();
    let (first, _) = keys.exchange();
    let (second, _) = keys.exchange();
    assert_ne!(first.handshake_hash(), second.handshake_hash());
}

#[test]
fn protected_segments_round_trip_in_both_directions() {
    let (mut caller, mut board) = Keys::new().exchange();
    let mut output = [0; TEST_CONTENT.len()];

    assert_eq!(
        board.open(&seal(&mut caller, TEST_CONTENT), &mut output),
        Ok(output.len())
    );
    assert_eq!(&output, TEST_CONTENT);
    assert_eq!(
        caller.open(&seal(&mut board, TEST_CONTENT), &mut output),
        Ok(output.len())
    );
    assert_eq!(&output, TEST_CONTENT);
}

#[test]
fn wrong_entrance_cannot_read_the_opening() {
    let keys = Keys::new();
    let (_, opening) = keys.initiate();

    for entrance in [Entrance::Lead, Entrance::Owner] {
        let binding = Binding {
            entrance,
            ..GENERAL
        };
        assert!(matches!(
            keys.respond(binding, &opening),
            Err(HandshakeError::Authentication)
        ));
    }
}

#[test]
fn wrong_version_cannot_read_the_opening() {
    let keys = Keys::new();
    let (_, opening) = keys.initiate();
    let changed = Binding {
        version: GENERAL.version + 1,
        ..GENERAL
    };
    assert!(matches!(
        keys.respond(changed, &opening),
        Err(HandshakeError::Authentication)
    ));
}

#[test]
fn another_board_does_not_hold_the_pinned_key() {
    let keys = Keys::new();
    let (_, opening) = keys.initiate();
    let other = KeyPair::generate().unwrap();
    let result = Responder::read(GENERAL, other.private(), other.public(), &opening);
    assert!(matches!(result, Err(HandshakeError::Authentication)));
}

#[test]
fn the_opening_must_be_the_fixed_payloadless_length() {
    let keys = Keys::new();
    let (_, opening) = keys.initiate();
    let result = keys.respond(GENERAL, &opening[1..]);
    assert!(matches!(result, Err(HandshakeError::Length)));
}

#[test]
fn a_replayed_opening_cannot_authenticate_the_old_first_segment() {
    let keys = Keys::new();
    let (caller, opening) = keys.initiate();
    let (_, reply) = keys.respond(GENERAL, &opening).unwrap().reply().unwrap();
    let recorded_segment = seal(&mut caller.finish(&reply).unwrap(), TEST_CONTENT);

    let (mut replayed_board, _) = keys.respond(GENERAL, &opening).unwrap().reply().unwrap();

    let mut output = [0; TEST_CONTENT.len()];
    assert_eq!(
        replayed_board.open(&recorded_segment, &mut output),
        Err(HandshakeError::Authentication)
    );
    assert!(replayed_board.is_closed());
}

#[test]
fn a_modified_segment_permanently_closes_the_cipher() {
    let (mut caller, mut board) = Keys::new().exchange();
    let mut segment = seal(&mut caller, TEST_CONTENT);
    segment[0] ^= 1;

    let mut output = [0; TEST_CONTENT.len()];
    assert_eq!(
        board.open(&segment, &mut output),
        Err(HandshakeError::Authentication)
    );
    assert_eq!(
        board.open(&segment, &mut output),
        Err(HandshakeError::Closed)
    );
}

#[test]
fn a_repeated_segment_fails_the_counter_check() {
    let (mut caller, mut board) = Keys::new().exchange();
    let segment = seal(&mut caller, TEST_CONTENT);
    let mut output = [0; TEST_CONTENT.len()];
    board.open(&segment, &mut output).unwrap();

    assert_eq!(
        board.open(&segment, &mut output),
        Err(HandshakeError::Authentication)
    );
}

#[test]
fn a_gap_in_segment_order_fails_the_counter_check() {
    let (mut caller, mut board) = Keys::new().exchange();
    let _dropped = seal(&mut caller, TEST_CONTENT);
    let second = seal(&mut caller, TEST_CONTENT);

    let mut output = [0; TEST_CONTENT.len()];
    assert_eq!(
        board.open(&second, &mut output),
        Err(HandshakeError::Authentication)
    );
}

#[test]
fn a_tag_without_content_is_rejected() {
    let (_caller, mut board) = Keys::new().exchange();
    let tag_only = [0; malak_proto::TAG_BYTES];

    assert_eq!(board.open(&tag_only, &mut []), Err(HandshakeError::Length));
    assert!(board.is_closed());
}

#[test]
fn a_full_protected_segment_round_trips() {
    let (mut caller, mut board) = Keys::new().exchange();
    let content = vec![42; malak_proto::MAX_CONTENT];
    let segment = seal(&mut caller, &content);

    let mut output = vec![0; content.len()];
    assert_eq!(segment.len(), u16::MAX as usize);
    assert_eq!(board.open(&segment, &mut output), Ok(content.len()));
    assert_eq!(output, content);
}
