//! Name aliases cross the real CLI and board boundary.

use super::{Fixture, call, messages};

fn worker(board: &Fixture) -> (std::path::PathBuf, String) {
    let (inbox, handover) = board.enrol("reviewer");
    (handover, inbox)
}

#[test]
fn inbox_names_send_and_get_use_the_real_id_channel() {
    let board = Fixture::new();
    let (_, inbox) = worker(&board);
    call(&board.lead(), &["send", "to/reviewer", "Review this."]);

    let got = call(&board.lead(), &["get", "to/reviewer", "1", "--json"]);
    assert_eq!(messages(&got)[0]["channel"], inbox);
}

#[test]
fn inbox_names_ack_without_creation_discovers_the_resolved_channel() {
    let board = Fixture::new();
    worker(&board);
    call(&board.lead(), &["send", "to/reviewer", "Review this."]);
    call(&board.lead(), &["read", "to/reviewer"]);

    call(&board.lead(), &["ack", "to/reviewer", "1"]);
    let got = call(&board.lead(), &["read", "to/reviewer", "--json"]);
    assert!(messages(&got).is_empty());
}

#[test]
fn inbox_names_reply_inherits_trace_and_stores_an_id_address() {
    let board = Fixture::new();
    let (_, inbox) = worker(&board);
    call(
        &board.lead(),
        &["send", "--kind", "task", "to/reviewer", "Review this."],
    );
    let original = call(&board.lead(), &["get", "to/reviewer", "1", "--json"]);
    let original = &messages(&original)[0];
    let creation = original["creation"].as_str().unwrap();

    call(
        &board.lead(),
        &[
            "send",
            "lead",
            "Done.",
            "--reply-to",
            "to/reviewer",
            creation,
            "1",
        ],
    );
    let got = call(&board.lead(), &["get", "lead", "1", "--json"]);
    let message = &messages(&got)[0];
    assert_eq!(message["headers"]["reply_to"][0], inbox);
    assert_eq!(message["headers"]["trace"], original["headers"]["trace"]);
}

#[test]
fn inbox_names_a_typo_is_refused_without_creating_a_channel() {
    let board = Fixture::new();
    worker(&board);
    let output = board
        .without_handover(&["send", "to/revewer", "Review this."])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reviewer"));
    let status = board.status();
    assert!(
        !status["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|channel| channel["name"] == "to/revewer")
    );
}

#[test]
fn inbox_names_owner_remove_discovers_and_prints_the_resolved_id_creation() {
    let board = Fixture::new();
    let (_, inbox) = worker(&board);

    let output = super::run(&mut board.owner_command(&["channel", "remove", "to/reviewer"]));
    assert!(
        output.contains(&format!("Removing inbox {inbox}.")),
        "{output}"
    );
    assert!(
        !board.status()["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|channel| channel["name"] == inbox)
    );
}

#[test]
fn inbox_names_owner_remove_refuses_an_unknown_name_with_a_suggestion() {
    let board = Fixture::new();
    worker(&board);

    let output = board
        .owner_command(&["channel", "remove", "to/revewer"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reviewer"));
}

#[test]
fn inbox_names_owner_trim_discovers_the_name_addressed_creation() {
    let board = Fixture::new();
    let (_, inbox) = worker(&board);
    call(&board.lead(), &["send", "to/reviewer", "Review this."]);

    super::run(&mut board.owner_command(&["trim", "to/reviewer", "2"]));
    let got = call(&board.lead(), &["get", &inbox, "1", "--json"]);
    let value: serde_json::Value = serde_json::from_str(got.trim()).unwrap();
    assert_eq!(value["result"], "trimmed");
}
