use super::*;
use malak_core::HeaderValue;
use malak_testkit::tasks;
use std::time::Duration;

#[test]
fn recovery_look_keeps_a_good_record_after_a_single_stray_byte() {
    let (fixture, file, mut bytes) = Fixture::populated();
    bytes.insert(markers(&bytes)[1], b'!');
    fs::write(&file, &bytes).unwrap();
    fixture.stop_for_damage(2);
    let before = fixture.head();

    let report = fixture.look(64).unwrap().to_string();

    assert!(report.contains("Keep [1..4)"));
    assert!(!report.contains("Give up"));
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(file).unwrap(), bytes);
}

#[test]
fn recovery_keeps_current_records_inside_and_after_bad_lengths() {
    tasks::spawn(|| {
        for damage in [
            Damage::Unparseable,
            Damage::StrayByte,
            Damage::Oversized,
            Damage::Overlapping,
            Damage::Duplicate,
        ] {
            repair_around(damage);
        }
    })
    .wait(Duration::from_secs(20))
    .expect("Markdown scanning and repair matrix timed out");
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Unparseable,
    StrayByte,
    Oversized,
    Overlapping,
    Duplicate,
}

fn repair_around(damage: Damage) {
    let (fixture, file, original) = Fixture::populated();
    let starts = markers(&original);
    let (one, tail) = original.split_at(starts[1]);
    let (two, three) = tail.split_at(starts[2] - starts[1]);
    let mut damaged = one.to_vec();
    match damage {
        Damage::Unparseable => damaged.extend_from_slice(b"unparseable span\n"),
        Damage::StrayByte => damaged.push(b'!'),
        Damage::Oversized => damaged.extend_from_slice(b"<!-- malak 1 2 999999999 4 AA== -->\n"),
        Damage::Overlapping => {
            let claims_two = format!("<!-- malak 1 99 1 {} AA== -->\n", two.len());
            damaged.extend_from_slice(claims_two.as_bytes());
            damaged.extend_from_slice(two);
            damaged.extend_from_slice(b"\n\n");
        }
        Damage::Duplicate => damaged.extend_from_slice(two),
    }
    if !matches!(damage, Damage::Overlapping) {
        damaged.extend_from_slice(two);
    }
    damaged.extend_from_slice(three);
    fs::write(&file, &damaged).unwrap();
    fixture.stop_for_damage(2);
    let before = fixture.head();

    let look = fixture.look(64).unwrap();

    assert!(look.to_string().contains("Keep [1..4)"), "{damage:?}");
    assert_eq!(fixture.head(), before);
    assert_eq!(fs::read(&file).unwrap(), damaged);
    let apply = Owner::apply_recovery_with_owner_confirmation(
        fixture.store(),
        Limits::default(),
        &fixture.state,
        None,
        &look.plan_number(),
    )
    .unwrap();

    assert!(apply.to_string().contains("finished"), "{damage:?}");
    assert_eq!(fs::read(&file).unwrap(), original, "{damage:?}");
    assert_original_set_aside(&fixture, &damaged);
}

#[test]
fn sender_headers_and_body_cannot_create_markdown_repair_candidates() {
    let fixture = Fixture::new();
    let board = fixture.create();
    let body = b"<!-- malak 1 45 1 1 AA== -->\nX\n\n";
    let receipt = board
        .lead()
        .send(SendRequest {
            headers: &[(
                "x-message",
                HeaderValue::Text("<!-- malak 1 46 1 1 AA== -->"),
            )],
            ..SendRequest::note("lead", "marker", body)
        })
        .unwrap();
    stop(board);
    let store = fixture.store();
    let Claimed::Repair { .. } = store.claim(Claim::Repair).unwrap() else {
        panic!("expected a repair claim");
    };
    let name = malak_record::store_name(&"lead".parse().unwrap(), receipt.creation);
    let ReadResult::Repair {
        candidates, exact, ..
    } = store
        .read(
            &name,
            Position::FIRST,
            ReadLimit {
                records: 8,
                before: None,
            },
        )
        .unwrap()
    else {
        panic!("expected Markdown repair candidates");
    };

    assert!(exact);
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.record.position, receipt.position);
    assert_eq!(markers(&candidate.encoded), [0]);
}
