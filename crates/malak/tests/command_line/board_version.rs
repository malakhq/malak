use super::Fixture;
use serde_json::Value;
use std::{fs, path::PathBuf, process::Output};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version_record(board: &Fixture) -> PathBuf {
    board.socket.path().join("d/o/version")
}

fn set_version(board: &Fixture, version: &str) {
    // The real board still holds the run lock on this inode.
    fs::write(
        version_record(board),
        format!("{} {version}\n", board.board.id()),
    )
    .unwrap();
}

fn status_json(board: &Fixture) -> Output {
    board
        .without_handover(&["board", "status", "--json"])
        .output()
        .unwrap()
}

#[test]
fn status_text_shows_the_running_board_version() {
    let board = Fixture::new();
    set_version(&board, "0.0.9");

    let text = super::run(&mut board.without_handover(&["board", "status"]));

    assert!(
        text.lines()
            .any(|line| line.split_whitespace().eq(["Version", "0.0.9"])),
        "{text}"
    );
}

#[test]
fn status_json_shows_the_running_board_version() {
    let board = Fixture::new();

    let output = status_json(&board);

    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["board_version"], VERSION);
    assert_eq!(status["board_build"].as_str().unwrap().len(), 64);
    assert_eq!(status["version"], 1);
}

#[test]
fn a_different_build_with_the_same_version_warns() {
    let board = Fixture::new();
    let digest = "0".repeat(64);
    fs::write(
        version_record(&board),
        format!("{} {VERSION} {digest} different-file\n", board.board.id()),
    )
    .unwrap();

    let output = status_json(&board);

    assert!(output.status.success());
    let warning = String::from_utf8(output.stderr).unwrap();
    assert!(warning.starts_with(&format!("Warning: running board build {digest} differs")));
    assert!(warning.contains("Restart with"));
    assert_eq!(warning.lines().count(), 1);
}

#[test]
fn a_matching_board_version_is_quiet() {
    let board = Fixture::new();

    let output = status_json(&board);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
}

#[test]
fn board_list_reports_the_running_build_mismatch() {
    let board = Fixture::new();
    set_version(&board, "0.0.9");

    let output = board
        .without_handover(&["board", "list", "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let boards: Value = serde_json::from_slice(&output.stdout).unwrap();
    let board = &boards[0];
    assert_eq!(board["status"], "running");
    assert!(board["build_warning"].as_str().unwrap().contains("0.0.9"));
}

#[test]
fn a_newer_board_warns_once_without_changing_json_or_exit_status() {
    let board = Fixture::new();
    let (inbox, _) = board.enrol("worker");
    super::run(&mut board.without_handover(&["send", &inbox, "--kind", "task", "Test the board"]));
    // The waiting view needs both an owner session and a participant session.
    let waiting = || {
        board
            .without_handover(&["mailbox", "--waiting", "--json"])
            .output()
            .unwrap()
    };
    let matching = waiting();
    assert!(matching.status.success());
    assert!(matching.stderr.is_empty());
    let json: Value = serde_json::from_slice(&matching.stdout).unwrap();
    assert_eq!(json["waiting"][0]["inbox"], inbox);
    set_version(&board, "99.0.0");

    let mismatch = waiting();

    assert_eq!(mismatch.status, matching.status);
    assert_eq!(mismatch.stdout, matching.stdout);
    let warning = String::from_utf8(mismatch.stderr).unwrap();
    let config = board.config.display();
    assert_eq!(
        warning,
        format!(
            "Warning: running board version 99.0.0 differs from this malak ({VERSION}). Restart with `malak --config '{config}' board stop` then `malak --config '{config}' board start`.\n"
        )
    );
}

#[test]
fn a_participant_with_only_a_handover_can_see_an_owner_readable_version() {
    let board = Fixture::new();
    let (inbox, handover) = board.enrol("worker");
    set_version(&board, "0.0.9");

    let output = std::process::Command::new(super::MALAK)
        .env("MALAK_HANDOVER", handover)
        .args(["read", &inbox, "--json"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let warning = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        warning,
        format!(
            "Warning: running board version 0.0.9 differs from this malak ({VERSION}). Restart with `malak board stop` then `malak board start`.\n"
        )
    );
}

#[test]
#[cfg(target_os = "macos")]
fn a_participant_without_version_record_access_is_quiet() {
    let board = Fixture::new();
    let (inbox, handover) = board.enrol("worker");
    set_version(&board, "0.0.9");
    let record = version_record(&board);
    let policy = format!("(version 1)(allow default)(deny file-read* (literal {record:?}))");

    let output = std::process::Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &policy, super::MALAK, "read", &inbox, "--json"])
        .env("MALAK_HANDOVER", handover)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_running_board_without_a_version_record_is_reported_as_older() {
    let board = Fixture::new();
    fs::remove_file(version_record(&board)).unwrap();

    let output = status_json(&board);

    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status.get("board_version"), Some(&Value::Null));
    let warning = String::from_utf8(output.stderr).unwrap();
    assert_eq!(warning.lines().count(), 1, "{warning}");
    assert!(warning.contains("unknown (older than 0.1.0)"), "{warning}");
}

#[test]
fn a_version_record_from_an_exited_process_is_ignored() {
    let board = Fixture::new();
    // Replace the record with an unlocked one, like a file left by an exited board.
    let record = version_record(&board);
    let previous = record.with_extension("previous");
    fs::rename(&record, &previous).unwrap();
    fs::copy(&previous, &record).unwrap();

    let output = status_json(&board);

    assert!(output.status.success());
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status.get("board_version"), Some(&Value::Null));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown (older than 0.1.0)"));
}

#[test]
fn a_stopped_board_does_not_trigger_a_version_warning() {
    let mut board = Fixture::new();
    set_version(&board, "0.0.9");
    board.board.kill().unwrap();
    board.board.wait().unwrap();

    let output = status_json(&board);

    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no board is answering"), "{stderr}");
    assert!(!stderr.contains("running board version"), "{stderr}");
}
