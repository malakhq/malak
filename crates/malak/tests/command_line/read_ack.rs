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
