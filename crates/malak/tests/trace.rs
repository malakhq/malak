//! A task's messages share one trace, and `handoff` keeps the task on that trace.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_testkit::unix_socket::SocketDir;
use serde_json::Value;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MALAK: &str = env!("CARGO_BIN_EXE_malak");

struct Board {
    home: SocketDir,
    config: PathBuf,
}

/// A participant's name, ID and handover file.
struct Worker {
    id: String,
    handover: PathBuf,
}

/// Where a sent message landed.
struct Receipt {
    creation: String,
    position: String,
}

impl Board {
    fn start() -> Self {
        let home = SocketDir::new().unwrap();
        let directory = home.path().join(".malak/trace");
        fs::create_dir_all(&directory).unwrap();
        let config = directory.join("board.toml");
        fs::write(
            &config,
            format!(
                "[board]\nstate = {:?}\nsocket_folder = {:?}\n",
                home.path().join("state"),
                home.path().join("d")
            ),
        )
        .unwrap();
        let board = Self { home, config };
        stdout(board.recipe("lead-up", &["--config", board.config.to_str().unwrap()]));
        board
    }

    fn lead_handover(&self) -> PathBuf {
        self.home.path().join("state/lead.handover")
    }

    fn recipe_command(&self, name: &str, args: &[&str]) -> Command {
        let recipes = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../recipes");
        let malak = Path::new(MALAK).parent().unwrap().to_path_buf();
        let path = env::var_os("PATH").unwrap();
        let path = env::join_paths([malak, recipes].into_iter().chain(env::split_paths(&path)));
        let mut command = Command::new(name);
        command
            .args(args)
            .env("PATH", path.unwrap())
            .env("HOME", self.home.path())
            .env("MALAK_HANDOVER", self.lead_handover());
        command
    }

    fn recipe(&self, name: &str, args: &[&str]) -> Output {
        self.recipe_command(name, args).output().unwrap()
    }

    fn handoff(&self, args: &[&str]) -> String {
        let config = self.config.to_str().unwrap();
        let args = [args, &["--config", config]].concat();
        stdout(self.recipe("handoff", &args))
    }

    /// Run the command line as a participant; `None` is the lead.
    fn run(&self, handover: Option<&Path>, words: &[&str]) -> Output {
        let lead = self.lead_handover();
        Command::new(MALAK)
            .arg("--config")
            .arg(&self.config)
            .args(words)
            .env("MALAK_HANDOVER", handover.unwrap_or(&lead))
            .output()
            .unwrap()
    }

    fn lead(&self, words: &[&str]) -> String {
        stdout(self.run(None, words))
    }

    fn enrol(&self, name: &str) -> Worker {
        let handover = self.home.path().join(name);
        let out = handover.to_str().unwrap();
        let grants = ["lead:send", "questions:send"];
        let mut words = vec!["enrol", "--json", name, "--out", out];
        words.extend(grants.iter().flat_map(|grant| ["--grant", grant]));
        let output: Value = serde_json::from_str(&self.lead(&words)).unwrap();
        let id = output["id"].as_str().unwrap().to_owned();
        let inbox = format!("to/{id}");
        self.lead(&["channel", "create", &inbox]);
        let own = format!("{inbox}:read,get,ack");
        self.lead(&["grant", &id, "lead:send", "questions:send", &own]);
        Worker { id, handover }
    }

    fn send(&self, from: Option<&Path>, words: &[&str]) -> Receipt {
        let output = stdout(self.run(from, &[&["send", "--json"], words].concat()));
        let receipt: Value = serde_json::from_str(&output).unwrap();
        Receipt {
            creation: receipt["creation"].as_str().unwrap().into(),
            position: receipt["position"].to_string(),
        }
    }

    fn reply(&self, from: Option<&Path>, to: (&str, &Receipt), words: &[&str]) -> Receipt {
        let (channel, receipt) = to;
        let reply = ["--reply-to", channel, &receipt.creation, &receipt.position];
        self.send(from, &[&reply[..], words].concat())
    }

    fn message(&self, channel: &str, position: &str) -> Value {
        serde_json::from_str(&self.lead(&["get", "--json", "--", channel, position])).unwrap()
    }

    fn head(&self, channel: &str) -> u64 {
        let status = self.lead(&["board", "status", "--json"]);
        let status: Value = serde_json::from_str(&status).unwrap();
        let channels = status["channels"].as_array().unwrap();
        channels.iter().find(|c| c["name"] == channel).unwrap()["head"]
            .as_u64()
            .unwrap()
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        let _ = self.run(None, &["board", "stop"]);
    }
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn trace_of(message: &Value) -> Option<&str> {
    message["headers"]["trace"].as_str()
}

fn body(message: &Value) -> String {
    let bytes: Vec<u8> = serde_json::from_value(message["body"].clone()).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn a_new_task_gets_a_w3c_trace_id() {
    let board = Board::start();

    let task = board.send(None, &["--kind", "task", "lead", "work"]);

    let message = board.message("lead", &task.position);
    let trace = trace_of(&message).unwrap();
    assert_eq!(trace.len(), 32);
    assert!(
        trace
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    );
    assert!(trace.bytes().any(|b| b != b'0'));
}

#[test]
fn a_note_that_answers_nothing_carries_no_trace() {
    let board = Board::start();

    let note = board.send(None, &["lead", "hello"]);

    assert_eq!(trace_of(&board.message("lead", &note.position)), None);
}

#[test]
fn a_reply_carries_the_trace_of_the_message_it_answers() {
    let board = Board::start();
    let task = board.send(None, &["--kind", "task", "lead", "work"]);

    let reply = board.reply(None, ("lead", &task), &["lead", "done"]);

    let task = board.message("lead", &task.position);
    let reply = board.message("lead", &reply.position);
    assert_eq!(trace_of(&reply), trace_of(&task));
}

#[test]
fn a_reply_to_a_working_inbox_keeps_the_answered_trace() {
    let board = Board::start();
    let worker = board.enrol("worker");
    let inbox = format!("to/{}", worker.id);
    board.send(None, &["--kind", "task", "--trace", "a1", &inbox, "work"]);
    stdout(board.run(Some(&worker.handover), &["read", "--ack", &inbox]));
    let answered = board.send(None, &["--trace", "b2", "lead", "question"]);

    let reply = board.reply(None, ("lead", &answered), &[&inbox, "answer"]);

    assert_eq!(
        trace_of(&board.message(&inbox, &reply.position)),
        Some("b2")
    );
}

#[test]
fn an_explicit_trace_wins_over_the_answered_trace() {
    let board = Board::start();
    let task = board.send(None, &["--kind", "task", "lead", "work"]);
    board.send(None, &["--kind", "task", "lead", "new work"]);
    board.lead(&["read", "--ack", "lead"]);
    let explicit = "00000000000000000000000000abcdef";

    let reply = board.reply(None, ("lead", &task), &["--trace", explicit, "lead", "x"]);

    assert_eq!(
        trace_of(&board.message("lead", &reply.position)),
        Some(explicit)
    );
}

#[test]
fn a_refused_lookup_of_the_answered_message_still_sends_and_says_so() {
    let board = Board::start();
    let worker = board.enrol("alice");
    // The worker may send to `lead` but may not read what it answers there.
    let first = board.send(None, &["--kind", "task", "lead", "t"]);
    let inbox = format!("to/{}", worker.id);
    board.send(None, &["--kind", "task", &inbox, "working task"]);
    stdout(board.run(Some(&worker.handover), &["read", "--ack", &inbox]));
    let words = [
        "send",
        "--json",
        "--reply-to",
        "lead",
        &first.creation,
        &first.position,
    ];
    let output = board.run(
        Some(&worker.handover),
        &[&words[..], &["lead", "r"]].concat(),
    );

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Without a trace"));
    let receipt: Value = serde_json::from_slice(&output.stdout).unwrap();
    let position = receipt["position"].to_string();
    assert_eq!(trace_of(&board.message("lead", &position)), None);
}

#[test]
fn the_agent_label_shows_the_trace() {
    let board = Board::start();
    let task = board.send(None, &["--kind", "task", "lead", "work"]);
    let trace = trace_of(&board.message("lead", &task.position))
        .unwrap()
        .to_owned();

    let label = board.lead(&["get", "--agent", "--", "lead", &task.position]);

    assert!(label.contains(&format!("\nTrace {trace}\n")));
}

#[test]
fn the_plain_output_shows_the_trace() {
    let board = Board::start();
    let task = board.send(None, &["--kind", "task", "lead", "work"]);
    let trace = trace_of(&board.message("lead", &task.position))
        .unwrap()
        .to_owned();

    let plain = board.lead(&["read", "lead"]);

    assert!(plain.contains(&format!("  Trace     {trace}")));
}

/// A trace with a task, a report, a question and its answer, beside an
/// unrelated task and a note on no trace.
struct Trail {
    board: Board,
    alice: Worker,
    bob: Worker,
    trace: String,
    task: Receipt,
}

fn trail() -> Trail {
    let board = Board::start();
    let alice = board.enrol("alice");
    let bob = board.enrol("bob");
    let inbox = format!("to/{}", alice.id);
    let task = board.send(None, &["--kind", "task", &inbox, "build the parser"]);
    let from_alice = Some(alice.handover.as_path());
    board.reply(
        from_alice,
        (&inbox, &task),
        &[
            "--kind",
            "report",
            "lead",
            "parser half done\nBranch: parser-wip",
        ],
    );
    let question = board.reply(
        from_alice,
        (&inbox, &task),
        &["questions", "tabs or spaces?"],
    );
    board.reply(
        None,
        ("questions", &question),
        &[&format!("to/{}", alice.id), "spaces"],
    );
    board.send(None, &["--kind", "task", &inbox, "unrelated chore"]);
    board.send(None, &[&inbox, "untraced aside"]);
    let trace = trace_of(&board.message(&inbox, &task.position))
        .unwrap()
        .to_owned();
    Trail {
        board,
        alice,
        bob,
        trace,
        task,
    }
}

fn handoff(trail: &Trail, extra: &[&str]) -> String {
    let position = trail.task.position.as_str();
    let args = ["alice", position, "--to", "bob"];
    trail.board.handoff(&[&args[..], extra].concat())
}

fn bob_inbox(trail: &Trail) -> String {
    format!("to/{}", trail.bob.id)
}

#[test]
fn a_whole_task_handoff_collects_exactly_the_trace_and_keeps_it() {
    let trail = trail();

    let output = handoff(&trail, &[]);

    let sent = trail.board.message(&bob_inbox(&trail), "1");
    let text = body(&sent);
    assert!(output.contains("4 messages"));
    for wanted in [
        "build the parser",
        "parser half done",
        "tabs or spaces?",
        "spaces",
    ] {
        assert!(text.contains(wanted), "{wanted}");
    }
    for unwanted in ["unrelated chore", "untraced aside"] {
        assert!(!text.contains(unwanted), "{unwanted}");
    }
    assert_eq!(trace_of(&sent), Some(trail.trace.as_str()));
    assert_eq!(
        sent["headers"]["reply_to"][2],
        trail.task.position.parse::<u64>().unwrap()
    );
}

#[test]
fn a_whole_task_handoff_summarises_the_trail() {
    let trail = trail();

    handoff(&trail, &[]);

    let text = body(&trail.board.message(&bob_inbox(&trail), "1"));
    assert!(text.contains("- [answered] tabs or spaces?"));
    assert!(text.contains("- Branch: parser-wip"));
}

#[test]
fn a_whole_task_handoff_tells_the_old_agent_to_stand_down_on_the_trace() {
    let trail = trail();
    let inbox = format!("to/{}", trail.alice.id);
    let before = trail.board.head(&inbox);

    let output = handoff(&trail, &[]);

    let note = trail.board.message(&inbox, &before.to_string());
    assert_eq!(note["kind"], "note");
    assert_eq!(trace_of(&note), Some(trail.trace.as_str()));
    assert!(body(&note).contains("Stop work"));
    assert!(output.contains("Stand-down"));
}

#[test]
fn a_handoff_from_a_step_sends_earlier_steps_as_context_only() {
    let trail = trail();

    handoff(&trail, &["--from", "3"]);

    let sent = trail.board.message(&bob_inbox(&trail), "1");
    let text = body(&sent);
    let (done, work) = text.split_once("The work:").unwrap();
    assert!(done.contains("Already done"));
    assert!(done.contains("Step 2:"));
    assert!(work.contains("Step 3:") && work.contains("Step 4:"));
    assert_eq!(trace_of(&sent), Some(trail.trace.as_str()));
}

#[test]
fn a_part_handoff_is_a_task_on_the_same_trace_and_leaves_the_old_agent_working() {
    let trail = trail();
    let inbox = format!("to/{}", trail.alice.id);
    let before = trail.board.head(&inbox);

    handoff(&trail, &["--part", "write the tests"]);

    let sent = trail.board.message(&bob_inbox(&trail), "1");
    assert_eq!(sent["kind"], "task");
    assert!(body(&sent).starts_with("write the tests"));
    assert_eq!(trace_of(&sent), Some(trail.trace.as_str()));
    assert_eq!(trail.board.head(&inbox), before);
}

#[test]
fn a_handoff_of_a_task_that_is_not_there_is_refused() {
    let trail = trail();

    let output = trail.board.recipe(
        "handoff",
        &[
            "alice",
            "99",
            "--to",
            "bob",
            "--config",
            trail.board.config.to_str().unwrap(),
        ],
    );

    assert!(!output.status.success());
}

#[test]
fn a_human_retry_receipt_displays_the_stored_trace() {
    let board = Board::start();
    board.lead(&[
        "send",
        "--id",
        "retry-note",
        "--trace",
        "aBcD",
        "lead",
        "same",
    ]);

    let retry = board.lead(&[
        "send",
        "--id",
        "retry-note",
        "--trace",
        "ef01",
        "lead",
        "same",
    ]);

    assert!(retry.contains("already recorded"));
    assert!(
        retry
            .lines()
            .any(|line| line.contains("Trace") && line.ends_with("aBcD"))
    );
}

#[test]
fn plain_inbox_follow_ups_share_the_workers_trace_in_mailbox() {
    let board = Board::start();
    let worker = board.enrol("worker");
    let inbox = format!("to/{}", worker.id);
    let handover = Some(worker.handover.as_path());
    let original = board.send(None, &["--kind", "task", &inbox, "Write the parser"]);
    let task = board.message(&inbox, &original.position);
    let trace = trace_of(&task).unwrap();
    stdout(board.run(handover, &["read", "--ack", &inbox]));

    board.send(handover, &["--kind", "report", "lead", "Parser started"]);
    let note = board.send(None, &[&inbox, "The input uses tabs"]);
    let follow_up = board.send(None, &["--kind", "task", &inbox, "Cover tabs in tests"]);
    board.send(handover, &["--kind", "report", "lead", "Adding cases"]);

    assert_eq!(
        trace_of(&board.message(&inbox, &note.position)),
        Some(trace)
    );
    assert_eq!(
        trace_of(&board.message(&inbox, &follow_up.position)),
        Some(trace)
    );
    let output = board.lead(&["mailbox", "--trace", trace, "--json"]);
    let messages: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let expected = [
        ("task", "Write the parser"),
        ("report", "Parser started"),
        ("note", "The input uses tabs"),
        ("task", "Cover tabs in tests"),
        ("report", "Adding cases"),
    ];
    assert_eq!(messages.len(), expected.len());
    for (kind, body) in expected {
        assert!(
            messages.iter().any(|message| {
                message["kind"] == kind
                    && message["text"] == body
                    && trace_of(message) == Some(trace)
            }),
            "missing {kind}: {body}"
        );
    }
}

#[test]
fn lane_reports_forward_and_answer_share_the_tasks_trace() {
    let board = Board::start();
    let worker = board.enrol("worker");
    board.enrol("recipient");
    let inbox = format!("to/{}", worker.id);
    let task = board.send(None, &["--kind", "task", &inbox, "Write the parser"]);
    let task_record = board.message(&inbox, &task.position);
    let trace = trace_of(&task_record).unwrap();
    let handover = Some(worker.handover.as_path());
    stdout(board.run(handover, &["read", "--ack", &inbox]));

    let progress = ["First progress", "Second progress", "Third progress"];
    let reports = progress.map(|text| board.send(handover, &["--kind", "report", "lead", text]));
    let config = board.config.to_str().unwrap();
    stdout(board.recipe(
        "forward",
        &[
            "--config",
            config,
            "lead",
            &reports[0].position,
            "recipient",
        ],
    ));
    let mut answer = board
        .recipe_command(
            "answer",
            &["--config", config, "lead", &reports[1].position],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    answer
        .stdin
        .take()
        .unwrap()
        .write_all(b"Proceed with the parser")
        .unwrap();
    stdout(answer.wait_with_output().unwrap());
    board.send(None, &["--trace", "ff", "lead", "Separate work"]);

    let output = board.run(None, &["mailbox", "--trace", trace, "--json"]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("a label any sender can set"));
    let messages: Vec<Value> = stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[0]["text"], "Write the parser");
    for (message, progress) in messages[1..4].iter().zip(progress) {
        assert_eq!(message["text"], progress);
        assert!(message["headers"].get("reply_to").is_none());
    }
    assert!(messages[4]["text"].as_str().unwrap().ends_with(progress[0]));
    assert_eq!(messages[5]["text"], "Proceed with the parser");
    assert_eq!(
        messages[4]["headers"]["reply_to"][2].to_string(),
        reports[0].position
    );
    assert_eq!(
        messages[5]["headers"]["reply_to"][2].to_string(),
        reports[1].position
    );
    assert!(
        messages
            .iter()
            .all(|message| trace_of(message) == Some(trace))
    );
}
