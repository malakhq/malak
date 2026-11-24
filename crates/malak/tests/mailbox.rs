//! Exercise mailbox output against a local board through a pipe and a terminal.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_testkit::unix_socket::SocketDir;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[path = "fixtures/retained.rs"]
mod retained;
#[path = "fixtures/terminal.rs"]
mod terminal;
use terminal::{TerminalOutput, terminal_pair, terminal_text};

const MALAK: &str = env!("CARGO_BIN_EXE_malak");
// An upper bound for a missing event, independent of when the board is expected to respond.
const WATCHDOG: Duration = Duration::from_secs(60);

struct Board {
    process: Running,
    root: SocketDir,
}

impl Board {
    fn start() -> Self {
        Self::start_with("")
    }

    fn start_with(settings: &str) -> Self {
        let root = SocketDir::new().unwrap();
        std::fs::write(
            root.path().join("board.toml"),
            format!(
                "[board]\nstate = {:?}\nsocket_folder = {:?}\n{settings}\n",
                root.path().join("st"),
                root.path().join("d"),
            ),
        )
        .unwrap();
        let board = launch_board(root.path());
        Self {
            process: board,
            root,
        }
    }

    fn restart(&mut self) {
        self.run(&["board", "stop"]);
        let _ = self.process.0.wait();
        self.process = launch_board(self.root.path());
    }

    fn run(&self, words: &[&str]) -> String {
        run(&mut command(self.root.path(), words))
    }

    fn enrol(&self, name: &str) -> (String, PathBuf) {
        let handover = self.root.path().join(name);
        let mut command = command(self.root.path(), &["enrol", "--json", name, "--out"]);
        let output = run(command.arg(&handover));
        let receipt: serde_json::Value = serde_json::from_str(&output).unwrap();
        (receipt["id"].as_str().unwrap().into(), handover)
    }

    fn asker(&self, name: &str) -> (String, PathBuf) {
        let handover = self.root.path().join(name);
        let mut command = command(
            self.root.path(),
            &[
                "enrol",
                "--json",
                name,
                "--grant",
                "questions:send",
                "--out",
            ],
        );
        let output = run(command.arg(&handover));
        let receipt: serde_json::Value = serde_json::from_str(&output).unwrap();
        (receipt["id"].as_str().unwrap().into(), handover)
    }

    fn ask(&self, handover: &Path, body: &str) -> Receipt {
        let mut command = command(self.root.path(), &["send", "--json", "questions", body]);
        Receipt::parse(&run(command.env("MALAK_HANDOVER", handover)))
    }

    fn plain_questions(&self) -> Vec<String> {
        self.run(&["mailbox", "--questions", "--plain"])
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('#'))
            .map(str::to_owned)
            .collect()
    }

    fn ack_as(&self, handover: &Path, channel: &str, receipt: &Receipt) {
        let mut read = command(self.root.path(), &["read", channel]);
        run(read.env("MALAK_HANDOVER", handover));
        let mut ack = command(
            self.root.path(),
            &[
                "ack",
                channel,
                &receipt.position,
                "--creation",
                &receipt.creation,
            ],
        );
        run(ack.env("MALAK_HANDOVER", handover));
    }

    fn send(&self, channel: &str, kind: &str, body: &str) -> Receipt {
        Receipt::parse(&self.run(&["send", "--json", channel, "--kind", kind, body]))
    }

    fn post_upgrade_notice(&self) -> String {
        use malak_core::{LocalClient, OwnerCredential};
        use malak_proto::encode_frame;
        use malak_transport_unix::UnixTransport;
        use std::sync::Arc;

        let status: serde_json::Value =
            serde_json::from_str(&self.run(&["board", "status", "--json"])).unwrap();
        let run = status["run"].as_str().unwrap();
        let installed = "a".repeat(64);
        let credential = OwnerCredential::read(self.root.path().join("st")).unwrap();
        let mut owner = LocalClient::owner(Arc::new(UnixTransport::default()), credential).unwrap();
        let notice = encode_frame(
            1,
            "UPGRADE_NOTICE",
            3,
            |fields| {
                fields.str("run").unwrap().str(run).unwrap();
                fields
                    .str("previous_build")
                    .unwrap()
                    .str("unknown")
                    .unwrap();
                fields
                    .str("installed_build")
                    .unwrap()
                    .str(&installed)
                    .unwrap();
            },
            &[],
        )
        .unwrap();
        owner.owner_call(notice, 1).unwrap();
        run.into()
    }

    fn reply(&self, channel: &str, target: &str, receipt: &Receipt, body: &str) -> Receipt {
        Receipt::parse(&self.run(&[
            "send",
            "--json",
            channel,
            "--reply-to",
            target,
            &receipt.creation,
            &receipt.position,
            body,
        ]))
    }

    fn plain(&self) -> (Running, Receiver<String>) {
        self.plain_as(None)
    }

    fn startup_summary(&self) -> Vec<String> {
        let (_mailbox, lines) = launch_plain(command(self.root.path(), &["mailbox", "--plain"]));
        assert!(
            lines
                .recv_timeout(WATCHDOG)
                .unwrap()
                .starts_with("Mailbox activity")
        );
        (0..5)
            .map(|_| lines.recv_timeout(WATCHDOG).unwrap())
            .collect()
    }

    fn plain_as(&self, handover: Option<&Path>) -> (Running, Receiver<String>) {
        self.plain_with(handover, &[])
    }

    fn plain_with(&self, handover: Option<&Path>, args: &[&str]) -> (Running, Receiver<String>) {
        let mut command = command(self.root.path(), &["mailbox", "--plain"]);
        command.args(args);
        if let Some(handover) = handover {
            command.env("MALAK_HANDOVER", handover);
        }
        let (mailbox, lines) = launch_plain(command);
        // The heading is written after the tail subscription and snapshot.
        line_with(&lines, "Mailbox activity");
        if args != ["--for", "lead"] {
            let marker = self.send("lead", "note", "mailbox-ready");
            block_with(
                &lines,
                &format!("lead sent a note to lead #{}", marker.position),
            );
        }
        (mailbox, lines)
    }

    fn terminal(&self, rows: u16) -> (Running, TerminalOutput) {
        self.terminal_with(rows, 200, &[])
    }

    fn terminal_with(&self, rows: u16, columns: u16, args: &[&str]) -> (Running, TerminalOutput) {
        let (master, slave) = terminal_pair(rows, columns);
        let mailbox = Running(
            command(self.root.path(), &["mailbox"])
                .args(args)
                .env("TERM", "xterm")
                .env("NO_COLOR", "1")
                .stdin(slave.try_clone().unwrap())
                .stdout(slave)
                .spawn()
                .unwrap(),
        );
        (mailbox, TerminalOutput::new(master))
    }
}

fn launch_board(root: &Path) -> Running {
    let mut board = Running(
        command(root, &["board", "start"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(board.0.stdout.take().unwrap());
    let (send, ready) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let _ = send.send(line);
    });
    assert_eq!(ready.recv_timeout(WATCHDOG).unwrap().trim(), "ready");
    board
}

fn launch_plain(mut command: Command) -> (Running, Receiver<String>) {
    let mut mailbox = Running(command.stdout(Stdio::piped()).spawn().unwrap());
    let lines = pipe_lines(mailbox.0.stdout.take().unwrap());
    (mailbox, lines)
}

fn pipe_lines(pipe: impl Read + Send + 'static) -> Receiver<String> {
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

fn command(root: &Path, words: &[&str]) -> Command {
    let mut command = Command::new(MALAK);
    command
        .arg("--config")
        .arg(root.join("board.toml"))
        .env("HOME", root)
        .env("MALAK_HANDOVER", root.join("st/lead.handover"))
        .args(words);
    command
}

fn run(command: &mut Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Receipt {
    creation: String,
    position: String,
}

impl Receipt {
    fn parse(receipt: &str) -> Self {
        let receipt: serde_json::Value = serde_json::from_str(receipt).unwrap();
        Self {
            creation: receipt["creation"].as_str().unwrap().into(),
            position: receipt["position"].to_string(),
        }
    }
}

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl TerminalOutput {
    fn resize(&self, mailbox: &Running, rows: u16, columns: u16) {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        use rustix::termios::{Winsize, tcsetwinsize};

        let size = Winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        tcsetwinsize(&self.input, size).unwrap();
        let pid = Pid::from_raw(i32::try_from(mailbox.0.id()).unwrap());
        kill(pid, Signal::SIGWINCH).unwrap();
    }
}

fn line_with(lines: &Receiver<String>, expected: &str) -> String {
    let deadline = Instant::now() + WATCHDOG;
    loop {
        let line = lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("expected {expected:?}: {error}"));
        if line.contains(expected) {
            return line;
        }
    }
}

fn block_with(lines: &Receiver<String>, expected: &str) -> String {
    use std::fmt::Write as _;

    let deadline = Instant::now() + WATCHDOG;
    let mut block = String::new();
    loop {
        let line = lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("expected message with {expected:?}: {error}"));
        if line.is_empty() {
            if block.contains(expected) {
                return block;
            }
            block.clear();
        } else {
            if !line.starts_with("  ") {
                block.clear();
            }
            writeln!(block, "{line}").unwrap();
        }
    }
}

fn send_from_xargs(board: &Board, folder: &Path, body: &str) {
    let send = command(board.root.path(), &["send", "lead", "--kind", "report"]);
    let mut process = Command::new("xargs");
    process
        .args(["-0", MALAK])
        .args(send.get_args())
        .envs(
            send.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(folder);
    let mut child = Running(
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    input.write_all(body.as_bytes()).unwrap();
    input.write_all(b"\0").unwrap();
    drop(input);
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn plain_mailbox_names_the_upgrade_notice_sender_owner() {
    let board = Board::start();
    board.post_upgrade_notice();

    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));
    let block = block_with(&lines, "Malak upgraded");

    assert!(block.contains("  owner sent a note to lead #1"), "{block}");
    assert!(block.contains("\n  owner sent from "), "{block}");
}

#[test]
fn interactive_trace_names_the_upgrade_notice_sender_owner() {
    let board = Board::start();
    let trace = board.post_upgrade_notice();

    let (_mailbox, mut terminal) = board.terminal_with(30, 120, &["--trace", &trace]);

    terminal.wait_for("@owner → @lead · note #1");
}

#[test]
fn an_upgrade_notice_does_not_set_the_lead_session_baseline() {
    let board = Board::start();
    board.post_upgrade_notice();

    send_from_xargs(&board, board.root.path(), "First lead message");
    let record: serde_json::Value =
        serde_json::from_str(&board.run(&["get", "--json", "lead", "2"])).unwrap();

    assert_eq!(record["sender_trace"]["session"]["program"], "xargs");
    assert!(
        record["sender_trace"]["earlier_session"].is_null(),
        "{record}"
    );
}

#[test]
fn plain_mailbox_flags_a_shared_handover_used_from_another_folder() {
    let board = Board::start();
    let home = board.root.path().canonicalize().unwrap();
    let earlier = home.join("first");
    let other = home.join("other");
    std::fs::create_dir(&earlier).unwrap();
    std::fs::create_dir(&other).unwrap();
    run(command(board.root.path(), &["send", "lead", "original session"]).current_dir(earlier));
    send_from_xargs(&board, &other, "shared handover from another folder");

    let mut mailbox = command(board.root.path(), &["mailbox", "--plain"]);
    mailbox.env("HOME", &home);
    let (_mailbox, lines) = launch_plain(mailbox);
    let block = block_with(&lines, "shared handover from another folder");

    assert!(block.contains("lead sent from xargs "), "{block}");
    assert!(block.contains(" in ~/other (earlier: "), "{block}");
}

#[test]
fn plain_mailbox_flags_different_sessions_in_the_same_folder() {
    let board = Board::start();
    let folder = board.root.path().canonicalize().unwrap();
    let first = run(command(
        board.root.path(),
        &["send", "--json", "lead", "first session"],
    )
    .current_dir(&folder));
    let first = Receipt::parse(&first);
    let status: serde_json::Value =
        serde_json::from_str(&board.run(&["get", "--json", "lead", &first.position])).unwrap();
    assert_eq!(status["sender_trace"]["cwd"], folder.to_str().unwrap());
    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));
    line_with(&lines, "Mailbox activity");
    send_from_xargs(&board, &folder, "another session in the same folder");

    let block = block_with(&lines, "another session in the same folder");

    let previous = &status["sender_trace"]["session"];
    let earlier = format!(
        "(earlier: {} {})",
        previous["program"].as_str().unwrap(),
        previous["pid"]
    );
    assert!(block.contains(&earlier), "{block}");
}

#[test]
fn a_task_in_the_plain_feed_shows_its_full_reply_address() {
    let board = Board::start();
    let (id, _) = board.enrol("reviewer");
    let channel = format!("to/{id}");
    let task = board.send(&channel, "task", "Check the fixture.");

    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));
    let block = block_with(&lines, "Check the fixture.");

    let address = format!("{channel} {} {}", task.creation, task.position);
    assert!(block.contains(&address), "{block}");
}

#[test]
fn a_task_in_the_terminal_feed_shows_its_full_reply_address() {
    let board = Board::start();
    let task = board.send("lead", "task", "Check the fixture.");

    let (mailbox, mut view) = board.terminal(40);
    view.wait_for("Check the fixture.");
    view.press(b"\r");
    view.wait_for("lines 1–1 of 1");
    view.bytes.clear();
    view.resize(&mailbox, 40, 160);

    let address = format!("lead {} {}", task.creation, task.position);
    view.wait_for(&address);
}

#[test]
fn a_same_channel_reply_names_its_position_in_plain_output() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let original = board.send("lead", "task", "original task");
    board.reply("lead", "lead", &original, "same-channel reply");

    let line = block_with(&lines, "    same-channel reply");

    assert!(
        line.contains(&format!("Reply to #{}", original.position)),
        "{line}"
    );
}

#[test]
fn a_cross_channel_reply_names_the_channel_in_plain_output() {
    let board = Board::start();
    board.run(&["channel", "create", "to/worker"]);
    let (_mailbox, lines) = board.plain();
    let original = board.send("to/worker", "task", "original task");
    board.reply("lead", "to/worker", &original, "cross-channel reply");

    let line = block_with(&lines, "    cross-channel reply");

    assert!(
        line.contains(&format!("Reply to #{} to/worker", original.position)),
        "{line}"
    );
}

#[test]
fn a_reply_to_a_note_sent_before_opening_is_marked_answered() {
    let board = Board::start();
    let (id, _) = board.enrol("cli");
    let inbox = format!("to/{id}");
    board.run(&["channel", "create", &inbox]);
    let original = board.send("lead", "note", "plain note without a prefix");
    let (_mailbox, lines) = board.plain();
    let reply = board.reply(&inbox, "lead", &original, "linked answer");

    let block = block_with(&lines, "    linked answer");
    let line = block.lines().next().unwrap();

    assert!(line.contains(&format!("Reply to #{} lead", original.position)));
    assert!(line.ends_with(&format!("Answered by #{} to cli", reply.position)));
}

#[test]
fn an_observed_lead_report_can_be_marked_answered_without_get_access_to_it() {
    let board = Board::start();
    let (id, handover) = board.enrol("cli");
    let inbox = format!("to/{id}");
    board.run(&["channel", "create", &inbox]);
    board.run(&["grant", &id, &format!("{inbox}:get")]);
    let (_mailbox, lines) = board.plain_as(Some(&handover));
    let report = board.send("lead", "report", "report without get permission");
    let reply = board.reply(&inbox, "lead", &report, "linked answer");

    let line = line_with(&lines, "Answered by");

    assert!(line.contains(&format!("Answered by #{} to cli", reply.position)));
}

#[test]
fn a_same_channel_link_to_a_future_position_has_no_reply_marker() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let mut future = board.send("lead", "note", "preceding note");
    future.position = (future.position.parse::<u64>().unwrap() + 2).to_string();
    board.reply("lead", "lead", &future, "forward link");
    board.send("lead", "note", "future position");

    let line = block_with(&lines, "    forward link");

    assert!(!line.contains("Reply to"), "{line}");
}

#[test]
fn a_reference_to_a_retired_creation_does_not_link_to_its_replacement() {
    let board = Board::start();
    let old = board.send("to/old", "note", "original creation");
    board.run(&["channel", "remove", "to/old"]);
    let replacement = board.send("to/old", "note", "replacement creation");
    assert_eq!(old.position, replacement.position);
    assert_ne!(old.creation, replacement.creation);
    let (_mailbox, lines) = board.plain();
    board.reply("lead", "to/old", &old, "retired link");

    let line = block_with(&lines, "    retired link");

    assert!(!line.contains("Reply to"), "{line}");
}

#[test]
fn question_text_alone_does_not_mark_a_message_answered() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let note = board.send("lead", "note", "QUESTION: question without a link");
    board.send("lead", "note", &format!("ANSWER #{}", note.position));
    board.send("lead", "note", "end marker");

    loop {
        let line = lines.recv_timeout(WATCHDOG).unwrap();
        assert!(!line.contains("Answered by"), "{line}");
        if line.contains("end marker") {
            break;
        }
    }
}

#[test]
fn enter_shows_the_full_message_from_the_waiting_tab() {
    let board = Board::start();
    let (id, _) = board.enrol("worker");
    let inbox = format!("to/{id}");
    let body = format!(
        "waiting task preview\n{}\nend of the waiting task",
        "x".repeat(160)
    );
    board.send(&inbox, "task", &body);
    let (_mailbox, mut output) = board.terminal_with(30, 45, &[]);

    output.wait_for("[Activity]");
    output.press(b"4");
    output.wait_for("[Waiting]");
    output.wait_for("waiting task preview");
    assert!(!terminal_text(&output.bytes).contains("endofthewaitingtask"));
    output.press(b"\r");

    output.wait_for("end of the waiting task");
}

#[test]
fn a_plain_body_preserves_a_literal_trailing_ellipsis() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let body = format!("literal-ellipsis {}…", "x".repeat(108));
    board.send("lead", "report", &body);

    let line = line_with(&lines, "literal-ellipsis");

    assert!(line.ends_with(&body), "{line}");
    assert!(!line.contains("malak get"), "{line}");
}

#[test]
fn the_full_reply_address_remains_readable_in_an_eighty_column_terminal() {
    let board = Board::start();
    let (id, _) = board.enrol("cli");
    let inbox = format!("to/{id}");
    let message = board.send(&inbox, "report", "review this");
    let (_mailbox, mut output) = board.terminal_with(30, 80, &[]);

    output.wait_for("review this");
    output.press(b"\r");
    output.wait_for(&format!(
        "{inbox} {} {}",
        message.creation, message.position
    ));
}

#[test]
fn an_expanded_waiting_message_shows_the_reply_reference() {
    let board = Board::start();
    let (id, _) = board.enrol("worker");
    let inbox = format!("to/{id}");
    let original = board.send(&inbox, "task", "waiting task");
    board.reply(&inbox, &inbox, &original, "waiting reply");
    let (mailbox, mut output) = board.terminal(30);

    output.wait_for("[Activity]");
    output.press(b"4");
    output.wait_for("[Waiting]");
    output.wait_for("note #2");
    output.press(b"\r");
    output.wait_for("lines 1–");
    output.bytes.clear();
    output.resize(&mailbox, 31, 200);

    output.wait_for(&format!("Reply to #{}", original.position));
}

#[test]
fn a_live_activity_row_shows_its_preview_without_a_keypress() {
    let board = Board::start();
    board.run(&["channel", "create", "to/alias"]);
    // This store belongs to the selected sender's feed but not its inbox.
    let (_mailbox, mut output) = board.terminal_with(30, 200, &["--for", "lead"]);
    output.wait_for("[Activity]");
    board.send("to/alias", "note", "arrived with its preview");

    output.wait_for("@lead → to/alias");
    output.wait_for("arrived with its preview");
}

#[test]
fn a_live_answer_does_not_clear_the_waiting_for_ack_count() {
    let board = Board::start();
    let (id, _) = board.enrol("cli");
    let inbox = format!("to/{id}");
    board.run(&["channel", "create", &inbox]);
    let original = board.send("lead", "note", "waiting lead note");
    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));
    block_with(&lines, "    waiting lead note");
    let reply = board.reply(&inbox, "lead", &original, "live answer");

    let answered = block_with(&lines, "    live answer");
    assert!(answered.contains(&format!("Answered by #{} to cli", reply.position)));
    let status = board.run(&["board", "status", "--json"]);
    let status: serde_json::Value = serde_json::from_str(&status).unwrap();
    let lead = status["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|channel| channel["name"] == "lead")
        .unwrap();
    assert_eq!(lead["head"], 2);
    assert_eq!(lead["readers"]["lead"].as_u64().unwrap_or(0), 0);
}

#[test]
fn the_waiting_tab_excludes_aliases_like_the_one_shot_list() {
    let board = Board::start();
    board.run(&["channel", "create", "to/m2"]);
    board.send("to/m2", "note", "unread alias message");
    let (_mailbox, mut output) = board.terminal(30);
    output.wait_for("unread alias message");
    output.bytes.clear();
    output.press(b"4");

    output.wait_for("No waiting messages");
    assert!(!terminal_text(&output.bytes).contains("unreadaliasmessage"));
}

#[test]
fn the_waiting_tab_updates_after_a_worker_acknowledges() {
    let board = Board::start();
    let (id, handover) = board.enrol("ada");
    let inbox = format!("to/{id}");
    board.run(&["grant", &id, &format!("{inbox}:read,ack")]);
    let message = board.send(&inbox, "task", "task awaiting acknowledgement");
    let (_mailbox, mut output) = board.terminal(30);
    output.wait_for("[Activity]");
    output.press(b"4");
    output.wait_for("[Waiting]");
    output.wait_for("task awaiting acknowledgement");

    board.ack_as(&handover, &inbox, &message);

    output.wait_for("No waiting messages");
}

#[test]
fn down_moves_to_a_participant_outside_the_initial_view() {
    let board = Board::start();
    for participant in ["alpha", "bravo", "charlie", "delta"] {
        board.enrol(participant);
    }
    let (_mailbox, mut output) = board.terminal_with(18, 45, &[]);
    output.wait_for("[Activity]");
    output.press(b"\t");
    output.wait_for("▶ @lead");

    output.press(&b"\x1b[B".repeat(4));

    output.press(b"\r");
    output.wait_for("Activity · @delta");
}

#[test]
fn a_pipe_uses_the_lead_handover_when_the_environment_is_unset() {
    let board = Board::start();
    board.send("lead", "report", "private report");
    let mut command = command(board.root.path(), &["mailbox"]);
    command.env_remove("MALAK_HANDOVER");
    let (_mailbox, lines) = launch_plain(command);

    let block = block_with(&lines, "    private report");

    assert!(block.starts_with(|character: char| character.is_ascii_digit()));
    assert!(block.contains("lead sent a report to lead"));
}

#[test]
fn multiline_plain_bodies_escape_terminal_controls() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    board.send("lead", "report", "first line\n\u{1b}[31m");

    let block = block_with(&lines, "    first line");

    assert!(
        block.ends_with("    first line\n    \\u{1b}[31m\n"),
        "{block:?}"
    );
    assert!(!block.contains('\u{1b}'));
}

#[test]
fn a_linked_reply_quotes_the_original_above_its_body() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let original = board.send("lead", "note", "QUESTION: May I use it?\nMore context");
    board.reply("lead", "lead", &original, "Yes.");

    let block = block_with(&lines, "    Yes.");
    let (header, body) = block.split_once("  Body\n").unwrap();

    assert!(
        header
            .lines()
            .next()
            .unwrap()
            .contains(&format!("Reply to #{}", original.position))
    );
    let quote = header.lines().last().unwrap();
    assert!(quote.starts_with(&format!("  Quote  lead #{} note ", original.position)));
    assert!(quote.ends_with("QUESTION: May I use it?…"));
    assert_eq!(body, "    Yes.\n");
}

#[test]
fn history_and_live_messages_share_one_ordered_plain_feed() {
    let board = Board::start();
    board.send("lead", "note", "removed by trim");
    let first = board.send("to/beta", "note", "first retained message");
    board.send("to/alpha", "note", "second retained message");
    board.run(&["trim", "lead", "2"]);
    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));
    assert!(
        lines
            .recv_timeout(WATCHDOG)
            .unwrap()
            .starts_with("Mailbox activity")
    );
    line_with(
        &lines,
        "Acknowledgements and refusals shown since this view opened.",
    );
    board.send("lead", "note", "live after opening");

    let mut bodies = Vec::new();
    loop {
        let line = lines.recv_timeout(WATCHDOG).unwrap();
        if let Some(body) = line.strip_prefix("    ") {
            bodies.push(body.to_owned());
            if body == "live after opening" {
                break;
            }
        }
    }
    assert_eq!(
        bodies,
        [
            "first retained message",
            "second retained message",
            "live after opening"
        ]
    );
    assert_eq!(first.position, "1");
}

#[test]
fn history_only_loads_channels_the_viewer_can_get() {
    let board = Board::start();
    let (id, handover) = board.enrol("cli");
    let inbox = format!("to/{id}");
    board.run(&["grant", &id, &format!("{inbox}:get")]);
    board.send("lead", "report", "private earlier report");
    board.send(&inbox, "note", "visible earlier note");
    let mut command = command(board.root.path(), &["mailbox", "--plain"]);
    command.env("MALAK_HANDOVER", handover);
    let (_mailbox, lines) = launch_plain(command);

    loop {
        let line = lines.recv_timeout(WATCHDOG).unwrap();
        assert!(!line.contains("private earlier report"));
        if line == "    visible earlier note" {
            break;
        }
    }
}

#[test]
fn a_read_or_ack_is_printed_on_one_line_without_a_body() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let message = board.send("lead", "note", "separate body");
    board.run(&["read", "lead"]);
    board.run(&[
        "ack",
        "lead",
        &message.position,
        "--creation",
        &message.creation,
    ]);
    board.send("lead", "note", "after ack");

    let read = line_with(&lines, &format!("lead read lead #{}", message.position));
    let ack = lines.recv_timeout(WATCHDOG).unwrap();
    let next = lines.recv_timeout(WATCHDOG).unwrap();

    assert_eq!(
        &read[8..],
        format!("  lead read lead #{}", message.position)
    );
    assert_eq!(
        &ack[8..],
        format!("  lead acknowledged lead through #{}", message.position)
    );
    assert!(next.contains("lead sent a note to lead"), "{next:?}");
}

#[test]
fn live_activity_updates_the_message_as_it_is_read_and_acknowledged() {
    let board = Board::start();
    let message = board.send("lead", "note", "receipt status body");
    let (_mailbox, mut terminal) = board.terminal_with(24, 45, &[]);
    terminal.wait_for(&format!("note #{}  waiting", message.position));

    board.run(&["read", "lead"]);
    // The 'a' may stay in the same cell as it was in "waiting".
    terminal.wait_for_output("read now", |bytes| {
        let output = terminal_text(bytes);
        output.contains("readnow") || output.contains("rednow")
    });

    board.run(&[
        "ack",
        "lead",
        &message.position,
        "--creation",
        &message.creation,
    ]);
    terminal.wait_for("acknowledge");
}

#[test]
fn a_stopped_board_stays_visible_until_the_operator_quits() {
    let board = Board::start();
    board.send("lead", "note", "retained before stop");
    let (mailbox, mut terminal) = board.terminal_with(24, 45, &[]);
    terminal.wait_for("retained before stop");

    stop_and_repaint(&board, &mailbox, &mut terminal);
    terminal.wait_for("retained before stop");
    quit_stopped_mailbox(mailbox, terminal, b"q");
}

#[test]
fn a_stop_during_loading_shows_partial_history() {
    let board = Board::start();
    board.send("orders", "note", "retained order");
    // Hold preview handshakes without blocking the real owner door or its tail.
    let lead_socket = board.root.path().join("d/l/s");
    std::fs::rename(&lead_socket, lead_socket.with_file_name("held")).unwrap();
    let _preview_gate = UnixListener::bind(lead_socket).unwrap();
    let (mailbox, mut terminal) = board.terminal_with(24, 45, &[]);
    terminal.wait_for("orders loading…");

    stop_and_repaint(&board, &mailbox, &mut terminal);
    terminal.wait_for("PARTICIPANTS");
    terminal.wait_for("history partial");
    terminal.wait_for("orders unavailable");
    assert!(!terminal_text(&terminal.bytes).contains("loading"));
    quit_stopped_mailbox(mailbox, terminal, b"\x1b");
}

fn stop_and_repaint(board: &Board, mailbox: &Running, terminal: &mut TerminalOutput) {
    board.run(&["board", "stop"]);
    terminal.wait_for("Owner STOP");
    // A normal terminal diff can omit cells shared by the old and new headers.
    terminal.bytes.clear();
    terminal.resize(mailbox, 24, 80);
    terminal.wait_for("board stopped");
    terminal.wait_for("Owner STOP");
}

fn quit_stopped_mailbox(mut mailbox: Running, mut terminal: TerminalOutput, key: &[u8]) {
    assert!(mailbox.0.try_wait().unwrap().is_none());
    terminal.press(key);
    terminal.wait_for_output("leave alternate screen", |bytes| {
        bytes.windows(8).any(|part| part == b"\x1b[?1049l")
    });
    assert!(mailbox.0.wait().unwrap().success());
}

fn multiline_body(count: usize) -> String {
    (1..=count)
        .map(|number| format!("body-row-{number:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn plain_feed_prints_every_body_line_even_past_the_terminal_fold_limit() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain();
    let body = multiline_body(45);
    board.send("lead", "note", &body);

    let block = block_with(&lines, "    body-row-45");
    let (_, rendered_body) = block.split_once("  Body\n").unwrap();

    assert_eq!(
        rendered_body.lines().collect::<Vec<_>>(),
        body.lines()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn enter_expands_a_retained_activity_body() {
    let board = Board::start();
    board.send("lead", "note", &multiline_body(45));
    let (_mailbox, mut output) = board.terminal(80);
    output.wait_for("body-row-01");

    output.press(b"\r");

    output.wait_for("body-row-45");
}

#[test]
fn c_emits_the_terminal_clipboard_sequence_for_an_expanded_message() {
    let board = Board::start();
    board.send("lead", "note", "copy this message's address");
    let (_mailbox, mut output) = board.terminal(30);
    output.wait_for("copy this message");
    output.press(b"\r");
    output.wait_for("lines 1–1 of 1");

    output.press(b"c");

    output.wait_for("\x1b]52;c;");
}

#[test]
fn replies_already_stored_at_opening_do_not_mark_notes_answered() {
    let board = Board::start();
    let note = board.send("lead", "note", "already retained note");
    board.reply("lead", "lead", &note, "already retained reply");
    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));

    let original = block_with(&lines, "    already retained note");
    let reply = block_with(&lines, "    already retained reply");

    assert!(!original.contains("Answered by"));
    assert!(!reply.contains("Answered by"));
}

#[test]
fn the_terminal_feed_scrolls_back_to_earlier_retained_bodies() {
    let board = Board::start();
    board.send("lead", "note", "older retained body");
    board.send("lead", "note", &multiline_body(30));
    let (_mailbox, mut output) = board.terminal(18);
    output.wait_for("body-row-01");

    output.press(b"\x1b[B");

    output.wait_for("retained body");
}

#[test]
fn opening_loads_past_the_old_feed_limit() {
    let board = Board::start();
    retained::seed(&board.root.path().join("st/lead.handover"), "lead", 251);
    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));
    assert!(
        lines
            .recv_timeout(WATCHDOG)
            .expect("the plain mailbox did not print its startup heading")
            .starts_with("Mailbox activity")
    );
    board.send("lead", "note", "after retained history");

    let deadline = Instant::now() + WATCHDOG;
    let mut retained = 0;
    loop {
        let line = lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| panic!("only saw {retained} retained messages: {error}"));
        if line.starts_with("    retained-") {
            retained += 1;
            assert_eq!(line, format!("    retained-{retained:03}"));
        } else if line == "    after retained history" {
            break;
        }
    }
    assert_eq!(retained, 251);
}

#[test]
fn activity_folds_separate_rate_limit_events_on_a_loaded_board() {
    let board = Board::start_with(r#"persistence = "sqlite""#);
    retained::seed(&board.root.path().join("st/lead.handover"), "lead", 2_100);
    let (id, handover) = board.enrol("reader");
    board.run(&["grant", &id, "lead:get"]);
    let (mailbox, mut terminal) = board.terminal_with(40, 140, &[]);
    terminal.wait_for("retained-2100");

    let mut reader = retained::connect(&handover);
    for rid in 2..602 {
        assert_eq!(retained::get(&mut reader, rid, "lead"), "OK");
    }
    for rid in 602..604 {
        assert_eq!(retained::get(&mut reader, rid, "lead"), "ERR");
    }
    terminal.wait_for("(2 calls)");
    board.run(&["channel", "create", "between-batches"]);
    terminal.wait_for("between-batches");

    for rid in 604..607 {
        assert_eq!(retained::get(&mut reader, rid, "lead"), "ERR");
    }
    terminal.wait_for_output("the next rate-limit batch", |bytes| {
        let text = terminal_text(bytes);
        text.contains("(3calls)") || text.contains("(5calls)")
    });
    terminal.bytes.clear();
    terminal.resize(&mailbox, 41, 140);
    terminal.wait_for_output("the redrawn Activity rows", |bytes| {
        terminal_text(bytes)
            .rsplit_once("┌─Malak")
            .is_some_and(|(_, screen)| screen.contains("between-batches"))
    });

    let screen = terminal_text(&terminal.bytes);
    let (_, screen) = screen.rsplit_once("┌─Malak").unwrap();
    assert_eq!(
        screen.matches("participantcallrateexceeded").count(),
        1,
        "{screen}"
    );
    assert_in_order(screen, &["(5calls)", "between-batches"]);
}

#[test]
fn a_refusal_is_printed_on_one_line_without_a_body() {
    let board = Board::start();
    let (_, handover) = board.enrol("worker");
    let (_mailbox, lines) = board.plain();
    let refused = command(board.root.path(), &["send", "lead", "refused body"])
        .env("MALAK_HANDOVER", handover)
        .output()
        .unwrap();
    assert!(!refused.status.success());

    let line = line_with(&lines, "send refused for worker in lead");
    board.send("lead", "note", "after refusal");
    let next = lines.recv_timeout(WATCHDOG).unwrap();

    assert!(line.contains("send refused"));
    assert!(!line.contains("refused body"));
    assert!(next.contains("lead sent a note to lead"), "{next:?}");
}

#[test]
fn questions_lists_unanswered_questions_oldest_first_with_asker_and_first_line() {
    let board = Board::start();
    board.run(&["channel", "create", "questions"]);
    let (_, handover) = board.asker("alpha");
    board.ask(&handover, "first?\nmore detail");
    board.ask(&handover, "second?");

    let lines = board.plain_questions();

    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("#1") && lines[0].ends_with("alpha  first?"),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with("#2") && lines[1].ends_with("alpha  second?"),
        "{lines:?}"
    );
}

#[test]
fn an_owner_question_stays_open_until_the_lead_replies_to_it() {
    let board = Board::start();
    let unrelated = board.send("lead", "note", "A different message");
    let question = board.send("questions", "note", "Ready to release?");
    board.reply("questions", "lead", &unrelated, "An unrelated answer");

    let lines = board.plain_questions();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with("Ready to release?"), "{lines:?}");

    board.reply(
        "questions",
        "questions",
        &question,
        "Owner, in chat at 17:46: \"Yes.\"",
    );

    assert!(board.plain_questions().is_empty());
}

#[test]
fn a_lead_reply_on_another_channel_leaves_an_owner_question_open() {
    let board = Board::start();
    let question = board.send("questions", "note", "Ready to release?");

    board.reply("lead", "questions", &question, "Yes.");

    let lines = board.plain_questions();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with("Ready to release?"), "{lines:?}");
}

#[test]
fn a_lead_reply_to_a_future_position_leaves_an_owner_question_open() {
    let board = Board::start();
    let first = board.send("questions", "note", "Keep the first question open?");
    let future = Receipt {
        creation: first.creation,
        position: "3".into(),
    };

    board.reply("questions", "questions", &future, "A reply sent too early");
    let posted = board.send("questions", "note", "Ready to release?");

    assert_eq!(posted.position, future.position);
    let lines = board.plain_questions();
    assert_eq!(lines.len(), 2);
    assert!(lines[1].ends_with("Ready to release?"), "{lines:?}");
}

#[test]
fn a_lead_reply_to_an_unavailable_message_is_not_an_owner_question() {
    let board = Board::start();
    let question = board.send("questions", "note", "Ready to release?");
    board.run(&["channel", "remove", "questions"]);
    board.run(&["channel", "create", "questions"]);

    board.reply(
        "questions",
        "questions",
        &question,
        "A reply to the old question",
    );

    assert!(board.plain_questions().is_empty());
}

fn board_with_owner_and_agent_questions() -> Board {
    let board = Board::start();
    let (_, handover) = board.asker("alpha");
    board.ask(&handover, "Which file?");
    board.send("questions", "note", "Ready to release?");
    board
}

#[test]
fn the_mailbox_opens_with_all_five_standard_channels_on_an_empty_board() {
    let board = Board::start();

    assert_eq!(
        board.startup_summary(),
        [
            "lead 0 waiting",
            "questions 0 open",
            "orders 0 in force",
            "inboxes 0 waiting",
            "relay none",
        ]
    );
}

#[test]
fn inbox_summary_uses_each_recipients_ack_and_folds_extra_names() {
    let board = Board::start();
    for name in ["delta", "charlie", "bravo", "alpha"] {
        let (id, handover) = board.enrol(name);
        let inbox = format!("to/{id}");
        board.run(&["grant", &id, &format!("{inbox}:read,ack")]);
        let first = board.send(&inbox, "task", "First");
        let second = board.send(&inbox, "task", "Second");
        board.ack_as(&handover, &inbox, &first);
        board.ack_as(&board.root.path().join("st/lead.handover"), &inbox, &second);
    }

    assert_eq!(
        board.startup_summary()[3],
        "inboxes 4 waiting (alpha, bravo, charlie, …)"
    );
}

#[test]
fn the_inbox_summary_leaves_out_revoked_recipients() {
    let board = Board::start();
    let (active, _) = board.enrol("active");
    let (revoked, _) = board.enrol("revoked");
    board.send(&format!("to/{active}"), "task", "Keep this waiting.");
    board.send(
        &format!("to/{revoked}"),
        "task",
        "Left in the revoked inbox.",
    );
    board.run(&["revoke", &revoked]);

    assert_eq!(board.startup_summary()[3], "inboxes 1 waiting (active)");
}

#[test]
fn the_leads_plain_monitor_streams_worker_questions() {
    let board = Board::start();
    let (_, worker) = board.asker("alpha");
    let (_mailbox, lines) = board.plain_with(None, &["--for", "lead"]);

    board.ask(&worker, "Question for the lead?");

    let block = block_with(&lines, "alpha sent a note to questions #1");
    assert!(block.contains("Question for the lead?"), "{block}");
}

#[test]
fn the_leads_plain_monitor_names_a_worker_enrolled_after_startup() {
    let board = Board::start();
    let (_mailbox, lines) = board.plain_with(None, &["--for", "lead"]);
    let (id, handover) = board.enrol("reviewer");
    board.run(&["grant", &id, "lead:send"]);
    let inbox = format!("to/{id}");
    let original = board.send(&inbox, "task", "Name the recipient.");
    run(command(
        board.root.path(),
        &[
            "send",
            "--kind",
            "report",
            "--reply-to",
            &inbox,
            &original.creation,
            &original.position,
            "lead",
            "Name the sender.",
        ],
    )
    .env("MALAK_HANDOVER", handover));

    let task = block_with(&lines, "Name the recipient.");
    assert!(task.contains("lead sent a task to reviewer #1"), "{task}");
    let report = block_with(&lines, "Name the sender.");
    assert!(
        report.contains("reviewer sent a report to lead #1  Reply to #1 reviewer"),
        "{report}"
    );
}

#[test]
fn the_leads_plain_monitor_uses_an_id_for_a_reused_name() {
    let board = Board::start();
    let (old, handover) = board.enrol("reviewer");
    board.run(&["revoke", &old]);
    std::fs::remove_file(handover).unwrap();
    let (id, _) = board.enrol("reviewer");
    let inbox = format!("to/{id}");
    let (_mailbox, lines) = board.plain_with(None, &["--for", "lead"]);
    board.send(&inbox, "task", "Distinguish the new reviewer.");

    let task = block_with(&lines, "Distinguish the new reviewer.");
    assert!(
        task.contains(&format!("lead sent a task to {inbox} #1")),
        "{task}"
    );
}

#[test]
fn the_leads_plain_monitor_omits_the_leads_own_echoes() {
    let board = Board::start();
    board.send("lead", "note", "Retained setup note.");
    let (_mailbox, lines) = board.plain_with(None, &["--for", "lead"]);
    let note = board.send("lead", "note", "Live setup note.");
    board.ack_as(&board.root.path().join("st/lead.handover"), "lead", &note);
    board.send("orders", "note", "The lead can still post to orders.");

    loop {
        let line = lines.recv_timeout(WATCHDOG).unwrap();
        for echo in ["lead sent a note to lead", "lead read", "lead acknowledged"] {
            assert!(!line.contains(echo), "{line}");
        }
        if line.contains("lead sent a note to orders #1") {
            break;
        }
    }
}

#[test]
fn the_leads_plain_monitor_streams_orders_from_any_sender() {
    let board = Board::start();
    let (id, handover) = board.enrol("operator");
    board.run(&["grant", &id, "orders:send"]);
    let (_mailbox, lines) = board.plain_with(None, &["--for", "lead"]);

    let lead_order = "Choose which check to run.";
    board.post_order(None, lead_order);
    let other_order = "Run the fixture check first.";
    run(command(board.root.path(), &["send", "orders", other_order])
        .env("MALAK_HANDOVER", handover));

    assert!(block_with(&lines, "lead sent a note to orders #1").contains(lead_order));
    assert!(block_with(&lines, "operator sent a note to orders #2").contains(other_order));
}

fn waiting_json(board: &Board, args: &[&str]) -> serde_json::Value {
    let words = ["mailbox", "--waiting", "--json"];
    serde_json::from_str(&board.run(&[words.as_slice(), args].concat())).unwrap()
}

#[test]
fn waiting_json_is_empty_without_waiting_inboxes() {
    assert_eq!(
        waiting_json(&Board::start(), &[]),
        serde_json::json!({"waiting": []})
    );
}

#[test]
fn waiting_json_groups_unacknowledged_lead_messages_by_inbox() {
    let board = Board::start();
    let (alpha, handover) = board.enrol("alpha");
    let (bravo, _) = board.enrol("bravo");
    let alpha = format!("to/{alpha}");
    let bravo = format!("to/{bravo}");
    board.run(&[
        "grant",
        alpha.strip_prefix("to/").unwrap(),
        &format!("{alpha}:send,read,ack"),
    ]);
    let acked = board.send(&alpha, "task", "Already acknowledged.");
    board.ack_as(&handover, &alpha, &acked);
    board.send(&alpha, "note", "Waiting.");
    board.send(&alpha, "task", "Also waiting.");
    run(
        command(board.root.path(), &["send", &alpha, "From the worker."])
            .env("MALAK_HANDOVER", handover),
    );
    board.send(&bravo, "task", "First for bravo.");

    assert_eq!(
        waiting_json(&board, &[]),
        serde_json::json!({"waiting": [
            {"participant": "alpha", "inbox": alpha, "messages": [
                {"position": 2, "kind": "note"},
                {"position": 3, "kind": "task"}
            ]},
            {"participant": "bravo", "inbox": bravo, "messages": [{"position": 1, "kind": "task"}]}
        ]})
    );
}

#[test]
fn waiting_can_select_one_participant() {
    let board = Board::start();
    for name in ["alpha", "bravo"] {
        let (id, _) = board.enrol(name);
        board.send(&format!("to/{id}"), "task", "Waiting.");
    }

    let entries = waiting_json(&board, &["--for", "bravo"]);
    let entries = entries["waiting"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["participant"], "bravo");
}

#[test]
fn the_live_header_counts_the_same_worker_inboxes_as_waiting_json() {
    let board = Board::start();
    let (active, _) = board.enrol("active");
    let (retired, _) = board.enrol("retired");
    for id in [&active, &retired] {
        let inbox = format!("to/{id}");
        board.send(&inbox, "note", "First.");
        board.send(&inbox, "task", "Second.");
    }
    board.run(&["revoke", &retired]);

    let (_mailbox, mut terminal) = board.terminal(30);
    terminal.wait_for("inboxes 1 waiting");
    terminal.wait_for("[Activity]");

    let reviewed = waiting_json(&board, &[]);
    assert_eq!(reviewed["waiting"].as_array().unwrap().len(), 1);
    assert_eq!(reviewed["waiting"][0]["participant"], "active");
}

#[test]
fn a_live_revocation_moves_the_participant_into_the_archive() {
    let board = Board::start();
    let (worker, _) = board.enrol("lane");
    let (_mailbox, mut terminal) = board.terminal(30);
    terminal.wait_for("@lane");
    assert!(!terminal_text(&terminal.bytes).contains("Archived"));

    board.run(&["revoke", &worker]);

    terminal.wait_for("▸ Archived (1)");
}

#[test]
fn revoking_a_worker_clears_the_waiting_entry_and_keeps_history() {
    let board = Board::start();
    let (id, _) = board.enrol("alpha");
    let inbox = format!("to/{id}");
    let retained = board.send(&inbox, "task", "Retained body.");

    board.run(&["revoke", &id]);

    assert_eq!(
        waiting_json(&board, &[]),
        serde_json::json!({"waiting": []})
    );
    assert!(
        board
            .run(&["get", &inbox, &retained.position])
            .contains("Retained body.")
    );
}

#[test]
fn waiting_pipes_one_unboxed_line_per_inbox() {
    let board = Board::start();
    let (id, _) = board.enrol("alpha");
    let inbox = format!("to/{id}");
    board.send(&inbox, "task", "Private body.");
    board.send(&inbox, "note", "Another body.");

    assert_eq!(
        board.run(&["mailbox", "--waiting"]),
        format!("alpha  {inbox}  #2 note, #1 task\n")
    );
}

#[test]
fn human_waiting_lists_the_most_recently_updated_inbox_first() {
    let board = Board::start();
    let (alpha, _) = board.enrol("alpha");
    let (zulu, _) = board.enrol("zulu");
    board.send(&format!("to/{alpha}"), "task", "Older");
    board.send(&format!("to/{zulu}"), "task", "Newer");

    let output = board.run(&["mailbox", "--waiting"]);
    assert_in_order(
        &output,
        &[&format!("zulu  to/{zulu}"), &format!("alpha  to/{alpha}")],
    );
}

#[test]
fn waiting_boxes_an_inbox_on_a_terminal() {
    let board = Board::start();
    let (id, _) = board.enrol("alpha");
    let inbox = format!("to/{id}");
    board.send(&inbox, "task", "Private body.");

    let (mut mailbox, mut terminal) = board.terminal_with(30, 80, &["--waiting"]);
    terminal.wait_for("┌");
    terminal.wait_for(&format!("alpha {inbox}"));
    terminal.wait_for("#1 task");
    terminal.wait_for("└");
    assert!(mailbox.0.wait().unwrap().success());
}

#[test]
fn waiting_conflicts_with_plain_questions_and_orders() {
    for incompatible in ["--plain", "--questions", "--orders"] {
        let output = Command::new(MALAK)
            .args(["mailbox", "--waiting", incompatible])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{incompatible}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--waiting"));
    }
}

#[test]
fn the_question_summary_counts_unanswered_questions_for_the_owner() {
    let board = board_with_owner_and_agent_questions();
    let answered = board.send("questions", "note", "Already answered?");
    board.reply("questions", "questions", &answered, "Yes.");

    assert_eq!(
        board.startup_summary()[1],
        "questions 2 open (1 for the owner)"
    );
}

#[test]
fn the_order_summary_counts_only_orders_still_in_force() {
    let board = Board::start();
    board.post_order(Some("2000-01-01T00:00:00Z"), "Ended.");
    board.post_order(None, "In force.");

    assert_eq!(board.startup_summary()[2], "orders 1 in force");
}

#[test]
fn the_terminal_pins_the_summary_above_questions_on_a_narrow_screen() {
    let board = board_with_owner_and_agent_questions();

    let (_mailbox, mut output) = board.terminal_with(30, 30, &[]);
    output.wait_for("relay –");
    output.wait_for("2 (1 you)");
    output.wait_for("1 for you");
    let view = terminal_text(&output.bytes);
    assert!(
        view.find("2(1you)").unwrap() < view.find("1foryou").unwrap(),
        "{view}"
    );
}

#[test]
fn the_live_mailbox_disables_wrapping_inside_the_alternate_screen() {
    let board = Board::start();
    let (mut mailbox, mut output) = board.terminal_with(24, 45, &[]);
    output.wait_for("[Activity]");
    output.press(b"q");
    assert!(mailbox.0.wait().unwrap().success());
    for chunk in &output.chunks {
        output.bytes.extend(chunk);
    }

    let position = |sequence: &[u8]| {
        output
            .bytes
            .windows(sequence.len())
            .position(|part| part == sequence)
            .unwrap_or_else(|| panic!("missing terminal sequence {sequence:?}"))
    };
    assert!(position(b"\x1b[?1049h") < position(b"\x1b[?7l"));
    assert!(position(b"\x1b[?7l") < position(b"\x1b[?7h"));
    assert!(position(b"\x1b[?7h") < position(b"\x1b[?1049l"));
}

#[test]
fn a_mouse_wheel_moves_the_live_selection_inside_the_alternate_screen() {
    let board = Board::start();
    board.enrol("alpha");
    let (mut mailbox, mut terminal) = board.terminal_with(30, 80, &[]);
    terminal.wait_for("[Activity]");
    terminal.press(b"\t");
    terminal.wait_for("▶ @lead");

    terminal.press(b"\x1b[<65;5;8M");
    terminal.wait_for("▶ @alpha");
    terminal.press(b"\x1b[<64;5;8M\r");
    terminal.wait_for("Activity · @lead");
    terminal.press(b"q");
    assert!(mailbox.0.wait().unwrap().success());
    for chunk in &terminal.chunks {
        terminal.bytes.extend(chunk);
    }

    let output = String::from_utf8_lossy(&terminal.bytes);
    let enter = output.find("\x1b[?1049h").unwrap();
    let enable = output.find("\x1b[?1000h").unwrap();
    let disable = output.find("\x1b[?1000l").unwrap();
    let exit = output.find("\x1b[?1049l").unwrap();
    assert!(enter < enable && enable < disable && disable < exit);
    assert!(!output[..enter].contains("PARTICIPANTS"));
    assert!(!output[exit..].contains("PARTICIPANTS"));
}

#[test]
fn questions_lists_owner_questions_above_agent_questions() {
    let board = board_with_owner_and_agent_questions();

    let view = board.run(&["mailbox", "--questions"]);

    let heading = view.find("For the owner").unwrap();
    let owner = view.find("Ready to release?").unwrap();
    let agent = view.find("Which file?").unwrap();
    assert!(heading < owner && owner < agent, "{view}");
}

#[test]
fn questions_json_identifies_owner_and_agent_questions() {
    let board = board_with_owner_and_agent_questions();

    let json = board.run(&["mailbox", "--questions", "--json"]);
    let questions: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();

    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0]["kind"], "owner");
    assert_eq!(questions[0]["first_line"], "Ready to release?");
    assert_eq!(questions[1]["kind"], "agent");
    assert_eq!(questions[1]["first_line"], "Which file?");
}

#[test]
fn human_questions_list_newest_first_within_each_group() {
    let board = Board::start();
    let (_, handover) = board.asker("alpha");
    board.send("questions", "note", "Older owner question");
    board.send("questions", "note", "Newer owner question");
    board.ask(&handover, "Older agent question");
    board.ask(&handover, "Newer agent question");

    let output = board.run(&["mailbox", "--questions"]);
    assert_in_order(
        &output,
        &[
            "Newer owner question",
            "Older owner question",
            "Newer agent question",
            "Older agent question",
        ],
    );
}

#[test]
fn mailbox_highlights_only_the_owner_question_below_the_box() {
    let board = board_with_owner_and_agent_questions();
    let (_mailbox, mut output) = board.terminal_with(30, 200, &["--for", "lead"]);

    output.wait_for("1 for you · #2 Ready to release?");
    let view = terminal_text(&output.bytes);
    assert!(view.contains("2(1you)"), "{view}");
    assert!(!view.contains("Whichfile?"), "{view}");
}

#[test]
fn a_lead_reply_in_the_askers_inbox_answers_a_question() {
    let board = Board::start();
    board.run(&["channel", "create", "questions"]);
    let (id, handover) = board.asker("alpha");
    let answered = board.ask(&handover, "answered?");
    board.ask(&handover, "still open?");

    board.reply(&format!("to/{id}"), "questions", &answered, "yes");

    let lines = board.plain_questions();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with("still open?"), "{lines:?}");
}

#[test]
fn a_reply_from_another_participant_does_not_answer_a_question() {
    let board = Board::start();
    board.run(&["channel", "create", "questions"]);
    let (_, handover) = board.asker("alpha");
    let question = board.ask(&handover, "open?");
    let mut command = command(
        board.root.path(),
        &[
            "send",
            "questions",
            "--reply-to",
            "questions",
            &question.creation,
            &question.position,
            "me too",
        ],
    );
    run(command.env("MALAK_HANDOVER", &handover));

    let lines = board.plain_questions();
    assert!(lines[0].ends_with("open?"), "{lines:?}");
}

#[test]
fn an_answered_question_stays_answered_after_the_board_restarts() {
    let mut board = Board::start();
    board.run(&["channel", "create", "questions"]);
    let (_, handover) = board.asker("alpha");
    let answered = board.ask(&handover, "answered?");
    board.reply("questions", "questions", &answered, "yes");
    board.ask(&handover, "still open?");

    board.restart();

    let lines = board.plain_questions();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].ends_with("still open?"), "{lines:?}");
}

#[test]
fn questions_works_with_plain() {
    let board = Board::start();
    board.run(&["channel", "create", "questions"]);
    let (_, handover) = board.asker("alpha");
    board.ask(&handover, "plain?");

    assert_eq!(board.plain_questions().len(), 1);
}

#[test]
fn a_child_mailbox_refuses_its_parents_handover() {
    let parent = Board::start();
    let child = Board::start();
    let (_, worker) = child.asker("alpha");
    child.ask(&worker, "child question?");

    let result = command(child.root.path(), &["mailbox", "--questions"])
        .env(
            "MALAK_HANDOVER",
            parent.root.path().join("st/lead.handover"),
        )
        .output()
        .unwrap();
    let diagnostic = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(diagnostic.contains("handover") && diagnostic.contains("does not match"));
    assert!(result.stdout.is_empty());
    assert!(
        child
            .run(&["mailbox", "--questions"])
            .contains("child question?")
    );
}

#[test]
fn mailbox_rejects_orders_with_json_and_points_to_questions() {
    let board = Board::start();

    let result = command(board.root.path(), &["mailbox", "--orders", "--json"])
        .output()
        .unwrap();
    let diagnostic = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(diagnostic.contains("--questions --json"), "{diagnostic}");
    assert!(result.stdout.is_empty());
}

#[test]
fn questions_show_each_item_in_its_own_box() {
    let board = board_with_owner_and_agent_questions();

    let view = board.run(&["mailbox", "--questions"]);

    assert_eq!(view.matches('┌').count(), 2, "{view}");
    assert_eq!(view.matches('└').count(), 2, "{view}");
}

#[test]
fn a_question_box_shows_its_full_reply_address() {
    let board = Board::start();
    let (_, handover) = board.asker("reviewer");
    let question = board.ask(&handover, "Which fixture?");

    let view = board.run(&["mailbox", "--questions"]);

    let address = format!("questions {} {}", question.creation, question.position);
    assert!(view.contains(&address), "{view}");
}

#[test]
fn questions_wrap_long_words_and_show_multiple_body_lines_on_a_narrow_terminal() {
    let board = Board::start();
    board.send(
        "questions",
        "note",
        "界界界界界界界界界界界界界界界界界界\nSecond line\nThird line\nHidden",
    );

    let (_mailbox, mut output) = board.terminal_with(30, 26, &["--questions"]);
    output.wait_for("└");
    let raw = String::from_utf8_lossy(&output.bytes);
    let rows = raw.lines().filter(|line| line.starts_with('│'));

    for row in rows {
        assert_eq!(ratatui::text::Line::from(row).width(), 26, "{row:?}");
    }
    assert!(raw.contains("│ 界界界界界界界界界界界 │"), "{raw}");
    assert!(
        raw.contains("Second line") && raw.contains("Third line"),
        "{raw}"
    );
    assert!(raw.contains("…") && !raw.contains("Hidden"), "{raw}");
}

#[test]
fn questions_shows_an_empty_state_when_the_board_has_no_questions_channel() {
    let board = Board::start();

    board.run(&["channel", "remove", "questions"]);

    assert_eq!(
        board.run(&["mailbox", "--questions"]),
        "No open questions.\n"
    );
}

impl Board {
    fn plain_orders(&self) -> Vec<String> {
        run(command(self.root.path(), &["mailbox", "--orders", "--plain"]).env("TZ", "UTC"))
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn post_order(&self, until: Option<&str>, scope: &str) -> Receipt {
        let body = until.map_or_else(
            || scope.to_owned(),
            |until| format!("Until: {until}\n\n{scope}"),
        );
        self.send("orders", "note", &body)
    }

    fn decide_under(&self, order: &Receipt, decision: &str) -> Receipt {
        let body = format!(
            "Under order: {} {}\n\n{decision}",
            order.creation, order.position
        );
        self.send("lead", "note", &body)
    }
}

#[test]
fn orders_lists_an_order_in_force_with_its_end_time() {
    let board = Board::start();
    board.run(&["channel", "create", "orders"]);
    board.post_order(Some("2999-01-01T00:00:00Z"), "Pick between test fixtures.");

    let lines = board.plain_orders();

    assert!(lines[1].starts_with("order #1  in force  until 2999-01-01 00:00 +00:00"));
    assert_eq!(lines[2], "  Scope  Pick between test fixtures.");
}

#[test]
fn orders_lists_an_ended_order_with_the_decisions_made_under_it() {
    let board = Board::start();
    board.run(&["channel", "create", "orders"]);
    let ended = board.post_order(Some("2000-01-01T00:00:00Z"), "Pick between test fixtures.");
    let current = board.post_order(Some("2999-01-01T00:00:00Z"), "Rename internal helpers.");
    board.decide_under(&ended, "Use the shared fixture.");
    board.decide_under(&current, "Call it `load`.");
    board.send("lead", "note", "Not under any order.");

    let lines = board.plain_orders();

    assert_eq!(lines[0], "Orders");
    assert!(lines[1].starts_with("order #1  ended  until "));
    assert_eq!(
        &lines[2..5],
        [
            "  Scope  Pick between test fixtures.",
            "  Decision  lead #1",
            "            Use the shared fixture."
        ]
    );
    assert!(lines[5].starts_with("order #2  in force  until "));
    assert_eq!(
        &lines[6..],
        [
            "  Scope  Rename internal helpers.",
            "  Decision  lead #2",
            "            Call it `load`."
        ]
    );
}

#[test]
fn an_order_with_no_end_time_is_in_force_just_after_it_is_posted() {
    let board = Board::start();
    board.run(&["channel", "create", "orders"]);
    board.post_order(None, "Pick between test fixtures.");

    let lines = board.plain_orders();

    assert!(
        lines[1].starts_with("order #1  in force  until "),
        "{lines:?}"
    );
}

#[test]
fn orders_survive_a_restart_with_their_decisions() {
    let mut board = Board::start();
    board.run(&["channel", "create", "orders"]);
    let order = board.post_order(Some("2000-01-01T00:00:00Z"), "Pick between test fixtures.");
    board.decide_under(&order, "Use the shared fixture.");

    board.restart();

    assert!(
        board
            .plain_orders()
            .contains(&"            Use the shared fixture.".to_owned())
    );
}

#[test]
fn orders_box_shows_the_poster_and_associated_decision() {
    let board = Board::start();
    let order = board.post_order(None, "Pick between test fixtures.\nUse the shared setup.");
    board.decide_under(&order, "Use the shared fixture.");

    let view = board.run(&["mailbox", "--orders"]);

    assert!(
        view.contains("│ Order #1 · lead · ") && view.contains(" ago "),
        "{view}"
    );
    assert!(view.contains("│ in force · until "), "{view}");
    assert!(view.contains("│ Use the shared setup."), "{view}");
    assert!(view.contains("│ Decision  lead #1"), "{view}");
}

#[test]
fn an_order_box_shows_its_full_reply_address() {
    let board = Board::start();
    let order = board.post_order(None, "Choose the fixture.");

    let view = board.run(&["mailbox", "--orders"]);

    let address = format!("orders {} {}", order.creation, order.position);
    assert!(view.contains(&address), "{view}");
}

#[test]
fn a_decision_in_an_order_box_shows_its_full_reply_address() {
    let board = Board::start();
    let (id, _) = board.enrol("reviewer");
    let channel = format!("to/{id}");
    let order = board.post_order(None, "Choose the fixture.");
    let body = format!(
        "Under order: {} {}\n\nUse the shared fixture.",
        order.creation, order.position
    );
    let decision = board.send(&channel, "note", &body);

    let view = board.run(&["mailbox", "--orders"]);

    let address = format!("{channel} {} {}", decision.creation, decision.position);
    assert!(view.contains(&address), "{view}");
}

#[test]
fn an_order_box_names_a_decisions_inbox_recipient() {
    let board = Board::start();
    let (id, _) = board.enrol("reviewer");
    let order = board.post_order(None, "Choose the fixture.");
    let channel = format!("to/{id}");
    let body = format!(
        "Under order: {} {}\n\nUse the shared fixture.",
        order.creation, order.position
    );
    board.send(&channel, "note", &body);

    let view = board.run(&["mailbox", "--orders"]);

    assert!(view.contains("│ Decision  reviewer #1"), "{view}");
}

#[test]
fn an_order_box_shortens_an_inbox_id_when_the_recipient_is_unknown() {
    let board = Board::start();
    let order = board.post_order(None, "Choose the fixture.");
    let body = format!(
        "Under order: {} {}\n\nUse the shared fixture.",
        order.creation, order.position
    );
    board.send("to/0123456789abcdef0123456789abcdef", "note", &body);

    let view = board.run(&["mailbox", "--orders"]);

    assert!(view.contains("│ Decision  01234567... #1"), "{view}");
}

#[test]
fn an_order_box_wraps_the_whole_first_scope_line() {
    let board = Board::start();
    let scope = "Keep the shared fixture and verify that the archived sample still represents the prior release before applying any changes to the journal fixtures.";
    board.post_order(Some("2999-01-01T00:00:00Z"), scope);

    let view = board.run(&["mailbox", "--orders"]);
    let rows = view
        .lines()
        .filter_map(|line| line.strip_prefix("│ "))
        .map(|line| line.trim_end_matches('│').trim_end())
        .collect::<Vec<_>>();

    assert!(rows.join(" ").contains(scope), "{view}");
}

#[test]
fn a_quote_truncates_after_a_whole_word() {
    let board = Board::start();
    let original = board.send(
        "lead",
        "note",
        "Keep the existing report format and preserve everyone's acknowledgement cursor.",
    );
    board.reply("lead", "lead", &original, "Agreed.");
    let (_mailbox, lines) = launch_plain(command(board.root.path(), &["mailbox", "--plain"]));

    let quote = line_with(&lines, "  Quote");

    assert!(quote.ends_with("preserve everyone's…"), "{quote}");
}

#[test]
fn orders_shows_an_empty_state_without_an_item_box() {
    let board = Board::start();

    assert_eq!(board.run(&["mailbox", "--orders"]), "No orders recorded.\n");
}

#[test]
fn human_orders_list_newest_first() {
    let board = Board::start();
    board.post_order(None, "Older order");
    board.post_order(None, "Newer order");

    let output = board.run(&["mailbox", "--orders"]);
    assert_in_order(&output, &["Newer order", "Older order"]);
}

fn assert_in_order(text: &str, expected: &[&str]) {
    let mut rest = text;
    for needle in expected {
        rest = rest
            .split_once(needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {text}"))
            .1;
    }
}

#[test]
fn a_mark_from_another_creation_of_orders_is_not_a_decision() {
    let board = Board::start();
    board.run(&["channel", "create", "orders"]);
    board.post_order(Some("2999-01-01T00:00:00Z"), "Pick between test fixtures.");
    board.send("lead", "note", "Under order: other-creation 1\n\nStale.");

    assert!(
        !board
            .plain_orders()
            .iter()
            .any(|line| line.contains("Decision"))
    );
}

const TRACE: &str = "C0ffee";

#[test]
fn activity_trace_opens_the_same_retained_and_live_conversation_at_each_supported_width() {
    for width in [160, 120, 100, 80, 45] {
        let board = Board::start();
        trace_conversation(&board);
        let (mailbox, mut terminal) = board.terminal_with(60, width, &[]);
        terminal.wait_for("unrelated body");
        terminal.press(b"\x1b[B");
        terminal.wait_for("t trace");

        terminal.press(b"t");
        terminal.wait_for("Esc return");
        terminal.bytes.clear();
        terminal.resize(&mailbox, 61, width);
        let title = format!("Trace{TRACE}");
        terminal.wait_for_output("oldest and newest rows in the trace", |bytes| {
            terminal_text(bytes)
                .split_once(&title)
                .is_some_and(|(_, trace)| {
                    trace.contains("traceworkerreport") && trace.contains("tracetask")
                })
        });
        let output = terminal_text(&terminal.bytes);
        let (_, output) = output.split_once(&title).unwrap();
        assert_in_order(
            output,
            &[
                "traceworkerreport",
                "traceanswer",
                "traceforwardedcopy",
                "tracetask",
            ],
        );

        terminal.bytes.clear();
        board.run(&["send", "lead", "--trace", "b0", "unrelated live body"]);
        board.run(&["send", "lead", "--trace", TRACE, "ARRIVED TRACE ENTRY"]);
        terminal.wait_for("ARRIVED TRACE ENTRY");
        terminal.bytes.clear();
        terminal.resize(&mailbox, 62, width);
        terminal.wait_for("trace task");
        let screen = terminal_text(&terminal.bytes);
        assert_in_order(
            &screen,
            &["ARRIVEDTRACEENTRY", "traceworkerreport", "tracetask"],
        );
        assert_eq!(screen.matches("ARRIVEDTRACEENTRY").count(), 1);
        assert!(!screen.contains("unrelatedlivebody"));
    }
}

#[test]
fn activity_opens_a_trace_when_all_owner_connections_are_in_use() {
    use malak_core::{LocalClient, OwnerCredential};
    use malak_transport_unix::UnixTransport;
    use std::sync::Arc;

    let board = Board::start();
    board.run(&[
        "send",
        "lead",
        "--trace",
        TRACE,
        "retained under owner pressure",
    ]);
    let (_mailbox, mut terminal) = board.terminal_with(35, 120, &[]);
    terminal.wait_for("retained under owner pressure");
    let open = || {
        let credential = OwnerCredential::read(board.root.path().join("st")).unwrap();
        LocalClient::owner(Arc::new(UnixTransport::default()), credential)
    };
    // The mailbox holds two of the CLI board's eight owner connections.
    let _held = (0..6).map(|_| open().unwrap()).collect::<Vec<_>>();
    assert!(open().is_err());

    terminal.wait_for("t trace");
    terminal.bytes.clear();
    terminal.press(b"t");
    terminal.wait_for_output("retained body in the selected trace", |bytes| {
        terminal_text(bytes)
            .split_once("TraceC0ffee")
            .is_some_and(|(_, trace)| trace.contains("retainedunderownerpressure"))
    });
}

#[test]
fn escaping_a_trace_restores_the_selected_message_after_live_inserts() {
    for width in [160, 120, 100, 80, 45] {
        let board = Board::start();
        board.run(&["send", "lead", "--trace", TRACE, "selected trace body"]);
        let (_mailbox, mut terminal) = board.terminal_with(40, width, &[]);
        terminal.wait_for("selected trace body");
        terminal.wait_for("t trace");
        terminal.press(b"t");
        terminal.wait_for("Esc return");
        board.run(&["send", "lead", "newer activity outside the trace"]);

        terminal.bytes.clear();
        terminal.press(b"\x1b");
        terminal.wait_for("t trace");
        terminal.press(b"y");
        terminal.wait_for_output("complete trace ID copied", |bytes| {
            bytes
                .windows(b"\x1b]52;c;QzBmZmVl\x07".len())
                .any(|part| part == b"\x1b]52;c;QzBmZmVl\x07")
        });
    }
}

fn trace_conversation(board: &Board) -> Receipt {
    let (id, handover) = board.enrol("reporter");
    let inbox = format!("to/{id}");
    board.run(&["grant", &id, "lead:send", &format!("{inbox}:get")]);
    let task = Receipt::parse(&board.run(&[
        "send",
        "--json",
        &inbox,
        "--kind",
        "task",
        "--trace",
        TRACE,
        "trace task",
    ]));
    let forwarded = board.reply(&inbox, &inbox, &task, "trace forwarded copy");
    board.reply("questions", &inbox, &forwarded, "trace answer");
    run(command(
        board.root.path(),
        &[
            "send",
            "lead",
            "--kind",
            "report",
            "--reply-to",
            &inbox,
            &forwarded.creation,
            &forwarded.position,
            "trace worker report",
        ],
    )
    .env("MALAK_HANDOVER", handover));
    board.run(&["send", "lead", "--trace", "b0", "unrelated body"]);
    forwarded
}

#[test]
fn trace_in_a_terminal_shows_retained_messages_then_follows_the_conversation() {
    let board = Board::start();
    let parent = trace_conversation(&board);
    board.enrol("outside-trace");
    let (mut mailbox, mut terminal) = board.terminal_with(40, 120, &["--trace", TRACE]);

    terminal.wait_for("[Trace]");
    terminal.wait_for("Board-generated, inherited by replies, or sender-supplied.");
    terminal.wait_for("trace task");
    let retained = terminal_text(&terminal.bytes);
    terminal.press(b"\r");
    terminal.wait_for("lines 1–");
    terminal.bytes.clear();
    terminal.resize(&mailbox, 41, 120);
    terminal.wait_for(&format!("Reply to #{}", parent.position));

    terminal.press(b"g");
    board.run(&["send", "lead", "--trace", "b0", "unrelated live body"]);
    board.run(&["send", "lead", "--trace", TRACE, "TRACE LIVE REPORT"]);
    terminal.wait_for("TRACE LIVE REPORT");
    terminal.press(b"q");
    assert!(mailbox.0.wait().unwrap().success());

    let screen = terminal_text(&terminal.bytes);
    assert_in_order(
        &retained,
        &[
            "traceworkerreport",
            "traceanswer",
            "traceforwardedcopy",
            "tracetask",
        ],
    );
    assert!(
        screen.contains(&format!("Replyto#{}", parent.position)),
        "{screen}"
    );
    assert!(retained.contains("@reporter→@lead·report#1"), "{retained}");
    assert!(!screen.contains("unrelated"), "{screen}");
    assert!(!retained.contains("@outside-trace"), "{retained}");
    assert!(screen.contains("TRACELIVEREPORT"), "{screen}");
}

#[test]
fn plain_trace_prints_history_and_stores_during_startup_once() {
    let board = Board::start();
    let parent = trace_conversation(&board);
    let mut cmd = command(board.root.path(), &["mailbox", "--plain", "--trace", TRACE]);
    cmd.stderr(Stdio::piped());
    let (mut mailbox, lines) = launch_plain(cmd);
    let notices = pipe_lines(mailbox.0.stderr.take().unwrap());
    line_with(&notices, "a label any sender can set");
    board.run(&["send", "lead", "--trace", TRACE, "during trace startup"]);
    board.run(&["send", "lead", "--trace", TRACE, "trace end marker"]);

    let mut bodies = Vec::new();
    let mut parent_seen = false;
    loop {
        let line = lines.recv_timeout(WATCHDOG).unwrap();
        parent_seen |= line.contains(&format!("Reply to #{} to/", parent.position))
            && line.contains(&parent.creation);
        if let Some(body) = line.strip_prefix("    ") {
            if body == "trace end marker" {
                break;
            }
            bodies.push(body.to_owned());
        }
    }
    assert_eq!(
        bodies,
        [
            "trace task",
            "trace forwarded copy",
            "trace answer",
            "trace worker report",
            "during trace startup",
        ]
    );
    assert!(parent_seen);
}

#[test]
fn trace_json_exits_after_printing_the_matching_read_message_objects() {
    let board = Board::start();
    trace_conversation(&board);
    let output = command(board.root.path(), &["mailbox", "--trace", TRACE, "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.matches("a label any sender can set").count(), 1);
    let messages = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        messages
            .iter()
            .map(|m| m["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "trace task",
            "trace forwarded copy",
            "trace answer",
            "trace worker report"
        ]
    );
    for message in messages {
        let expected = board.run(&[
            "get",
            "--json",
            message["channel"].as_str().unwrap(),
            &message["position"].to_string(),
        ]);
        assert_eq!(
            message,
            serde_json::from_str::<serde_json::Value>(&expected).unwrap()
        );
    }
}

#[test]
fn trace_json_uses_the_message_schema_for_non_utf8_bodies() {
    let board = Board::start();
    let mut sender = command(board.root.path(), &["send", "lead", "--trace", TRACE, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    sender.stdin.take().unwrap().write_all(&[0, 255]).unwrap();
    assert!(sender.wait().unwrap().success());

    let message = board.run(&["mailbox", "--trace", TRACE, "--json"]);
    let message: serde_json::Value = serde_json::from_str(&message).unwrap();
    assert_eq!(message["body"], serde_json::json!([0, 255]));
    assert!(message.get("text").is_none());
}

#[test]
fn trace_history_includes_acknowledged_messages_the_viewer_can_get() {
    let board = Board::start();
    let (id, handover) = board.enrol("reader");
    let inbox = format!("to/{id}");
    let message = Receipt::parse(&board.run(&[
        "send",
        "--json",
        &inbox,
        "--trace",
        TRACE,
        "acknowledged trace message",
    ]));
    board.run(&["send", "lead", "--trace", TRACE, "unreadable trace message"]);
    board.run(&["grant", &id, &format!("{inbox}:read,ack,get")]);
    board.ack_as(&handover, &inbox, &message);
    board.run(&["grant", &id, &format!("{inbox}:get")]);

    let output = run(
        command(board.root.path(), &["mailbox", "--trace", TRACE, "--json"])
            .env("MALAK_HANDOVER", handover),
    );
    let message: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(message["text"], "acknowledged trace message");
}

#[test]
fn trace_rejects_incompatible_mailbox_options() {
    for extra in [
        &["--plain", "--json"][..],
        &["--for", "lead"],
        &["--who"],
        &["--questions"],
        &["--orders"],
        &["--waiting"],
    ] {
        let output = Command::new(MALAK)
            .args(["mailbox", "--trace", TRACE])
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{extra:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--trace"));
    }
}

#[test]
fn trace_rejects_ids_outside_the_hex_header_format() {
    for id in ["", "g", "ｆ", &"a".repeat(65)] {
        let output = Command::new(MALAK)
            .args(["mailbox", "--trace", id])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{id:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--trace"));
    }
}

#[test]
fn an_unknown_trace_prints_no_json_and_says_none_were_found() {
    let board = Board::start();
    board.run(&["send", "lead", "--trace", "c0ffee", "different case"]);

    let output = command(board.root.path(), &["mailbox", "--trace", TRACE, "--json"])
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("No messages found on this trace"));
}

#[test]
fn an_empty_live_trace_waits_for_the_first_matching_message() {
    let board = Board::start();
    let (_mailbox, mut terminal) = board.terminal_with(24, 45, &["--trace", TRACE]);
    terminal.wait_for("for a message…");

    board.run(&["send", "lead", "--trace", TRACE, "first trace message"]);

    terminal.wait_for("first trace message");
}

#[test]
fn the_questions_tab_contains_only_open_questions() {
    let board = board_with_owner_and_agent_questions();
    let dealt_with = board.send("questions", "note", "already answered marker");
    board.reply("questions", "questions", &dealt_with, "answer marker");
    let (_mailbox, mut output) = board.terminal_with(30, 120, &[]);
    output.wait_for("[Activity]");
    output.bytes.clear();
    output.press(b"2");

    output.wait_for("[Questions]");
    output.wait_for("owner question");
    output.wait_for("question #2");
    assert!(!terminal_text(&output.bytes).contains("alreadyansweredmarker"));
}

#[test]
fn the_questions_tab_adds_new_questions_at_the_top_of_their_group() {
    let board = Board::start();
    let (_, handover) = board.asker("alpha");
    board.send("questions", "note", "Existing owner question");
    board.ask(&handover, "Older agent question");
    let (mailbox, mut terminal) = board.terminal_with(30, 120, &[]);
    terminal.wait_for("[Activity]");
    terminal.press(b"2");
    terminal.wait_for("[Questions]");
    terminal.wait_for("Older agent question");

    board.ask(&handover, "Newer agent question");
    terminal.wait_for("Newer agent question");
    terminal.bytes.clear();
    terminal.resize(&mailbox, 31, 120);
    terminal.wait_for("Older agent question");

    let screen = terminal_text(&terminal.bytes);
    let tab = screen.split_once("[Questions]").unwrap().1;
    assert_in_order(
        tab,
        &[
            "Existingownerquestion",
            "Neweragentquestion",
            "Olderagentquestion",
        ],
    );
    assert_eq!(tab.matches("Neweragentquestion").count(), 1);
}

#[test]
fn the_waiting_tab_adds_a_new_item_above_older_items_from_other_inboxes() {
    let board = Board::start();
    let (alpha, _) = board.enrol("alpha");
    let (zulu, _) = board.enrol("zulu");
    board.send(&format!("to/{alpha}"), "task", "Older waiting item");
    let (mailbox, mut terminal) = board.terminal_with(30, 120, &[]);
    terminal.wait_for("[Activity]");
    terminal.press(b"4");
    terminal.wait_for("[Waiting]");
    terminal.wait_for("Older waiting item");

    board.send(&format!("to/{zulu}"), "task", "FRESH WAITING TASK");
    terminal.wait_for("FRESH WAITING TASK");
    terminal.bytes.clear();
    terminal.resize(&mailbox, 31, 120);
    terminal.wait_for("Older waiting item");

    let screen = terminal_text(&terminal.bytes);
    assert_in_order(&screen, &["FRESHWAITINGTASK", "Olderwaitingitem"]);
    assert_eq!(screen.matches("FRESHWAITINGTASK").count(), 1);
}

#[test]
fn the_orders_tab_adds_new_orders_above_earlier_decisions_and_orders() {
    let board = Board::start();
    let order = board.post_order(None, "Review the scope");
    board.decide_under(&order, "Ship the decision");
    let (mailbox, mut output) = board.terminal_with(30, 120, &[]);
    output.wait_for("[Activity]");
    output.press(b"3");

    output.wait_for("[Orders]");
    output.wait_for("Ship the decision");
    board.post_order(None, "Newest order scope");
    output.wait_for("Newest order scope");
    output.bytes.clear();
    output.resize(&mailbox, 31, 120);
    output.wait_for("Review the scope");
    let screen = terminal_text(&output.bytes);
    assert_in_order(
        &screen,
        &["Newestorderscope", "Shipthedecision", "Reviewthescope"],
    );
    assert_eq!(screen.matches("Newestorderscope").count(), 1);
}

#[path = "mailbox/multiboard.rs"]
mod multiboard;
