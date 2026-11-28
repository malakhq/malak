//! Seed retained pre-decision-152 data without relaxing the board's write path.

use malak_core::{ChannelName, CreationId, Position};
use malak_journal::StateFolder;
use malak_layers::{Claim, Claimed, ReadLimit, ReadResult, Store, Trim};
use malak_record::{RecordKey, store_name};
use malak_store_markdown::MarkdownStore;
use minicbor::{Decoder, Encoder};
use std::path::Path;

/// Relabel one stopped fixture channel as an old stray. Store records keep
/// their creation, position, fields and body, with checks for the legacy name.
pub(super) fn channel(state: &Path, from: &str, to: &str, creation: &str) {
    let creation: CreationId = creation.parse().unwrap();
    let old: ChannelName = from.parse().unwrap();
    let legacy: ChannelName = to.parse().unwrap();
    let old_store = store_name(&old, creation);
    let legacy_store = store_name(&legacy, creation);
    let inspected = StateFolder::acquire_existing(state)
        .unwrap()
        .inspect()
        .unwrap();

    // A replay archive keeps the historical checkpoints used by witnesses.
    // Its application payload is seeded here as the old board's channel state.
    let mut archive = b"malak-journal-archive\x01".to_vec();
    let mut record_key = None;
    let mut changed = 0;
    for (at, payload) in inspected.accepted_entries().unwrap() {
        let mut input = Decoder::new(payload);
        input.array().unwrap();
        assert_eq!(input.u8().unwrap(), 1);
        let tag = input.u8().unwrap();
        let mut replacement = None;
        if tag == 0 {
            record_key = Some(<[u8; 32]>::try_from(input.bytes().unwrap()).unwrap());
        } else if tag == 7 && input.str().unwrap() == from {
            assert_eq!(input.bytes().unwrap(), creation.as_bytes());
            let creator = input.str().unwrap();
            let mut output = Encoder::new(Vec::new());
            output
                .array(5)
                .unwrap()
                .u8(1)
                .unwrap()
                .u8(7)
                .unwrap()
                .str(to)
                .unwrap()
                .bytes(creation.as_bytes())
                .unwrap()
                .str(creator)
                .unwrap();
            replacement = Some(output.into_writer());
            changed += 1;
        }
        let payload = replacement.as_deref().unwrap_or(payload);
        archive.extend_from_slice(&at.running_hash);
        archive.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        archive.extend_from_slice(payload);
    }
    assert_eq!(changed, 1, "seed exactly one legacy channel");
    let key = RecordKey::new(&record_key.expect("fixture record key"));

    let store = MarkdownStore::new(state.join("messages"), false);
    let Claimed::Taken(owned) = store.claim(Claim::Take).unwrap() else {
        panic!("claim the stopped fixture store");
    };
    let ReadResult::Records {
        records,
        unparseable: false,
    } = store
        .read(
            &old_store,
            Position::try_from(1).unwrap(),
            ReadLimit {
                records: 1,
                before: None,
            },
        )
        .unwrap()
    else {
        panic!("read the fixture record");
    };
    assert_eq!(records.len(), 1);
    let original = &records[0];
    let fields = key.verify(&old_store, original).unwrap();
    let retained = key
        .seal(
            &legacy_store,
            original.position,
            fields.stamp,
            fields.metadata,
            fields.body,
        )
        .unwrap();
    assert_eq!(retained.data.len(), original.data.len());
    store
        .append(owned.generation, &legacy_store, &[retained])
        .unwrap();
    store
        .trim(owned.generation, &old_store, Trim::Keep(1..1))
        .unwrap();
    store
        .claim(Claim::Settle {
            generation: owned.generation,
            channel: &legacy_store,
        })
        .unwrap();
    store
        .claim(Claim::Release {
            generation: owned.generation,
        })
        .unwrap();
    inspected.activate().unwrap().compact(&archive).unwrap();
}
