//! `read --ack` acknowledges exactly the messages it printed, after printing them.

use super::*;

struct Inbox {
    board: Fixture,
    name: String,
    handover: PathBuf,
}

impl Inbox {
    fn with_messages(count: usize) -> Self {
        let board = Fixture::new();
        let (name, handover) = board.enrol("reader");
        let inbox = Self {
            board,
            name,
            handover,
        };
        for number in 1..=count {
            inbox.send(&format!("message {number}"));
        }
        inbox
    }

    fn send(&self, body: &str) {
        run(&mut self.board.without_handover(&["send", &self.name, body]));
    }

    fn call(&self, words: &[&str]) -> String {
        call(&self.handover, words)
    }

    fn bodies(&self) -> Vec<String> {
        messages(&self.call(&["read", &self.name, "--json"]))
            .iter()
            .map(|message| message["text"].as_str().unwrap().to_string())
            .collect()
    }

    fn read_without_ack_grant(&self) -> std::process::Output {
        let id = self.name.strip_prefix("to/").unwrap();
        run(&mut self
            .board
            .owner_command(&["grant", id, &format!("{}:read,get", self.name)]));
        Command::new(MALAK)
            .args(["read", &self.name, "--ack"])
            .env("MALAK_HANDOVER", &self.handover)
            .output()
            .unwrap()
    }
}

#[test]
fn read_without_ack_leaves_messages_waiting() {
    let inbox = Inbox::with_messages(1);

    inbox.call(&["read", &inbox.name]);

    assert_eq!(inbox.bodies(), ["message 1"]);
}

#[test]
fn ack_acknowledges_the_messages_it_printed() {
    let inbox = Inbox::with_messages(2);

    let shown = inbox.call(&["read", &inbox.name, "--ack"]);

    assert!(shown.contains("message 1") && shown.contains("message 2"));
    assert!(inbox.bodies().is_empty());
}

#[test]
fn ack_with_a_limit_acknowledges_only_the_printed_prefix() {
    let inbox = Inbox::with_messages(3);

    let shown = inbox.call(&["read", &inbox.name, "--limit", "1", "--ack"]);

    assert!(shown.contains("message 1") && !shown.contains("message 2"));
    assert_eq!(inbox.bodies(), ["message 2", "message 3"]);
}

#[test]
fn ack_works_with_json_output() {
    let inbox = Inbox::with_messages(1);

    let shown = inbox.call(&["read", &inbox.name, "--json", "--ack"]);

    assert_eq!(messages(&shown)[0]["text"], "message 1");
    assert!(inbox.bodies().is_empty());
}

#[test]
fn ack_works_with_wait() {
    let inbox = Inbox::with_messages(0);
    let reader = Command::new(MALAK)
        .args(["read", &inbox.name, "--wait", "10", "--ack"])
        .env("MALAK_HANDOVER", &inbox.handover)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    inbox.send("late arrival");

    let output = reader.wait_with_output().unwrap();

    assert!(String::from_utf8_lossy(&output.stdout).contains("late arrival"));
    assert!(inbox.bodies().is_empty());
}

#[test]
fn a_waiting_read_survives_a_board_restart() {
    let mut inbox = Inbox::with_messages(0);
    let reader = start_waiting_read(&inbox, "15");
    inbox.board.restart();
    inbox.send("arrived after restart");

    let output = reader.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let received = messages(&String::from_utf8(output.stdout).unwrap());
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["text"], "arrived after restart");
    assert!(inbox.bodies().is_empty());
}

#[test]
fn a_waiting_read_reports_a_link_that_does_not_return() {
    let mut inbox = Inbox::with_messages(0);
    let reader = start_waiting_read(&inbox, "3");
    inbox.board.board.kill().unwrap();
    inbox.board.board.wait().unwrap();

    let output = reader.wait_with_output().unwrap();
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{diagnostic}");
    assert!(diagnostic.contains("link dropped"), "{diagnostic}");
    assert!(
        diagnostic.contains("board stopped or is restarting"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("handover"), "{diagnostic}");
    assert!(output.stdout.is_empty());
}

fn start_waiting_read(inbox: &Inbox, seconds: &str) -> Child {
    let mut reader = Command::new(MALAK)
        .args(["read", &inbox.name, "--wait", seconds, "--ack", "--json"])
        .env("MALAK_HANDOVER", &inbox.handover)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let id = inbox.name.strip_prefix("to/").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let status = inbox.board.status();
        if status["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|participant| participant["id"] == id && participant["connections"] == 1)
        {
            return reader;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = reader.kill();
    let output = reader.wait_with_output().unwrap();
    panic!(
        "waiting reader did not attach: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn ack_with_nothing_printed_succeeds() {
    let inbox = Inbox::with_messages(0);

    let shown = inbox.call(&["read", &inbox.name, "--ack"]);
    assert!(shown.contains(&format!("No messages returned from {}.", inbox.name)));
}

#[test]
fn a_failure_while_printing_acknowledges_nothing() {
    let inbox = Inbox::with_messages(1);

    // A pipe with no reader stands for a process lost before it could ack.
    let mut reader = Command::new(MALAK)
        .args(["read", &inbox.name, "--ack"])
        .env("MALAK_HANDOVER", &inbox.handover)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(reader.stdout.take());
    let failed = reader.wait_with_output().unwrap();

    assert!(!failed.status.success());
    assert_eq!(inbox.bodies(), ["message 1"]);
}

#[test]
fn a_refused_ack_says_the_messages_were_shown_and_shows_them_again() {
    let inbox = Inbox::with_messages(1);

    let output = inbox.read_without_ack_grant();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("message 1"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("shown but not acknowledged"));
    assert_eq!(inbox.bodies(), ["message 1"]);
}
