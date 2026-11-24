use super::*;
use std::process::Output;
use std::time::Instant;

struct CopiedBoard {
    config: PathBuf,
    socket: SocketDir,
}

impl CopiedBoard {
    fn from(source: &mut Fixture) -> Self {
        stop(source);

        let socket = SocketDir::new().unwrap();
        let config = write_config(socket.path(), "markdown");
        copy_state(&source.socket.path().join("st"), &socket.path().join("st"));

        source.board = launch(&source.config);
        source.wait_until_ready();
        Self { config, socket }
    }

    fn command(&self, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        command
            .arg("--config")
            .arg(&self.config)
            .env("HOME", self.socket.path())
            .args(words)
            .env_remove("MALAK_HANDOVER");
        command
    }
}

fn stop(board: &mut Fixture) {
    run(&mut board.owner_command(&["board", "stop"]));
    assert!(board.board.wait().unwrap().success());
}

fn copy_state(from: &Path, to: &Path) {
    run(Command::new("cp").arg("-pR").arg(from).arg(to));
}

fn assert_wrong_board(command: &mut Command) {
    let output = bounded_output(command);
    let reason = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{command:?}: {output:?}");
    assert!(reason.contains("belongs to a different board"), "{reason}");
    assert!(reason.contains("--config"), "{reason}");
    assert!(
        reason.contains("keep its original socket_folder"),
        "{reason}"
    );
    assert!(output.stdout.is_empty(), "{output:?}");
}

fn bounded_output(command: &mut Command) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{command:?} did not exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn stopping_a_copied_board_cannot_stop_its_source() {
    let mut source = Fixture::new();
    let copy = CopiedBoard::from(&mut source);
    let run = source.status()["run"].clone();

    assert_wrong_board(&mut copy.command(&["board", "stop"]));

    assert_eq!(source.status()["run"], run);
}

#[test]
fn owner_commands_refuse_a_copied_owner_credential() {
    let mut source = Fixture::new();
    let (inbox, _) = source.enrol("alice");
    let id = inbox.strip_prefix("to/").unwrap();
    let copy = CopiedBoard::from(&mut source);
    let handover = copy.socket.path().join("new.handover");
    let out = handover.to_str().unwrap();
    let before = source.status();

    for words in [
        &["board", "status", "--json"][..],
        &["enrol", "bob", "--out", out],
        &["grant", id, "lead:read"],
        &["revoke", id],
        &["rekey", id, "--out", out],
        &["channel", "create", "scratch"],
        &["channel", "remove", "orders"],
        &["trim", "lead", "1"],
        &["tail", "--json"],
        &["mailbox", "--plain"],
        &["mailbox", "--who"],
        &["mailbox", "--questions", "--json"],
        &["mailbox", "--orders"],
        &["mailbox", "--waiting", "--json"],
        &["mailbox", "--trace", "abcd", "--json"],
    ] {
        assert_wrong_board(&mut copy.command(words));
    }

    let after = source.status();
    assert_eq!(after["participants"], before["participants"]);
    assert_eq!(after["channels"], before["channels"]);
    assert!(!handover.exists());
}

#[test]
fn starting_copied_state_at_a_different_socket_is_refused() {
    let mut source = Fixture::new();
    let copy = CopiedBoard::from(&mut source);

    assert_wrong_board(&mut copy.command(&["board", "start"]));
    assert_wrong_board(&mut copy.command(&["board", "start", "--detach"]));

    assert!(!copy.socket.path().join("d").exists());
    assert!(!copy.socket.path().join("st/board.log").exists());
}

#[test]
fn recovery_of_copied_state_runs_offline_against_the_copy() {
    let mut source = Fixture::new();
    let copy = CopiedBoard::from(&mut source);
    let run_id = source.status()["run"].clone();

    let copied_recovery = run(&mut copy.command(&["recover"]));

    assert!(copied_recovery.starts_with("No stopped channels.\n"));
    assert_eq!(source.status()["run"], run_id);
}

#[test]
fn moving_board_state_keeps_the_board_at_its_original_socket() {
    assert_reopens_with_relocated_state(|from, to| std::fs::rename(from, to).unwrap());
}

#[test]
fn restoring_board_state_keeps_the_board_at_its_original_socket() {
    assert_reopens_with_relocated_state(copy_state);
}

fn assert_reopens_with_relocated_state(relocate: impl FnOnce(&Path, &Path)) {
    let relocated = SocketDir::new().unwrap();
    let mut board = Fixture::new();
    let (_, alice) = board.enrol("alice");
    call(&alice, &["send", "lead", "before the move"]);
    let previous_run = board.status()["run"].clone();
    stop(&mut board);

    relocate(
        &board.socket.path().join("st"),
        &relocated.path().join("st"),
    );
    board.config =
        write_config_with_socket(relocated.path(), "markdown", &board.socket.path().join("d"));
    board.board = launch(&board.config);
    board.wait_until_ready();

    assert_ne!(board.status()["run"], previous_run);
    call(&alice, &["send", "lead", "after the move"]);
    let read = run(&mut board.without_handover(&["read", "lead", "--json"]));
    let bodies: Vec<_> = messages(&read)
        .into_iter()
        .map(|message| message["body"].clone())
        .collect();
    assert_eq!(
        bodies,
        [
            serde_json::json!(b"before the move"),
            serde_json::json!(b"after the move"),
        ]
    );
    stop(&mut board);
}
