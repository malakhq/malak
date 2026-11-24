use super::*;
use crate::journal_entry::{Change, StartupEntry, TailCut};
use crate::{Outcome, witness};
use malak_journal::{Checkpoint, StateFolder};
use malak_record::{RecordKey, store_name};
use malak_testkit::tasks;
use std::fs;

const WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug)]
enum Misplaced {
    EarlierCreation,
    AnotherChannel,
    AnotherPosition,
    AnotherKey,
    Witness,
}

#[test]
fn misplaced_and_foreign_records_are_neither_adopted_nor_delivered() {
    tasks::spawn(|| {
        for misplaced in [
            Misplaced::EarlierCreation,
            Misplaced::AnotherChannel,
            Misplaced::AnotherPosition,
            Misplaced::AnotherKey,
            Misplaced::Witness,
        ] {
            assert_never_delivered(misplaced);
        }
    })
    .wait(WAIT)
    .expect("record verification matrix timed out");
}

fn assert_never_delivered(misplaced: Misplaced) {
    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    let (board, entrances) = start(root, true, false);
    let (earlier, _) = board.create_channel("history").unwrap();
    board.remove_channel("history", earlier).unwrap();
    let lead = board.lead();
    let first = lead
        .send(SendRequest::note("history", "first", b"retained"))
        .unwrap();
    let carrier = lead
        .send(SendRequest::note("carrier", "displaced", b"misplaced"))
        .unwrap();
    drop(lead);
    stop(board, entrances);
    let name = store_name(&"history".parse().unwrap(), first.creation);
    let carrier_name = store_name(&"carrier".parse().unwrap(), carrier.creation);
    let old = store_name(&"history".parse().unwrap(), earlier);
    let (key, checkpoint) = journal_evidence(root);
    let (store, generation) = claim_records(root, true);
    let kept = read_records(&store, &name).remove(0);
    let fields_source = read_records(&store, &carrier_name).remove(0);
    let fields = key.verify(&carrier_name, &fields_source).unwrap();
    let second = Position::try_from(2).unwrap();
    let seal = |key: &RecordKey, channel: &str, position| {
        key.seal(
            channel,
            position,
            fields.stamp,
            fields.metadata,
            fields.body,
        )
        .unwrap()
    };
    let wrong = match misplaced {
        Misplaced::EarlierCreation => seal(&key, &old, second),
        Misplaced::AnotherChannel => seal(&key, &carrier_name, second),
        Misplaced::AnotherPosition => Record {
            position: second,
            ..seal(&key, &name, Position::FIRST)
        },
        Misplaced::AnotherKey => seal(&RecordKey::new(&[3; 32]), &name, second),
        Misplaced::Witness => Record {
            position: second,
            data: witness::encode(&key, checkpoint),
        },
    };
    replace_records(&store, generation, &name, &[kept.clone(), wrong.clone()]);
    drop(store);

    let (board, entrances) = start(root, true, false);
    let lead = board.lead();
    assert!(
        matches!(
            lead.get("history", 2).unwrap(),
            GetReply::NeverWritten { .. }
        ),
        "{misplaced:?}"
    );
    let counted = lead
        .send(SendRequest::note("history", "new", b"real second record"))
        .unwrap();
    assert_eq!(counted.position, second);
    drop(lead);
    stop(board, entrances);

    let (store, generation) = claim_records(root, true);
    replace_records(&store, generation, &name, &[kept, wrong]);
    drop(store);

    let (board, entrances) = start(root, true, false);
    assert_unavailable_to_readers(&board, 2);
    stop(board, entrances);
}

#[test]
fn a_record_that_was_cut_cannot_displace_the_counted_record() {
    tasks::spawn(exercise_cut_record)
        .wait(WAIT)
        .expect("cut record scenario timed out");
}

fn exercise_cut_record() {
    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    let (board, entrances) = start(root, true, false);
    let first = board
        .lead()
        .send(SendRequest::note("history", "retained", b"first retained"))
        .unwrap();
    stop(board, entrances);

    kill(root, true, "send", "append-after");
    let name = store_name(&"history".parse().unwrap(), first.creation);
    let (store, generation) = claim_records(root, true);
    let [first, abandoned]: [Record; 2] = read_records(&store, &name).try_into().unwrap();
    replace_records(&store, generation, &name, std::slice::from_ref(&first));
    drop(store);

    // Losing this unreported append causes the restart to save a cut. Capture
    // the old whole bytes above, then put them back under the raised attempt.
    let (board, entrances) = start(root, true, false);
    assert!(matches!(
        board.lead().get("history", 2).unwrap(),
        GetReply::NeverWritten { .. }
    ));
    stop(board, entrances);
    let (store, generation) = claim_records(root, true);
    store
        .append(generation, &name, std::slice::from_ref(&abandoned))
        .unwrap();
    drop(store);

    let (board, entrances) = start(root, true, false);
    let lead = board.lead();
    assert!(matches!(
        lead.get("history", 2).unwrap(),
        GetReply::NeverWritten { .. }
    ));
    let counted = lead
        .send(SendRequest::note("history", "current", b"counted here"))
        .unwrap();
    assert_eq!(counted.position.get(), 2);
    lead.send(SendRequest::note("history", "after", b"third retained"))
        .unwrap();
    drop(lead);
    stop(board, entrances);
    assert_eq!(
        last_tail_report(root),
        TailCut {
            last: Some(counted.position),
            unparseable: false,
            tampered: true
        }
    );

    let (store, generation) = claim_records(root, true);
    let third = read_records(&store, &name).pop().unwrap();
    replace_records(&store, generation, &name, &[first, abandoned, third]);
    drop(store);
    let (board, entrances) = start(root, true, false);
    assert_unavailable_to_readers(&board, 2);
    stop(board, entrances);

    let journal = journal_evidence(root).1;
    let data = channel_files(root);
    let report = Owner::look_recovery(
        MarkdownStore::new(root.join("store"), true),
        Limits::default(),
        root.join("state"),
        Some("key"),
        64,
    )
    .unwrap()
    .to_string();
    assert!(report.contains("Keep [1..2)"));
    assert!(report.contains("Give up [2..3) (wrong stamp)"));
    assert!(report.contains("Keep [3..4)"));
    assert_eq!(journal_evidence(root).1, journal);
    assert_eq!(channel_files(root), data);
}

#[test]
fn a_torn_tail_is_reported_without_marking_tampering_or_damage() {
    use std::io::Write;

    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    let (board, entrances) = start(root, true, false);
    let receipt = board.lead().send(sample()).unwrap();
    stop(board, entrances);
    let channel = channel_files(root)
        .into_iter()
        .find(|(_, bytes)| {
            bytes
                .windows(sample().body.len())
                .any(|word| word == sample().body)
        })
        .unwrap()
        .0;
    fs::OpenOptions::new()
        .append(true)
        .open(channel)
        .unwrap()
        .write_all(b"interrupted record")
        .unwrap();

    let (board, entrances) = start(root, true, false);
    assert!(
        matches!(board.lead().get("history", 1).unwrap(), GetReply::Message(message) if message.receipt == receipt)
    );
    stop(board, entrances);

    assert_eq!(
        last_tail_report(root),
        TailCut {
            last: None,
            unparseable: true,
            tampered: false
        }
    );
}

fn last_tail_report(root: &Path) -> TailCut {
    let journal = StateFolder::acquire_existing(root.join("state"))
        .unwrap()
        .inspect()
        .unwrap();
    journal
        .accepted_entries()
        .unwrap()
        .filter_map(|(_, bytes)| match Change::decode(bytes) {
            Ok(Change::Cut(_, _, _, tail)) => tail,
            _ => None,
        })
        .last()
        .expect("startup cut report should be durable")
}

#[test]
fn a_message_in_the_witness_channel_cannot_block_the_current_state() {
    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    let (board, entrances) = start(root, true, false);
    let receipt = board.lead().send(sample()).unwrap();
    stop(board, entrances);
    let name = store_name(&"history".parse().unwrap(), receipt.creation);
    let (store, generation) = claim_records(root, true);
    let message = read_records(&store, &name).remove(0);
    replace_records(&store, generation, witness::CHANNEL, &[message]);
    drop(store);

    let (board, entrances) = start(root, true, false);
    assert!(
        matches!(board.lead().get("history", 1).unwrap(), GetReply::Message(message) if message.receipt == receipt)
    );
    stop(board, entrances);
}

fn assert_unavailable_to_readers(board: &Owner, position: u64) {
    let lead = board.lead();
    assert_eq!(
        lead.get("history", position).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    assert_eq!(
        lead.read(&["history"], 8, Duration::ZERO)
            .unwrap_err()
            .outcome(),
        Outcome::Unavailable
    );
}

fn stop(board: Owner, entrances: LocalEntrances) {
    board.stop(board.status(None, None).run).unwrap();
    drop((board, entrances));
}

pub(super) fn journal_evidence(root: &Path) -> (RecordKey, Checkpoint) {
    let journal = StateFolder::acquire_existing(root.join("state"))
        .unwrap()
        .inspect()
        .unwrap();
    let (_, first) = journal.accepted_entries().unwrap().next().unwrap();
    let StartupEntry::Keys(keys) = StartupEntry::decode(first).unwrap() else {
        panic!("expected the initial keys");
    };
    (RecordKey::new(&keys.record), journal.loaded().head)
}

fn channel_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = fs::read_dir(root.join("store"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .map(|path| {
            let bytes = fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect::<Vec<_>>();
    files.sort_unstable();
    files
}

#[test]
fn adoption_of_a_pre_release_empty_trace_keeps_a_legacy_receipt() {
    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    let (board, entrances) = start(root, true, false);
    let first = board.lead().send(sample()).unwrap();
    stop(board, entrances);

    // Canonical pre-release fields: task, empty trace, ID "legacy", lead,
    // proven, clock [0,0], synced. The position and stamp come from the board.
    let fields = b"\x88\x00\x81\x82\x65trace\x60\x66legacy\x64lead\x64lead\x01\x82\x00\x00\x02";
    let name = store_name(&"history".parse().unwrap(), first.creation);
    let (key, _) = journal_evidence(root);
    let (store, generation) = claim_records(root, true);
    let original = read_records(&store, &name).remove(0);
    let stamp = key.verify(&name, &original).unwrap().stamp;
    let position = Position::try_from(2).unwrap();
    let body = b"adopted";
    let sealed = key.seal(&name, position, stamp, fields, body).unwrap();
    store.append(generation, &name, &[sealed]).unwrap();
    drop(store);

    let (board, entrances) = start(root, true, false);
    let lead = board.lead();
    assert!(matches!(
        lead.get("history", 2).unwrap(),
        GetReply::Message(_)
    ));
    board.trim("history", first.creation, 3).unwrap();
    drop(lead);
    stop(board, entrances);

    // Replay from an archived legacy head so neither lookup can recover the
    // trace or digest by consulting the original message.
    let journal = StateFolder::acquire_existing(root.join("state"))
        .unwrap()
        .inspect()
        .unwrap();
    let archive = journal.archive_payload().unwrap();
    journal.activate().unwrap().compact(&archive).unwrap();
    let (board, entrances) = start(root, true, false);
    let lead = board.lead();
    let request = SendRequest {
        channel: "history",
        id: "legacy",
        kind: crate::Kind::Task,
        headers: &[("trace", crate::HeaderValue::Text(""))],
        body,
        minimum_durability: None,
    };
    let expected = crate::Receipt {
        position,
        creation: first.creation,
        durability: crate::Durability::Synced,
        duplicate: true,
        trace: None,
    };
    assert_eq!(lead.send(request).unwrap(), expected);
    assert_eq!(
        lead.send(SendRequest {
            headers: &[],
            ..request
        })
        .unwrap(),
        expected
    );
    assert_eq!(
        lead.send(SendRequest {
            body: b"changed",
            ..request
        })
        .unwrap_err()
        .outcome(),
        Outcome::Refused
    );
    drop(lead);
    stop(board, entrances);
}
