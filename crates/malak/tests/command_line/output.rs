//! Human-readable output and public receipt formats.

use super::*;

#[test]
fn channel_creation_says_whether_the_channel_was_already_there() {
    let board = Fixture::new();

    let first = run(&mut board.owner_command(&["channel", "create", "work"]));
    let second = run(&mut board.owner_command(&["channel", "create", "work"]));

    assert_eq!(first.lines().next(), Some("Channel work created."));
    assert_eq!(second.lines().next(), Some("Channel work already exists."));
    assert_eq!(first.lines().nth(1), second.lines().nth(1));
}

#[test]
fn default_get_shows_a_multiline_task_and_its_copyable_creation() {
    let board = Fixture::new();
    let sent = call(
        &board.lead(),
        &["send", "--json", "--kind", "task", "lead", "First\nSecond"],
    );
    let receipt: Value = serde_json::from_str(&sent).unwrap();

    let shown = call(&board.lead(), &["get", "lead", "1"]);

    assert!(shown.starts_with("Task from lead in lead #1\n"), "{shown}");
    assert!(shown.contains(&format!(
        "  Creation  {}\n",
        receipt["creation"].as_str().unwrap()
    )));
    assert!(
        shown.ends_with("  Body\n    First\n    Second\n\n"),
        "{shown}"
    );
}

#[test]
fn enrol_json_returns_only_the_canonical_id_and_written_path() {
    let board = Fixture::new();
    let path = board.socket.path().join("handover with spaces");

    let shown = run(board
        .owner_command(&["enrol", "--json", "reader", "--out"])
        .arg(&path));

    let value: Value = serde_json::from_str(&shown).unwrap();
    let person = board.status()["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|person| person["name"] == "reader")
        .unwrap()
        .clone();
    assert_eq!(
        value,
        serde_json::json!({ "id": person["id"], "handover": path })
    );
}

#[test]
fn default_recovery_displays_the_complete_plan_number() {
    let mut board = Fixture::new();
    run(&mut board.owner_command(&["board", "stop"]));
    board.board.wait().unwrap();

    let shown = run(&mut board.owner_command(&["recover"]));

    assert!(shown.starts_with("No stopped channels.\n"), "{shown}");
    let plan = shown
        .lines()
        .find_map(|line| line.strip_prefix("  Plan  "))
        .unwrap();
    assert!(plan.len() > 64, "{plan}");
}

#[test]
fn parser_diagnostics_escape_controls_in_rejected_arguments() {
    let result = Command::new(MALAK)
        .args(["board", "start", "--unsafe\u{1b}[2J\nsecond"])
        .output()
        .unwrap();

    let error = String::from_utf8(result.stderr).unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(error.contains("--unsafe\\u{1b}[2J\\nsecond"), "{error}");
    assert!(!error.contains('\u{1b}'), "{error}");
}

#[test]
fn renamed_binary_uses_malak_in_help() {
    let directory = SocketDir::new().unwrap();
    let binary = directory.path().join("malak-test-binary");
    std::os::unix::fs::symlink(MALAK, &binary).unwrap();

    let output = run(Command::new(binary).args(["board", "start", "--help"]));

    assert!(output.contains("Usage: malak board start"), "{output}");
}

#[test]
fn renamed_binary_uses_malak_in_usage_errors() {
    let directory = SocketDir::new().unwrap();
    let binary = directory.path().join("malak-test-binary");
    std::os::unix::fs::symlink(MALAK, &binary).unwrap();

    let result = Command::new(binary)
        .args(["board", "unknown"])
        .output()
        .unwrap();

    let error = String::from_utf8(result.stderr).unwrap();
    assert!(error.contains("malak board"), "{error}");
    assert!(!error.contains("malak-test-binary"), "{error}");
}

#[test]
fn the_command_line_refuses_hostile_names_and_ids_for_each_disk_store() {
    for store in ["markdown", "sqlite"] {
        let board = Fixture::with_store(store, launch);
        let lead = board.lead();
        call(&lead, &["send", "lead", "--id", "seed", "original"]);
        let long = "a".repeat(129);
        let controls = "lead\u{1b}[2J";
        for (channel, id) in [
            ("../lead", "seed"),
            ("lead//alias", "seed"),
            ("Lead", "seed"),
            (controls, "seed"),
            (&long, "seed"),
            ("lead", "../seed"),
            ("lead", controls),
            ("lead", &long),
        ] {
            let output = Command::new(MALAK)
                .args(["send", channel, "--id", id, "unexpected"])
                .env("MALAK_HANDOVER", &lead)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{store} {channel:?} {id:?}");
            assert!(output.stdout.is_empty());
            assert!(!output.stderr.contains(&0x1b));
        }

        let delivered = messages(&call(&lead, &["read", "lead", "--json"]));
        assert_eq!(delivered.len(), 1, "{store}");
        assert_eq!(delivered[0]["text"], "original", "{store}");
    }
}

#[test]
fn the_unix_participant_bridge_refuses_hostile_wire_fields_for_each_disk_store() {
    use malak_core::{HandoverFile, LocalClient};
    use malak_proto::encode_frame;
    use malak_transport_unix::UnixTransport;
    use std::sync::Arc;

    let wait = Duration::from_secs(5);
    for store in ["markdown", "sqlite"] {
        let board = Fixture::with_store(store, launch);
        let handover = HandoverFile::read(board.lead()).unwrap();
        let mut client =
            LocalClient::participant(Arc::new(UnixTransport::default()), handover, None).unwrap();
        let long = "a".repeat(129);
        let long_header = format!("x-{}", "h".repeat(2048));
        let controls = "bad\u{1b}[2J";
        let invalid = [
            ("../lead", "seed", "x-test", "safe"),
            ("lead//alias", "seed", "x-test", "safe"),
            ("Lead", "seed", "x-test", "safe"),
            (controls, "seed", "x-test", "safe"),
            (&long, "seed", "x-test", "safe"),
            ("lead", "../seed", "x-test", "safe"),
            ("lead", controls, "x-test", "safe"),
            ("lead", &long, "x-test", "safe"),
            ("lead", "seed", "sender", "lead"),
            ("lead", "seed", "../x-alias", "safe"),
            ("lead", "seed", controls, "safe"),
            ("lead", "seed", &long_header, "safe"),
            ("lead", "seed", "x-test", controls),
        ];

        for (n, (channel, id, key, value)) in invalid.into_iter().enumerate() {
            let rid = n as u64 + 1;
            let request = encode_frame(
                rid,
                "SEND",
                3,
                |out| {
                    out.str("channel").unwrap().str(channel).unwrap();
                    out.str("id").unwrap().str(id).unwrap();
                    out.str("headers").unwrap().map(1).unwrap();
                    out.str(key).unwrap().str(value).unwrap();
                },
                b"unexpected",
            )
            .unwrap();
            client.send_frame(request, wait).unwrap();
            let reply = client.read_frame(wait).unwrap();
            assert_eq!(
                (reply.op(), reply.rid()),
                ("ERR", rid),
                "{store} {channel:?} {id:?} {key:?}"
            );
        }

        let output = call(&board.lead(), &["read", "lead", "--json"]);
        assert!(messages(&output).is_empty(), "{store}");
    }
}

#[test]
fn a_task_retry_returns_its_original_trace_after_restart() {
    for store in ["markdown", "sqlite"] {
        let mut board = Fixture::with_store(store, launch);
        let send = [
            "send",
            "--json",
            "--kind",
            "task",
            "--id",
            "retry-task",
            "lead",
            "same",
        ];
        let mut expected: Value = serde_json::from_str(&call(&board.lead(), &send)).unwrap();
        expected["duplicate"] = Value::Bool(true);
        board.restart();

        let retry: Value = serde_json::from_str(&call(&board.lead(), &send)).unwrap();

        assert_eq!(retry, expected, "{store}");
        assert_eq!(retry["trace"].as_str().unwrap().len(), 32);
        assert_eq!(
            messages(&call(&board.lead(), &["read", "lead", "--json"])).len(),
            1
        );
    }
}

#[test]
fn a_reply_retry_keeps_the_inherited_trace_after_both_messages_are_trimmed() {
    let mut board = Fixture::new();
    let lead = board.lead();
    let first = call(
        &lead,
        &["send", "--json", "--trace", "aBcD", "lead", "task"],
    );
    let task: Value = serde_json::from_str(&first).unwrap();
    let send = [
        "send",
        "--json",
        "--id",
        "report-1",
        "--reply-to",
        "lead",
        task["creation"].as_str().unwrap(),
        "1",
        "lead",
        "done",
    ];
    let mut expected: Value = serde_json::from_str(&call(&lead, &send)).unwrap();
    expected["duplicate"] = Value::Bool(true);
    run(&mut board.owner_command(&["trim", "lead", "3"]));
    board.restart();

    let retry: Value = serde_json::from_str(&call(&lead, &send)).unwrap();

    assert_eq!(retry, expected);
    assert_eq!(retry["trace"], "aBcD");
    assert!(messages(&call(&lead, &["read", "lead", "--json"])).is_empty());
}
#[test]
fn get_json_preserves_a_miss_and_its_address() {
    let board = Fixture::new();
    let lead = board.lead();
    let sent = call(&lead, &["send", "--json", "lead", "first"]);
    let receipt: Value = serde_json::from_str(&sent).unwrap();

    let shown = call(&lead, &["get", "--json", "lead", "2"]);

    assert_eq!(
        serde_json::from_str::<Value>(&shown).unwrap(),
        serde_json::json!({
            "result": "never_written", "channel": "lead", "position": 2,
            "creation": receipt["creation"]
        })
    );
}

#[test]
fn read_json_closes_with_a_summary_of_present_and_absent_channels() {
    let board = Fixture::new();
    let lead = board.lead();
    let receipt: Value =
        serde_json::from_str(&call(&lead, &["send", "--json", "lead", "first"])).unwrap();
    call(&lead, &["send", "lead", "second"]);

    let shown = call(&lead, &["read", "--json", "lead", "absent"]);
    let records: Vec<Value> = shown
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["text"], "first");
    assert_eq!(records[1]["text"], "second");
    assert_eq!(
        records[2],
        serde_json::json!({"channels": [
            {"channel": "lead", "creation": receipt["creation"], "voids": 0},
            {"channel": "absent", "voids": 0}
        ]})
    );
}

#[test]
fn get_agent_json_includes_fields_for_a_trimmed_message() {
    let board = Fixture::new();
    let lead = board.lead();
    let receipt: Value =
        serde_json::from_str(&call(&lead, &["send", "--json", "lead", "first"])).unwrap();
    run(&mut board.owner_command(&["trim", "lead", "2"]));

    let shown = call(&lead, &["get", "--agent", "--json", "lead", "1"]);

    assert_eq!(
        serde_json::from_str::<Value>(&shown).unwrap(),
        serde_json::json!({
            "messages": [], "result": "trimmed", "channel": "lead", "position": 1,
            "creation": receipt["creation"], "trimmed_before": 2
        })
    );
}

#[test]
fn read_agent_offset_applies_only_to_the_first_message() {
    let board = Fixture::new();
    let lead = board.lead();
    call(&lead, &["send", "lead", "éfirst"]);
    call(&lead, &["send", "other", "second"]);

    let shown = call(
        &lead,
        &[
            "read", "--agent", "--json", "--offset", "2", "lead", "other",
        ],
    );
    let records: Value = serde_json::from_str(&shown).unwrap();

    assert_eq!(records["messages"][0]["body"], "first");
    assert_eq!(records["messages"][1]["body"], "second");
}

#[test]
fn an_empty_read_still_returns_a_channel_summary() {
    let board = Fixture::new();
    let shown = call(&board.lead(), &["read", "--json", "absent"]);

    assert_eq!(
        shown,
        "{\"channels\":[{\"channel\":\"absent\",\"voids\":0}]}\n"
    );
}

#[test]
fn read_agent_text_accepts_a_body_offset() {
    let board = Fixture::new();
    let lead = board.lead();
    call(&lead, &["send", "lead", "skippedprefix body"]);

    let shown = call(&lead, &["read", "--agent", "--offset", "7", "lead"]);

    assert!(shown.contains("prefix body"));
    assert!(!shown.contains("skipped"));
}

#[test]
fn read_agent_offset_rejects_a_split_utf8_character() {
    let board = Fixture::new();
    let lead = board.lead();
    call(&lead, &["send", "lead", "éfirst"]);

    let result = board
        .owner_command(&["read", "--agent", "--offset", "1", "lead"])
        .env("MALAK_HANDOVER", &lead)
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("splits a UTF-8 character")
    );
}
