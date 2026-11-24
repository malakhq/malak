use super::*;

fn guest_recipe(board: &Fixture, name: &str, handover: &Path) -> Command {
    let mut command = board.recipe("guest");
    command.args([name, "--out"]).arg(handover).arg("--json");
    command.env("TMPDIR", board.home.path());
    command
}

fn enroll_guest(board: &Fixture, name: &str, handover: &Path) -> Value {
    serde_json::from_str(&run(guest_recipe(board, name, handover))).unwrap()
}

fn call_guest(board: &Fixture, handover: &Path, args: &[&str]) -> Command {
    let mut command = board.owner(args);
    command.env("MALAK_HANDOVER", handover);
    command
}

fn assert_refused(command: Command, reason: &str) {
    let result = output(command);
    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains(reason),
        "{result:?}"
    );
}

#[test]
fn a_guest_gets_and_acknowledges_a_reply_linked_to_its_report() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest handover");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    assert_eq!(guest["handover"], json!(handover));
    assert_eq!(
        fs::metadata(&handover).unwrap().permissions().mode() & 0o777,
        0o600
    );

    run(call_guest(
        &board,
        &handover,
        &["send", "--kind", "report", "lead", "The build is ready."],
    ));
    let notes = messages(&board.lead(&["read", "--json", "lead"]));
    assert_eq!(notes[0]["sender"], guest["id"]);
    assert_eq!(notes[0]["grade"], "proven");
    assert!(
        board
            .lead(&["read", "lead"])
            .lines()
            .any(|line| { line.contains("Reply inbox") && line.ends_with(inbox) })
    );

    let origin = notes[0]["creation"].as_str().unwrap();
    board.lead(&[
        "send",
        "--reply-to",
        "lead",
        origin,
        "1",
        inbox,
        "Reviewed.",
    ]);
    let reply: Value = serde_json::from_str(&run(call_guest(
        &board,
        &handover,
        &["get", "--json", inbox, "1"],
    )))
    .unwrap();
    assert_eq!(reply["headers"]["reply_to"], json!(["lead", origin, 1]));
    let read = run(call_guest(
        &board,
        &handover,
        &["read", "--ack", "--json", inbox],
    ));
    assert_eq!(messages(&read)[0]["text"], "Reviewed.");
    let remaining = run(call_guest(&board, &handover, &["read", "--json", inbox]));
    assert!(messages(&remaining).is_empty());
}

#[test]
fn upgrading_a_legacy_guest_preserves_its_handover_and_unread_replies() {
    let board = Fixture::new();
    board.start();
    let handover = board.enrol("sender", &["lead:send"]);
    let original_handover = fs::read(&handover).unwrap();
    let original_id = participant(&board.status(), "sender")["id"].clone();
    let inbox = format!("to/{}", original_id.as_str().unwrap());
    board.lead(&["send", &inbox, "The earlier reply."]);
    assert_refused(
        call_guest(&board, &handover, &["read", &inbox]),
        "grant does not allow the call",
    );

    let guest = enroll_guest(&board, "sender", &handover);
    assert_eq!(guest["id"], original_id);
    assert_eq!(guest["handover"], json!(handover));
    assert_eq!(fs::read(&handover).unwrap(), original_handover);
    assert_eq!(enroll_guest(&board, "sender", &handover), guest);
    let read = run(call_guest(
        &board,
        &handover,
        &["read", "--ack", "--json", &inbox],
    ));
    assert_eq!(messages(&read)[0]["text"], "The earlier reply.");
}

#[test]
fn upgrading_a_guest_refuses_a_different_participants_handover() {
    let board = Fixture::new();
    board.start();
    board.enrol("sender", &["lead:send"]);
    let wrong_handover = board.enrol("other", &["lead:send"]);
    let before = participant(&board.status(), "sender")["grant"].clone();

    assert_refused(
        guest_recipe(&board, "sender", &wrong_handover),
        "use the existing handover for guest",
    );
    assert_eq!(participant(&board.status(), "sender")["grant"], before);
}

#[test]
fn reusing_a_guest_name_restores_its_missing_inbox() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    run(board.owner(&["channel", "remove", inbox]));

    assert_eq!(enroll_guest(&board, "sender", &handover), guest);
    board.lead(&["send", inbox, "A reply in the restored inbox."]);
    let received = run(call_guest(
        &board,
        &handover,
        &["get", "--json", inbox, "1"],
    ));
    let record: Value = serde_json::from_str(&received).unwrap();
    assert_eq!(record["text"], "A reply in the restored inbox.");
}

#[test]
fn the_guest_recipe_refuses_a_worker_name() {
    let board = Fixture::new();
    board.start();
    run(board.worker_start(&[], &["sh", "-c", "true"]));
    let before = participant(&board.status(), "alice")["grant"].clone();
    let handover = board.home.path().join("guest");
    assert_refused(
        guest_recipe(&board, "alice", &handover),
        "already belongs to a worker",
    );
    assert_eq!(participant(&board.status(), "alice")["grant"], before);
    assert!(!handover.exists());
}

#[test]
fn the_guest_recipe_refuses_the_lead_name() {
    let board = Fixture::new();
    board.start();
    assert_refused(
        guest_recipe(&board, "lead", &board.home.path().join("guest")),
        "reserved for the lead",
    );
}

#[test]
fn a_revoked_guest_cannot_read_its_inbox() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    board.lead(&["send", inbox, "Private reply."]);
    run(board.owner(&["revoke", "sender"]));

    let result = output(call_guest(&board, &handover, &["read", inbox]));
    assert!(!result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty(), "{result:?}");
    assert_eq!(participant(&board.status(), "sender")["revoked"], true);
}
