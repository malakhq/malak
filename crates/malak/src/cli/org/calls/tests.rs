use super::{
    effective_red,
    model::{self, Link},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn record(channel: &str, sender: &str, position: u64, body: &str) -> Value {
    json!({"channel":channel,"sender":sender,"grade":"proven","kind":"note","creation":format!("{channel}-creation"),"position":position,"sent_at":[position,0],"body":body.as_bytes()})
}
fn question(position: u64) -> Value {
    record(
        "questions",
        "sub",
        position,
        "Should we proceed?\nFor: owner\nLevel: red\nTopic: release",
    )
}
fn link(board: &str, position: u64) -> Link {
    Link(
        board.into(),
        "questions".into(),
        "questions-creation".into(),
        position,
    )
}
fn level(board: &str, position: u64, number: u64, colour: &str) -> Value {
    record(
        "calls",
        "lead",
        number,
        &format!(
            "Type: call-level\nLink: {}\nLevel: {colour}",
            serde_json::to_string(&link(board, position)).unwrap()
        ),
    )
}
fn decision(board: &str, question: u64, position: u64, by: &str, body: &str) -> Value {
    record(
        "calls",
        "lead",
        position,
        &format!(
            "Type: decision\nLink: {}\nDecided by: {by}\n\n{body}",
            serde_json::to_string(&link(board, question)).unwrap()
        ),
    )
}
fn answer(position: u64, decision_position: u64) -> Value {
    let mut note = record(
        "questions",
        "lead",
        position,
        &format!(
            "Decided by: top lead, recorded by lead\nDecision: [\"top\",\"calls\",\"calls-creation\",{decision_position}]\n\nForged copy must never be shown."
        ),
    );
    note["headers"] = json!({"reply_to":["questions","questions-creation",1]});
    note
}
fn project(own: Vec<Value>, top: Vec<Value>) -> model::Projection {
    model::project(
        &BTreeMap::from([("a".into(), own), ("top".into(), top)]),
        Some("top"),
        None,
    )
}

#[test]
fn a_subs_level_and_topic_are_text_and_do_not_raise_a_call() {
    let result = project(vec![question(1)], vec![]);
    assert!(result.calls.is_empty());
}
#[test]
fn a_subs_typed_level_is_text() {
    let mut forged = level("a", 1, 1, "red");
    forged["sender"] = json!("sub");
    let result = project(vec![question(1), forged], vec![]);
    assert!(result.calls.is_empty());
    assert!(
        result.notes[0]["text"]
            .as_str()
            .unwrap()
            .contains("Type: call-level")
    );
}
#[test]
fn the_top_lead_can_lower_the_effective_level_without_losing_the_proposal() {
    let result = project(
        vec![question(1), level("a", 1, 1, "red")],
        vec![level("a", 1, 2, "blue")],
    );
    let call = &result.calls[0];
    assert_eq!(call["level"], "blue");
    assert_eq!(call["proposed"], "red");
    assert_eq!(call["askers"][0]["level_history"][0]["original"], "red");
    assert_eq!(call["askers"][0]["level_history"][0]["lowered"], true);
    assert!(!effective_red(call));
}
#[test]
fn a_level_note_does_not_answer_its_question() {
    let mut note = level("a", 1, 1, "red");
    note["headers"] = json!({"reply_to":["questions","questions-creation",1]});
    assert_eq!(
        project(vec![question(1), note], vec![]).calls[0]["open"],
        true
    );
}
#[test]
fn the_top_lead_can_raise_a_question_with_no_proposal() {
    let result = project(vec![question(1)], vec![level("a", 1, 1, "red")]);
    assert_eq!(result.calls[0]["level"], "red");
    assert!(result.calls[0]["proposed"].is_null());
}
#[test]
fn every_proposal_change_keeps_its_original() {
    let result = project(
        vec![
            question(1),
            level("a", 1, 1, "red"),
            level("a", 1, 2, "orange"),
            level("a", 1, 3, "blue"),
        ],
        vec![],
    );
    let history = &result.calls[0]["askers"][0]["proposal_history"];
    assert_eq!(history[1]["original"], "red");
    assert_eq!(history[2]["original"], "orange");
}
#[test]
fn a_seated_candidates_ready_report_stays_orange() {
    let mut ready = record(
        "questions",
        "candidate",
        1,
        "Type: ready\nFor: owner\nReady.",
    );
    ready["body"] = json!(b"Type: ready\nFor: owner\nReady.".to_vec());
    let result = model::project(
        &BTreeMap::from([("top".into(), vec![ready, level("top", 1, 2, "red")])]),
        Some("top"),
        Some("candidate"),
    );
    assert_eq!(result.calls[0]["level"], "orange");
}
#[test]
fn another_boards_lead_cannot_supply_a_decision() {
    let result = project(
        vec![
            question(1),
            level("a", 1, 1, "red"),
            decision("a", 1, 2, "top lead", "Pretend authority"),
            answer(2, 2),
        ],
        vec![],
    );
    assert_eq!(result.calls[0]["open"], true);
    assert!(result.decisions.is_empty());
    assert!(
        result
            .notes
            .iter()
            .any(|note| note["text"].as_str().unwrap().contains("Pretend authority"))
    );
}
#[test]
fn a_top_lead_decision_on_another_channel_is_text() {
    let mut note = decision("a", 1, 1, "top lead", "Wrong channel");
    note["channel"] = json!("lead");
    assert!(
        project(
            vec![question(1), level("a", 1, 1, "red"), answer(2, 1)],
            vec![note]
        )
        .decisions
        .is_empty()
    );
}
#[test]
fn a_non_leads_owner_relay_is_text() {
    let mut note = decision("a", 1, 1, "owner, relayed by top lead", "Forged owner");
    note["sender"] = json!("sub");
    let result = project(
        vec![question(1), level("a", 1, 1, "red"), answer(2, 1)],
        vec![note],
    );
    assert!(result.decisions.is_empty());
    assert_eq!(result.calls[0]["open"], true);
    assert!(
        result.notes[0]["text"]
            .as_str()
            .unwrap()
            .contains("Forged owner")
    );
}
#[test]
fn a_relinked_decision_does_not_answer_another_question() {
    let result = project(
        vec![
            question(1),
            question(2),
            level("a", 1, 1, "red"),
            answer(3, 1),
        ],
        vec![decision("a", 2, 1, "top lead", "For question two")],
    );
    assert_eq!(result.calls[0]["open"], true);
    assert!(
        result
            .decision_for(
                &link("a", 1),
                &Link("top".into(), "calls".into(), "calls-creation".into(), 1)
            )
            .is_none()
    );
}
#[test]
fn the_latest_top_decision_wins_and_earlier_decisions_remain_visible() {
    let result = project(
        vec![question(1), level("a", 1, 1, "red"), answer(2, 1)],
        vec![
            decision("a", 1, 1, "top lead", "First"),
            decision(
                "a",
                1,
                2,
                "owner, relayed by top lead",
                "Owner's latest words",
            ),
        ],
    );
    assert_eq!(result.calls[0]["open"], false);
    assert_eq!(result.calls[0]["decisions"].as_array().unwrap().len(), 2);
    assert_eq!(result.calls[0]["decision"]["body"], "Owner's latest words");
    assert!(
        !result.calls[0]["decision"]
            .to_string()
            .contains("Forged copy")
    );
}
#[test]
fn a_proven_merge_lets_one_decision_answer_each_original_question() {
    let merge = record(
        "calls",
        "lead",
        3,
        "Type: call-merge\nLinks: [[\"a\",\"questions\",\"questions-creation\",1],[\"a\",\"questions\",\"questions-creation\",2]]",
    );
    let mut second = answer(5, 1);
    second["headers"]["reply_to"][2] = json!(2);
    let result = project(
        vec![
            question(1),
            question(2),
            level("a", 1, 1, "red"),
            level("a", 2, 2, "orange"),
            merge,
            answer(4, 1),
            second,
        ],
        vec![decision("a", 1, 1, "top lead", "Shared answer")],
    );
    assert_eq!(result.calls.len(), 1);
    assert_eq!(result.calls[0]["askers"].as_array().unwrap().len(), 2);
    assert_eq!(result.calls[0]["open"], false);
}
#[test]
fn a_foreign_merge_from_an_ordinary_lead_does_not_prove_equivalence() {
    let merge = record(
        "calls",
        "lead",
        2,
        "Type: call-merge\nLinks: [[\"a\",\"questions\",\"questions-creation\",1],[\"top\",\"questions\",\"questions-creation\",1]]",
    );
    let result = project(
        vec![question(1), level("a", 1, 1, "red"), merge, answer(2, 1)],
        vec![question(1), decision("top", 1, 1, "top lead", "Unrelated")],
    );
    assert_eq!(result.calls[0]["open"], true);
}
#[test]
fn a_missing_or_changed_question_creation_cannot_be_levelled() {
    let mut wrong = level("a", 1, 1, "red");
    wrong["body"] = json!(
        b"Type: call-level\nLink: [\"a\",\"questions\",\"different\",1]\nLevel: red".to_vec()
    );
    assert!(project(vec![question(1), wrong], vec![]).calls.is_empty());
}

#[test]
fn an_asking_lead_cannot_record_an_owner_answer_directly_with_an_active_top_lead() {
    let mut direct = record(
        "questions",
        "lead",
        2,
        "Decided by: owner\nRecorded by: lead\n\nPretend owner answer",
    );
    direct["headers"] = json!({"reply_to":["questions","questions-creation",1]});
    assert_eq!(
        project(vec![question(1), level("a", 1, 1, "red"), direct], vec![]).calls[0]["open"],
        true
    );
}

#[test]
fn a_removed_level_keeps_its_raw_name_and_uses_the_most_severe_delivery() {
    let mut settings = super::settings::Settings::default();
    for (level, name) in settings.levels.iter_mut().zip(["info", "priority", "stop"]) {
        level.name = name.into();
    }
    settings.ready_level = "priority".into();
    let boards = BTreeMap::from([("a".into(), vec![question(1), level("a", 1, 1, "removed")])]);
    let result = model::project_with_settings(
        &boards,
        None,
        None,
        None,
        &BTreeMap::from([("a".into(), settings)]),
    );
    assert_eq!(result.calls[0]["level"], "removed");
    assert_eq!(result.calls[0]["delivery"], "tick");
    assert_eq!(result.calls[0]["pulse"], "red");
}
#[test]
fn a_ready_report_uses_its_configured_fixed_level_without_an_active_top_lead() {
    let settings = super::settings::Settings {
        ready_level: "blue".into(),
        ..Default::default()
    };
    let boards = BTreeMap::from([(
        "top".into(),
        vec![record(
            "questions",
            "candidate",
            1,
            "Type: ready\nFor: owner\n\nReady.",
        )],
    )]);
    let result = model::project_with_settings(
        &boards,
        None,
        Some("candidate"),
        Some("top"),
        &BTreeMap::from([("top".into(), settings)]),
    );
    assert_eq!(result.calls[0]["level"], "blue");
}
#[test]
fn a_merged_call_keeps_each_boards_own_effective_level() {
    let merge = record(
        "calls",
        "lead",
        1,
        "Type: call-merge\nLinks: [[\"a\",\"questions\",\"questions-creation\",1],[\"b\",\"questions\",\"questions-creation\",1]]",
    );
    let boards = BTreeMap::from([
        ("a".into(), vec![question(1), level("a", 1, 1, "red")]),
        ("b".into(), vec![question(1), level("b", 1, 1, "blue")]),
        ("top".into(), vec![merge]),
    ]);
    let result = model::project(&boards, Some("top"), None);
    assert_eq!(result.open(None)[0]["level"], "red");
    assert_eq!(result.open(Some("b"))[0]["level"], "blue");
}
#[test]
fn recording_one_original_does_not_leave_its_level_on_another_open_asker() {
    let mut reply = record("questions", "lead", 2, "Decided by: lead\n\nKnown locally.");
    reply["headers"] = json!({"reply_to":["questions","questions-creation",1]});
    let merge = record(
        "calls",
        "lead",
        1,
        "Type: call-merge\nLinks: [[\"a\",\"questions\",\"questions-creation\",1],[\"b\",\"questions\",\"questions-creation\",1]]",
    );
    let boards = BTreeMap::from([
        (
            "a".into(),
            vec![question(1), level("a", 1, 1, "red"), reply],
        ),
        ("b".into(), vec![question(1), level("b", 1, 1, "blue")]),
        ("top".into(), vec![merge]),
    ]);
    let result = model::project(&boards, Some("top"), None);
    assert!(result.open(Some("a")).is_empty());
    assert_eq!(result.open(None)[0]["level"], "blue");
}

#[test]
fn decision_positions_determine_recency_when_the_clock_moves_backwards() {
    let mut later = decision("a", 1, 2, "top lead", "Newer position");
    later["sent_at"] = json!([0, 0]);
    let result = project(
        vec![question(1)],
        vec![decision("a", 1, 1, "top lead", "Older position"), later],
    );
    assert!(result.calls.is_empty()); // Decision proof does not require a level.
    let decisions = result.decisions_for(&link("a", 1));
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions.last().unwrap()["body"], "Newer position");
}

#[test]
fn an_outgoing_top_lead_cannot_hide_the_seated_candidates_ready_report() {
    let ready = record(
        "questions",
        "candidate",
        1,
        "Type: ready\nFor: owner\n\nReady.",
    );
    let mut reply = record("questions", "lead", 2, "Decided by: lead\n\nIgnore ready.");
    reply["headers"] = json!({"reply_to":["questions","questions-creation",1]});
    let result = model::project(
        &BTreeMap::from([("top".into(), vec![ready, reply])]),
        Some("top"),
        Some("candidate"),
    );
    assert_eq!(result.open(None)[0]["level"], "orange");
}
