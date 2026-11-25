use super::{Fixture, MALAK, run, terminal};
use std::process::Command;

fn enrol(board: &Fixture, pid: u32) -> (std::process::Output, std::path::PathBuf) {
    let handover = board.socket.path().join("bound");
    let output = board
        .without_handover(&[
            "enrol",
            "bound",
            "--session",
            &pid.to_string(),
            "--grant",
            "lead:send",
        ])
        .arg("--out")
        .arg(&handover)
        .output()
        .unwrap();
    (output, handover)
}

#[test]
fn enrol_refuses_a_missing_session() {
    let board = Fixture::new();
    let (output, handover) = enrol(&board, i32::MAX as u32);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("is not running"));
    assert!(!handover.exists());
    assert!(
        board.status()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["name"] != "bound")
    );
}

#[test]
fn enrol_session_binding_survives_board_restart() {
    let mut board = Fixture::new();
    let (output, handover) = enrol(&board, std::process::id());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    board.restart();

    let allowed = run(Command::new(MALAK)
        .env("HOME", board.socket.path())
        .env("MALAK_HANDOVER", &handover)
        .args(["send", "lead", "bound session after restart"]));
    assert!(allowed.contains("lead #"));

    let refused = Command::new(std::env::current_exe().unwrap())
        .env("HOME", board.socket.path())
        .env("MALAK_TEST_BOUND_HANDOVER", &handover)
        .args([
            "--exact",
            "session_binding::different_session_is_refused",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        refused.status.success(),
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );
}

#[test]
fn different_session_is_refused() {
    let Some(handover) = std::env::var_os("MALAK_TEST_BOUND_HANDOVER") else {
        return;
    };
    let output = Command::new(MALAK)
        .env("MALAK_HANDOVER", handover)
        .args(["send", "lead", "a different session"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("handover is in use by another session")
    );
}

fn claim(board: &Fixture, name: &str) -> serde_json::Value {
    board.status()["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["name"] == name && record["revoked"] == false)
        .unwrap()["claim"]
        .clone()
}

#[test]
fn a_refused_attach_records_the_peer_without_posting_a_message() {
    let board = Fixture::new();
    let (output, handover) = enrol(&board, std::process::id());
    assert!(output.status.success());

    let refused = Command::new(std::env::current_exe().unwrap())
        .env("HOME", board.socket.path())
        .env("MALAK_TEST_BOUND_HANDOVER", &handover)
        .args([
            "--exact",
            "session_binding::different_session_is_refused",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        refused.status.success(),
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );

    let claim = claim(&board, "bound");
    assert_eq!(claim["refused"], 1);
    let refusal = &claim["history"][1];
    assert_eq!(refusal["event"], "refused");
    assert!(refusal["at_ms"].as_u64().unwrap() > 0);
    assert!(refusal["peer"]["pid"].as_u64().unwrap() > 0);
    assert!(refusal["peer"]["program"].is_string());
    assert!(refusal["peer"]["cwd"].is_string());
    assert_eq!(
        board.status()["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| channel["name"] == "lead")
            .unwrap()["head"],
        1
    );
}

#[test]
fn the_current_lead_session_can_release_a_worker_without_a_terminal() {
    let board = Fixture::new();
    run(&mut board.without_handover(&["recipe", "lead-up"]));
    let (output, handover) = enrol(&board, std::process::id());
    assert!(output.status.success());

    run(&mut board.without_handover(&["release", "bound"]));

    let claim = claim(&board, "bound");
    assert_eq!(claim["state"], "released");
    assert_eq!(claim["history"][1]["event"], "released");
    let refused = Command::new(MALAK)
        .env("HOME", board.socket.path())
        .env("MALAK_HANDOVER", handover)
        .args(["send", "lead", "released"])
        .output()
        .unwrap();
    assert!(!refused.status.success());
}

#[test]
fn the_owner_release_requires_a_terminal_when_the_caller_has_no_lead_session() {
    let board = Fixture::new();
    let (output, _) = enrol(&board, std::process::id());
    assert!(output.status.success());

    let refused = board
        .without_handover(&["release", "bound"])
        .output()
        .unwrap();

    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("interactive owner confirmation"));
    assert_eq!(claim(&board, "bound")["state"], "bound");
}

#[test]
fn a_lead_claim_refuses_a_different_session() {
    let board = Fixture::new();
    run(&mut board.without_handover(&["recipe", "lead-up"]));

    let refused = Command::new(std::env::current_exe().unwrap())
        .env("HOME", board.socket.path())
        .env("MALAK_TEST_BOUND_HANDOVER", board.lead())
        .args([
            "--exact",
            "session_binding::different_session_is_refused",
            "--nocapture",
        ])
        .output()
        .unwrap();

    assert!(
        refused.status.success(),
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_eq!(claim(&board, "lead")["state"], "bound");
    assert_eq!(claim(&board, "lead")["refused"], 1);
}

#[test]
fn the_owner_can_release_with_typed_terminal_confirmation() {
    let board = Fixture::new();
    let (output, _) = enrol(&board, std::process::id());
    assert!(output.status.success());
    let (master, slave) = terminal::terminal_pair(25, 100);
    let mut child = board
        .without_handover(&["release", "bound"])
        .stdin(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    let mut terminal = terminal::TerminalOutput::new(master);
    terminal.wait_for("Type bound to release");
    terminal.press(b"bound\n");

    assert!(child.wait().unwrap().success());
    assert_eq!(claim(&board, "bound")["state"], "released");
}

#[test]
fn an_ended_claim_stays_ended_until_explicit_reissue() {
    let mut board = Fixture::new();
    let mut session = Command::new("sleep").arg("30").spawn().unwrap();
    let (output, handover) = enrol(&board, session.id());
    session.kill().unwrap();
    session.wait().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    board.restart();

    assert_eq!(claim(&board, "bound")["state"], "ended");
    let old_key = std::fs::read(&handover).unwrap();
    let (output, _) = enrol(&board, std::process::id());

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let claim = claim(&board, "bound");
    assert_eq!(claim["state"], "bound");
    assert_eq!(claim["history"][1]["event"], "ended");
    assert_eq!(claim["history"][2]["event"], "issued");
    assert_ne!(std::fs::read(&handover).unwrap(), old_key);
}
