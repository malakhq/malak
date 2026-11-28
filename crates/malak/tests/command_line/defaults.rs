//! A first run works with defaults, and a missing board is explained.

use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

/// A fresh home with no configuration, and the commands run in it.
struct Home(BoardHome);

impl Home {
    fn new() -> Self {
        Self(BoardHome::new())
    }

    fn command(&self, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        command
            .args(words)
            .env("HOME", self.0.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("MALAK_HANDOVER");
        command
    }

    fn output(&self, words: &[&str]) -> std::process::Output {
        self.command(words).output().unwrap()
    }

    fn start(&self) -> std::process::Output {
        self.output(&["board", "start", "--detach"])
    }

    /// Stop the board and wait until it has let go of its sockets.
    fn stop(&self) {
        let stopped = self.output(&["board", "stop"]);
        assert!(stopped.status.success(), "{}", stderr(&stopped));
        let sockets = std::fs::File::open(self.0.path().join(".malak/default/sockets")).unwrap();
        sockets.lock().unwrap();
    }

    fn config(&self) -> PathBuf {
        self.0.path().join(".config/malak/board.toml")
    }

    fn state(&self) -> PathBuf {
        self.0.path().join(".malak/default/state")
    }

    fn channels(&self) -> Vec<String> {
        let status = self.output(&["board", "status", "--json"]);
        assert!(status.status.success(), "{}", stderr(&status));
        let status: Value = serde_json::from_slice(&status.stdout).unwrap();
        status["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|channel| channel["name"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn refused_with(output: &std::process::Output, place: &Path) {
    let message = stderr(output);
    assert_eq!(output.status.code(), Some(2), "{message}");
    assert!(message.contains(place.to_str().unwrap()), "{message}");
    assert!(message.contains("malak board start"), "{message}");
    assert!(!message.contains("os error"), "{message}");
}

#[test]
fn a_first_start_creates_the_default_board_and_says_so() {
    let home = Home::new();

    let started = home.start();

    assert!(started.status.success(), "{}", stderr(&started));
    assert!(stderr(&started).contains("created the default board"));
    assert!(home.config().is_file());
}

#[test]
fn the_default_configuration_is_private() {
    let home = Home::new();
    home.start();

    let mode = |path: &Path| path.metadata().unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(&home.config()), 0o600);
    assert_eq!(mode(home.config().parent().unwrap()), 0o700);
    assert_eq!(mode(&home.0.path().join(".malak/default")), 0o700);
}

#[test]
fn a_first_start_creates_the_standard_channels() {
    let home = Home::new();
    home.start();

    assert_eq!(
        home.channels(),
        ["briefs", "calls", "lead", "orders", "questions", "rail"]
    );
}

#[test]
fn a_plain_mailbox_works_on_a_first_run() {
    let home = Home::new();
    home.start();

    let mut mailbox = home
        .command(&["mailbox", "--plain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let heading = BufReader::new(mailbox.stdout.take().unwrap())
        .lines()
        .next()
        .transpose()
        .unwrap();
    let _ = mailbox.kill();
    let result = mailbox.wait_with_output().unwrap();

    assert!(
        heading.is_some_and(|line| line.starts_with("Mailbox activity")),
        "{}",
        stderr(&result)
    );
}

#[test]
fn an_existing_default_board_is_not_replaced() {
    let home = Home::new();
    home.start();
    let before = std::fs::read(home.config()).unwrap();
    home.stop();

    let again = home.start();

    assert!(again.status.success(), "{}", stderr(&again));
    assert!(!stderr(&again).contains("created"));
    assert_eq!(std::fs::read(home.config()).unwrap(), before);
}

#[test]
fn a_board_missing_a_standard_channel_gets_it_on_its_next_start() {
    let home = Home::new();
    home.start();
    let removed = home.output(&["channel", "remove", "questions"]);
    assert!(removed.status.success(), "{}", stderr(&removed));
    assert!(!home.channels().contains(&"questions".to_owned()));
    home.stop();

    let again = home.start();

    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(
        home.channels(),
        ["briefs", "calls", "lead", "orders", "questions", "rail"]
    );
}

#[test]
fn a_restart_leaves_existing_messages_alone() {
    let home = Home::new();
    home.start();
    let handover = home.state().join("lead.handover");
    let send = |text: &str| {
        let mut command = home.command(&["send", "orders", text]);
        command.env("MALAK_HANDOVER", &handover).output().unwrap()
    };
    assert!(send("first").status.success());
    home.stop();
    home.start();

    let sent = send("second");

    assert!(sent.status.success(), "{}", stderr(&sent));
    assert!(String::from_utf8_lossy(&sent.stdout).contains("Note sent to orders #2."));
}

#[test]
fn a_board_with_no_default_configuration_is_explained_by_every_command() {
    let home = Home::new();

    for words in [
        &["mailbox", "--plain"][..],
        &["board", "status"],
        &["board", "stop"],
        &["channel", "create", "x"],
        &["recover"],
    ] {
        refused_with(&home.output(words), &home.config());
    }
}

#[test]
fn a_participant_command_with_no_board_names_where_it_looked() {
    let home = Home::new();

    refused_with(&home.output(&["read", "lead"]), &home.config());
}

#[test]
fn a_missing_handover_names_the_file() {
    let home = Home::new();
    let missing = home.0.path().join("nowhere.handover");

    let output = home
        .command(&["send", "lead", "hi"])
        .env("MALAK_HANDOVER", &missing)
        .output()
        .unwrap();

    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{message}");
    assert!(message.contains(missing.to_str().unwrap()), "{message}");
    assert!(!message.contains("os error"), "{message}");
}

#[test]
fn a_config_path_that_does_not_exist_is_named() {
    let home = Home::new();
    let missing = home.0.path().join("none.toml");

    let output = home.output(&["--config", missing.to_str().unwrap(), "board", "status"]);

    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(2), "{message}");
    assert!(message.contains(missing.to_str().unwrap()), "{message}");
}

#[test]
fn a_stopped_board_says_nothing_is_answering() {
    let home = Home::new();
    home.start();
    home.stop();

    let output = home.output(&["board", "status"]);

    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains(".malak/default/sockets"), "{message}");
    assert!(message.contains("malak board start"), "{message}");
    assert!(!message.contains("os error"), "{message}");
}

#[test]
fn an_invalid_default_configuration_is_not_overwritten() {
    let home = Home::new();
    std::fs::create_dir_all(home.config().parent().unwrap()).unwrap();
    std::fs::File::create(home.config())
        .unwrap()
        .write_all(b"[board]\n")
        .unwrap();

    let started = home.output(&["board", "start"]);

    assert_eq!(started.status.code(), Some(2));
    assert_eq!(std::fs::read(home.config()).unwrap(), b"[board]\n");
}
