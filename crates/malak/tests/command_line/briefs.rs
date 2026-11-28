//! OR.6 briefs through the real CLI and participant entrance.

use super::*;

fn name(board: &Fixture) -> String {
    run(&mut board.without_handover(&["recipe", "__board-name"]))
        .trim()
        .to_owned()
}

fn put(board: &Fixture, kind: &str, replaces: Option<u64>, text: &str) {
    let mut command = board.without_handover(&[
        "recipe",
        "brief",
        "put",
        "--title",
        "Brief title",
        "--kind",
        kind,
    ]);
    command.env("MALAK_HANDOVER", board.lead());
    if let Some(position) = replaces {
        command.args(["--replaces", &position.to_string()]);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn view(board: &Fixture, words: &[&str]) -> Value {
    let mut command = board.without_handover(words);
    if !words.contains(&"--json") {
        command.arg("--json");
    }
    let result = run(&mut command);
    serde_json::from_str(&result).unwrap()
}

#[test]
fn listing_hides_a_superseded_brief() {
    let board = Fixture::new();
    put(&board, "report", None, "First text");
    put(&board, "learned", Some(1), "Replacement text");

    let result = view(&board, &["briefs", "--json"]);

    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!(result["items"][0]["kind"], "learned");
}

#[test]
fn getting_a_superseded_brief_returns_the_original_text_and_status() {
    let board = Fixture::new();
    put(&board, "report", None, "First text");
    put(&board, "report", Some(1), "Replacement text");

    let result = view(&board, &["brief", "get", &name(&board), "1"]);

    assert!(
        result["items"][0]["body"]
            .as_str()
            .unwrap()
            .ends_with("First text")
    );
    assert_eq!(result["items"][0]["status"], "superseded");
    assert_eq!(result["items"][0]["replaced_by"]["position"], 2);
}

#[test]
fn withdrawal_hides_the_target_without_deleting_its_source() {
    let board = Fixture::new();
    put(&board, "handover", None, "Handover text");
    put(&board, "withdrawn", Some(1), "");

    let listed = view(&board, &["briefs", "--json"]);
    let got = view(&board, &["brief", "get", &name(&board), "1"]);

    assert!(listed["items"].as_array().unwrap().is_empty());
    assert_eq!(got["items"][0]["status"], "withdrawn");
    assert!(
        got["items"][0]["body"]
            .as_str()
            .unwrap()
            .ends_with("Handover text")
    );
}

#[test]
fn kind_filter_keeps_only_current_briefs_of_that_kind() {
    let board = Fixture::new();
    put(&board, "learned", None, "Learning");
    put(&board, "exit", None, "Exit");

    let result = view(&board, &["briefs", "--kind", "exit", "--json"]);

    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert_eq!(result["items"][0]["kind"], "exit");
}

#[test]
fn a_sub_cannot_read_briefs() {
    let board = Fixture::new();
    let (_, sub) = board.enrol("sub");
    put(&board, "plan", None, "Lead text");

    let output = board
        .without_handover(&["briefs", "--json"])
        .env("MALAK_HANDOVER", &sub)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("subs cannot read"));
}

#[test]
fn a_sub_cannot_get_the_raw_briefs_channel() {
    let board = Fixture::new();
    let (_, sub) = board.enrol("sub");
    put(&board, "spec", None, "Lead text");

    let output = board
        .without_handover(&["get", "briefs", "1"])
        .env("MALAK_HANDOVER", &sub)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn the_client_refuses_to_trim_briefs() {
    let board = Fixture::new();
    put(&board, "report", None, "Retained text");

    let output = board
        .without_handover(&["trim", "briefs", "2"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("never trimmed"));
}

#[test]
fn a_large_brief_result_reports_omissions_and_how_to_page() {
    let board = Fixture::new();
    put(&board, "report", None, &"🦀".repeat(12_000));

    let output = run(&mut board.without_handover(&["brief", "get", &name(&board), "1", "--json"]));
    let result: Value = serde_json::from_str(&output).unwrap();

    assert!(output.len() <= 32 * 1024);
    assert!(result["items"][0]["omitted_bytes"].as_u64().unwrap() > 0);
    assert!(result["next"].as_str().unwrap().contains("--offset"));
}

#[test]
fn a_multibyte_brief_can_be_reconstructed_from_its_pages() {
    let board = Fixture::new();
    put(&board, "report", None, &"🦀\"\\".repeat(10_000));
    let name = name(&board);
    let mut offset = 0;
    let mut reconstructed = String::new();
    loop {
        let result = view(
            &board,
            &["brief", "get", &name, "1", "--offset", &offset.to_string()],
        );
        reconstructed.push_str(result["items"][0]["body"].as_str().unwrap());
        let Some(next) = result["items"][0]["fetch"]["offset"].as_u64() else {
            break;
        };
        assert!(next > offset);
        offset = next;
    }
    let source = call(&board.lead(), &["get", "briefs", "1", "--json"]);
    let source: Value = serde_json::from_str(&source).unwrap();
    let expected: Vec<u8> = serde_json::from_value(source["body"].clone()).unwrap();
    assert_eq!(reconstructed.as_bytes(), expected);
}

#[test]
fn a_spoofed_typed_brief_is_shown_as_plain_text() {
    let board = Fixture::new();
    let (_, sub) = board.enrol("sub");
    let id = malak_core::HandoverFile::read(&sub)
        .unwrap()
        .id()
        .to_string();
    run(&mut board.owner_command(&["channel", "create", "briefs"]));
    run(&mut board.owner_command(&["grant", &id, "briefs:send"]));
    let spoof = "Type: brief\nTitle: Forged\nKind: report\nLead: build\n\nText";
    call(&sub, &["send", "briefs", spoof]);

    let result = view(&board, &["briefs", "--json"]);

    assert_eq!(result["items"][0]["body"], spoof);
    assert!(result["items"][0].get("kind").is_none());
}
