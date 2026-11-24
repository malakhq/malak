//! Journal replay distinguishes a torn tail from damage to committed entries.

use malak_journal::{Checkpoint, Cursor, Error, Loaded, Replay, Tail};

fn append(cursor: &mut Cursor, data: &[u8]) -> Vec<u8> {
    let prepared = cursor.prepare(data).unwrap();
    let bytes = prepared.bytes().to_vec();
    prepared.written();
    bytes
}

fn load(bytes: &[u8]) -> Result<(Loaded, Vec<Vec<u8>>), Error> {
    let mut replay = Replay::new(Checkpoint::EMPTY);
    let mut payloads = Vec::new();
    replay.file(1, bytes, |_, payload| payloads.push(payload.to_vec()))?;
    Ok((replay.finish()?, payloads))
}

#[test]
fn replay_preserves_payload_bytes_and_running_hash() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let payload = b"opaque\0data\0\0";
    let bytes = append(&mut cursor, payload);

    let (loaded, payloads) = load(&bytes).unwrap();
    assert_eq!(loaded.head, cursor.head());
    assert_eq!(payloads, [payload.to_vec()]);
}

#[test]
fn a_large_zero_filled_entry_does_not_break_the_following_entry() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let payload = [0u8; 600];
    let mut bytes = append(&mut cursor, &payload);
    cursor.synced(1).unwrap();
    bytes.extend(append(&mut cursor, b"next"));

    let (loaded, payloads) = load(&bytes).unwrap();

    assert_eq!(loaded.head, cursor.head());
    assert_eq!(payloads, [payload.to_vec(), b"next".to_vec()]);
}

#[test]
fn a_write_does_not_advance_the_synced_watermark() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    append(&mut cursor, b"new");
    assert_eq!(cursor.synced_through(), 0);
}

#[test]
fn completed_sync_advances_only_through_its_captured_sequence() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    append(&mut cursor, b"before sync");
    let covered = cursor.head().sequence;
    append(&mut cursor, b"after capture");

    cursor.synced(covered).unwrap();

    assert_eq!(cursor.synced_through(), covered);
}

#[test]
fn a_failed_write_does_not_advance_the_head() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    {
        let _pending = cursor.prepare(b"write failed").unwrap();
    }
    assert_eq!(cursor.head(), Checkpoint::EMPTY);
}

#[test]
fn entry_cannot_vouch_for_its_own_sync() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let mut bytes = append(&mut cursor, b"this entry will be torn");
    cursor.synced(1).unwrap();

    bytes.truncate(bytes.len() - 1);

    assert_eq!(
        load(&bytes).unwrap().0.tail,
        Some(Tail {
            file: 1,
            offset: 0,
            sequence: 1
        })
    );
}

#[test]
fn every_torn_write_preserves_the_preceding_prefix() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let mut bytes = append(&mut cursor, b"written");
    let safe = cursor.head();
    let offset = bytes.len();
    let after = append(&mut cursor, b"may be torn");
    bytes.extend_from_slice(&after);

    for written in 1..after.len() {
        let (loaded, _) = load(&bytes[..offset + written]).unwrap();
        assert_eq!(loaded.head, safe, "cut after {written} bytes");
        assert_eq!(
            loaded.tail,
            Some(Tail {
                file: 1,
                offset,
                sequence: 2
            })
        );
    }
}

#[test]
fn a_later_entry_proves_damage_even_if_the_break_lost_its_delimiter() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let mut bytes = append(&mut cursor, b"this entry will be damaged");
    cursor.synced(1).unwrap();
    bytes.truncate(bytes.len() - 12);
    bytes.extend(append(&mut cursor, b"vouches for entry one"));

    assert_eq!(load(&bytes), Err(Error::Damaged { sequence: 1 }));
}

#[test]
fn an_unsynced_break_does_not_adopt_later_entries() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let mut bytes = append(&mut cursor, b"torn before sync");
    bytes.truncate(bytes.len() - 12);
    bytes.extend(append(&mut cursor, b"does not vouch for first"));

    let (loaded, payloads) = load(&bytes).unwrap();
    assert_eq!(loaded.head, Checkpoint::EMPTY);
    assert!(payloads.is_empty());
}

#[test]
fn a_different_running_hash_breaks_replay() {
    let mut original = Cursor::from_synced(Checkpoint::EMPTY);
    let mut bytes = append(&mut original, b"branch a");

    let mut other = Cursor::from_synced(Checkpoint::EMPTY);
    append(&mut other, b"branch b");
    bytes.extend(append(&mut other, b"another branch b event"));

    let (loaded, _) = load(&bytes).unwrap();
    assert_eq!(loaded.head, original.head());
    assert_eq!(loaded.tail.unwrap().sequence, 2);
}

#[test]
fn missing_file_is_damage_if_a_later_file_witnessed_its_sync() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    let first = append(&mut cursor, b"one");
    append(&mut cursor, b"missing file");
    cursor.synced(2).unwrap();
    let third = append(&mut cursor, b"three");

    let mut replay = Replay::new(Checkpoint::EMPTY);
    replay.file(1, &first, |_, _| {}).unwrap();
    assert_eq!(
        replay.file(3, &third, |_, _| {}),
        Err(Error::Damaged { sequence: 2 })
    );
    assert_eq!(replay.finish(), Err(Error::Damaged { sequence: 2 }));
}

#[test]
fn snapshot_replay_ignores_covered_files_brought_back_by_a_crash() {
    let mut cursor = Cursor::from_synced(Checkpoint::EMPTY);
    append(&mut cursor, b"in snapshot");
    let snapshot = cursor.head();
    cursor.synced(snapshot.sequence).unwrap();
    let next = append(&mut cursor, b"after snapshot");

    let mut replay = Replay::new(snapshot);
    replay
        .file(1, b"damaged old bytes", |_, _| {
            panic!("old file was replayed")
        })
        .unwrap();
    replay
        .file(2, &next, |_, payload| {
            assert_eq!(payload, b"after snapshot")
        })
        .unwrap();
    assert_eq!(replay.finish().unwrap().head, cursor.head());
}
