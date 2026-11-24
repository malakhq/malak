//! Independent authentication of stored records and journal witnesses.

use malak_layers::{CreationId, Position, Record};
use malak_record::{History, RecordKey, Stamp, store_name};

const STAMP: Stamp = Stamp {
    run: [1; 16],
    attempt: 0,
};
const NEXT_ATTEMPT: Stamp = Stamp {
    attempt: 1,
    ..STAMP
};
const METADATA: &[u8] = b"board message fields and receipt";
const BODY: &[u8] = b"opaque\x00\xffbody";
const POSITION: Position = Position::FIRST;

struct Fixture {
    key: RecordKey,
    channel: String,
    record: Record,
}

impl Fixture {
    fn new() -> Self {
        let key = RecordKey::new(&[0x42; 32]);
        let channel = qualified("work", 0xab);
        let record = key.seal(&channel, POSITION, STAMP, METADATA, BODY).unwrap();
        Self {
            key,
            channel,
            record,
        }
    }

    fn history<'a>(&'a self, stamps: &'a [(Position, Stamp)]) -> History<'a> {
        History {
            channel: &self.channel,
            retained: 1..2,
            stamps,
            voids: &[],
        }
    }

    fn change_byte(&mut self, offset: usize) {
        let mut data = self.record.data.to_vec();
        data[offset] ^= 1;
        self.record.data = data.into();
    }
}

fn qualified(channel: &str, creation: u8) -> String {
    store_name(
        &channel.parse().unwrap(),
        CreationId::from_bytes([creation; 16]),
    )
}

#[test]
fn current_record_verifies_without_neighbouring_records() {
    let sample = Fixture::new();
    let stamps = [(POSITION, STAMP)];
    let verified = sample
        .key
        .current(&sample.history(&stamps), &sample.record)
        .unwrap();
    assert_eq!(verified.metadata, METADATA);
    assert_eq!(verified.body, BODY);
    assert_eq!(verified.stamp, STAMP);
}

#[test]
fn record_from_an_earlier_creation_does_not_verify() {
    let sample = Fixture::new();
    let recreated = qualified("work", 0xac);
    assert!(sample.key.verify(&recreated, &sample.record).is_none());
}

#[test]
fn record_moved_to_another_channel_does_not_verify() {
    let sample = Fixture::new();
    let other = qualified("personal", 0xab);
    assert!(sample.key.verify(&other, &sample.record).is_none());
}

#[test]
fn record_moved_to_another_position_does_not_verify() {
    let mut sample = Fixture::new();
    sample.record.position = Position::try_from(2).unwrap();
    assert!(sample.key.verify(&sample.channel, &sample.record).is_none());
}

#[test]
fn record_signed_with_another_key_does_not_verify() {
    let sample = Fixture::new();
    assert!(
        RecordKey::new(&[0x43; 32])
            .verify(&sample.channel, &sample.record)
            .is_none()
    );
}

#[test]
fn changing_the_opaque_body_invalidates_the_check() {
    let mut sample = Fixture::new();
    sample.change_byte(33 + METADATA.len());
    assert!(sample.key.verify(&sample.channel, &sample.record).is_none());
}

#[test]
fn changing_message_fields_invalidates_the_check() {
    let mut sample = Fixture::new();
    sample.change_byte(33);
    assert!(sample.key.verify(&sample.channel, &sample.record).is_none());
}

#[test]
fn incomplete_record_does_not_verify() {
    let mut sample = Fixture::new();
    sample.record.data.truncate(sample.record.data.len() - 1);
    assert!(sample.key.verify(&sample.channel, &sample.record).is_none());
}

#[test]
fn witness_with_an_edited_sequence_does_not_verify() {
    let sample = Fixture::new();
    let hash = [0x55; 32];
    let check = sample.key.witness_check(42, &hash);
    assert!(sample.key.verify_witness(42, &hash, &check));
    assert!(!sample.key.verify_witness(43, &hash, &check));
}

#[test]
fn message_check_cannot_authenticate_a_witness() {
    let sample = Fixture::new();
    let data = &sample.record.data;
    assert!(
        !sample
            .key
            .verify_witness(42, &[0x55; 32], &data[data.len() - 32..])
    );
}

#[test]
fn witness_check_cannot_authenticate_a_message() {
    let mut sample = Fixture::new();
    let mut data = sample.record.data.to_vec();
    let start = data.len() - 32;
    data[start..].copy_from_slice(&sample.key.witness_check(42, &[0x55; 32]));
    sample.record.data = data.into();
    assert!(sample.key.verify(&sample.channel, &sample.record).is_none());
}

#[test]
fn a_valid_record_from_the_wrong_stamp_is_not_current() {
    let sample = Fixture::new();
    let stamps = [(POSITION, NEXT_ATTEMPT)];
    assert!(
        sample
            .key
            .current(&sample.history(&stamps), &sample.record)
            .is_none()
    );
}

#[test]
fn a_record_at_a_void_position_is_not_current() {
    let sample = Fixture::new();
    let stamps = [(POSITION, STAMP)];
    let mut history = sample.history(&stamps);
    let void = 1..2;
    history.voids = std::slice::from_ref(&void);
    assert!(sample.key.current(&history, &sample.record).is_none());
}

#[test]
fn a_record_below_the_floor_is_not_current() {
    let sample = Fixture::new();
    let stamps = [(POSITION, STAMP)];
    let mut history = sample.history(&stamps);
    history.retained = 2..2;
    assert!(sample.key.current(&history, &sample.record).is_none());
}

#[test]
fn a_record_at_the_head_is_not_current() {
    let sample = Fixture::new();
    let stamps = [(POSITION, STAMP)];
    let mut history = sample.history(&stamps);
    history.retained = 1..1;
    assert!(sample.key.current(&history, &sample.record).is_none());
}

#[test]
fn reusing_the_body_digest_preserves_the_record_format() {
    let sample = Fixture::new();
    let body = malak_record::HashedBody::new(BODY);

    let sealed = sample
        .key
        .seal_hashed(&sample.channel, POSITION, STAMP, METADATA, &body)
        .unwrap();

    assert_eq!(sealed, sample.record);
}
