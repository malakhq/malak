//! Exchange messages through the installed command's three Unix entrances.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_testkit::unix_socket::SocketDir;
use serde_json::Value;
use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const MALAK: &str = env!("CARGO_BIN_EXE_malak");

#[path = "command_line/board_lifecycle.rs"]
mod board_lifecycle;
#[path = "command_line/board_version.rs"]
mod board_version;
#[path = "command_line/defaults.rs"]
mod defaults;
#[path = "command_line/detach.rs"]
mod detach;
#[path = "command_line/help.rs"]
mod help;
#[path = "command_line/hook.rs"]
mod hook;
#[path = "command_line/output.rs"]
mod output;
#[path = "command_line/owner_scope.rs"]
mod owner_scope;
#[path = "command_line/read_ack.rs"]
mod read_ack;
#[path = "command_line/tail_shutdown.rs"]
mod tail_shutdown;

struct Fixture {
    board: Child,
    config: PathBuf,
    socket: SocketDir,
}

impl Fixture {
    fn new() -> Self {
        Self::start(launch)
    }

    fn start(launch: impl FnOnce(&Path) -> Child) -> Self {
        Self::with_store("markdown", launch)
    }

    fn with_store(persistence: &str, launch: impl FnOnce(&Path) -> Child) -> Self {
        let socket = SocketDir::new().unwrap();
        let config = write_config(socket.path(), persistence);
        let board = launch(&config);
        let mut fixture = Self {
            board,
            config,
            socket,
        };
        fixture.wait_until_ready();
        fixture
    }

    fn restart(&mut self) {
        self.board.kill().unwrap();
        self.board.wait().unwrap();
        self.board = launch(&self.config);
        self.wait_until_ready();
    }

    fn wait_until_ready(&mut self) {
        let stdout = self.board.stdout.take().unwrap();
        let line = BufReader::new(stdout).lines().next().transpose().unwrap();
        assert_eq!(line.as_deref(), Some("ready"));
    }

    fn lead(&self) -> PathBuf {
        self.socket.path().join("st/lead.handover")
    }

    fn owner_command(&self, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        command
            .arg("--config")
            .arg(&self.config)
            .env("HOME", self.socket.path())
            .args(words);
        command
    }

    fn without_handover(&self, words: &[&str]) -> Command {
        let mut command = self.owner_command(words);
        command.env_remove("MALAK_HANDOVER");
        command
    }

    fn status(&self) -> Value {
        serde_json::from_str(&run(&mut self.owner_command(&["board", "status", "--json"]))).unwrap()
    }

    fn enrol(&self, name: &str) -> (String, PathBuf) {
        let path = self.socket.path().join(name);
        let output = run(Command::new(MALAK)
            .args(["enrol", name, "--json", "--config"])
            .arg(&self.config)
            .arg("--out")
            .arg(&path));
        let receipt: Value = serde_json::from_str(&output).unwrap();
        let id = receipt["id"].as_str().unwrap();
        let inbox = format!("to/{id}");
        run(Command::new(MALAK)
            .args([
                "grant",
                id,
                "lead:send",
                &format!("{inbox}:read,get,ack"),
                "--config",
            ])
            .arg(&self.config));
        (inbox, path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.board.kill();
        let _ = self.board.wait();
    }
}

fn launch(config: &Path) -> Child {
    spawn_board(Command::new(MALAK), config)
}

fn launch_with_one_general_place(config: &Path) -> Child {
    // The reserve leaves two lead places and one general place under this budget.
    launch_with_descriptor_limits(config, "120", "120")
}

fn launch_with_descriptor_limits(config: &Path, soft: &str, hard: &str) -> Child {
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "ulimit -Sn \"$1\" && ulimit -Hn \"$2\" && shift 2 && exec \"$@\"",
        "malak-test",
        soft,
        hard,
        MALAK,
    ]);
    spawn_board(command, config)
}

fn spawn_board(mut command: Command, config: &Path) -> Child {
    if !command.get_envs().any(|(name, _)| name == "HOME") {
        command.env("HOME", config.parent().unwrap());
    }
    command
        .args(["board", "start", "--config"])
        .arg(config)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn write_config(root: &Path, persistence: &str) -> PathBuf {
    write_config_with_socket(root, persistence, &root.join("d"))
}

fn write_config_with_socket(root: &Path, persistence: &str, sockets: &Path) -> PathBuf {
    let config = root.join("board.toml");
    std::fs::write(
        &config,
        format!(
            "[board]\nstate = {:?}\nsocket_folder = {sockets:?}\npersistence = {persistence:?}\n",
            root.join("st"),
        ),
    )
    .unwrap();
    config
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

fn call(path: &Path, words: &[&str]) -> String {
    run(Command::new(MALAK).args(words).env("MALAK_HANDOVER", path))
}

fn messages(output: &str) -> Vec<Value> {
    output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record.get("channels").is_none())
        .collect()
}

#[test]
fn a_lead_and_two_participants_exchange_messages_using_only_the_cli() {
    let board = Fixture::new();
    let (alice_inbox, alice) = board.enrol("alice");
    let (bob_inbox, bob) = board.enrol("bob");

    for (name, inbox, handover) in [
        ("alice", alice_inbox.as_str(), alice.as_path()),
        ("bob", bob_inbox.as_str(), bob.as_path()),
    ] {
        run(&mut board.without_handover(&["send", inbox, "--kind", "task", name]));
        let read = call(handover, &["read", inbox, "--json"]);
        let task = &messages(&read)[0];
        assert_eq!(task["sender"], "lead");
        assert_eq!(task["kind"], "task");
        assert_eq!(task["grade"], "proven");
        assert_eq!(task["body"], serde_json::json!(name.as_bytes()));
        let position = task["position"].as_u64().unwrap().to_string();
        call(
            handover,
            &[
                "ack",
                inbox,
                &position,
                "--creation",
                task["creation"].as_str().unwrap(),
            ],
        );
        call(handover, &["send", "lead", "--kind", "report", name]);
    }

    let read = run(&mut board.without_handover(&["read", "lead", "--json"]));
    let reports = messages(&read);
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["sender_name"], "alice");
    assert_eq!(reports[1]["sender_name"], "bob");
    assert!(reports.iter().all(|report| report["kind"] == "report"));
}

#[test]
fn an_explicit_participant_handover_takes_precedence_over_the_lead_fallback() {
    let board = Fixture::new();
    let (_, alice) = board.enrol("alice");
    let (bob_inbox, _) = board.enrol("bob");
    let output = board
        .owner_command(&["send", &bob_inbox, "misrouted"])
        .env("MALAK_HANDOVER", alice)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(messages(&call(&board.lead(), &["read", "--json", &bob_inbox])).is_empty());
}

#[test]
fn a_foreign_handover_cannot_write_to_either_board_with_an_explicit_config() {
    let destination = Fixture::new();
    let foreign = Fixture::new();

    let result = destination
        .owner_command(&["send", "lead", "wrong-board"])
        .env("MALAK_HANDOVER", foreign.lead())
        .output()
        .unwrap();
    let diagnostic = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(diagnostic.contains(destination.socket.path().join("d").to_str().unwrap()));
    assert!(diagnostic.contains(foreign.lead().to_str().unwrap()));
    assert!(diagnostic.contains("MALAK_HANDOVER"));
    for board in [&destination, &foreign] {
        let output = call(&board.lead(), &["read", "--json", "lead"]);
        assert!(messages(&output).is_empty());
    }
}

#[test]
fn stopped_sqlite_board_reports_that_recovery_is_unsupported() {
    let mut board = Fixture::with_store("sqlite", launch);
    call(&board.lead(), &["send", "lead", "persisted"]);
    run(&mut board.owner_command(&["board", "stop"]));
    assert!(board.board.wait().unwrap().success());

    let result = board.owner_command(&["recover"]).output().unwrap();
    let diagnostic = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(4), "{diagnostic}");
    assert!(
        diagnostic.contains("unsupported for SQLite"),
        "{diagnostic}"
    );
}

#[test]
fn version_flags_print_the_crate_version() {
    let expected = concat!("malak ", env!("CARGO_PKG_VERSION"), "\n");
    for flag in ["--version", "-V"] {
        assert_eq!(run(Command::new(MALAK).arg(flag)), expected, "{flag}");
    }
}

#[test]
fn release_help_lists_the_version_flags() {
    let help = run(Command::new(MALAK).arg("--help"));

    assert!(
        help.lines()
            .any(|line| line.trim_start().starts_with("-V, --version")),
        "{help}"
    );
}

#[test]
fn release_help_points_lead_chats_to_setup() {
    let help = run(Command::new(MALAK).arg("--help"));

    assert!(help.contains("malak recipe lead-up --help"), "{help}");
}

#[test]
fn recipe_long_help_includes_the_agent_setup_guide() {
    let help = run(Command::new(MALAK).args(["recipe", "--help"]));

    assert!(help.contains(include_str!("../agent-setup.md").trim_end()));
}

#[test]
fn the_tool_server_is_absent_from_release_help() {
    let help = run(Command::new(MALAK).arg("--help"));

    assert!(!help.contains("mcp"), "{help}");
}

#[test]
fn the_tool_server_refuses_invocation_until_v02() {
    let board = Fixture::new();
    let result = Command::new(MALAK)
        .arg("mcp")
        .env("MALAK_HANDOVER", board.lead())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let diagnostic = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(4), "{diagnostic}");
    assert!(diagnostic.contains("ships in v0.2"), "{diagnostic}");
    assert!(result.stdout.is_empty());
}

#[test]
fn help_for_the_deferred_tool_server_reports_its_release() {
    for args in [["mcp", "--help"], ["help", "mcp"]] {
        let result = Command::new(MALAK).args(args).output().unwrap();
        let diagnostic = String::from_utf8_lossy(&result.stderr);

        assert_eq!(result.status.code(), Some(4), "{args:?}: {result:?}");
        assert!(diagnostic.contains("ships in v0.2"), "{diagnostic}");
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn a_missing_state_folder_keeps_the_unset_handover_error() {
    let board = Fixture::new();
    let (inbox, _) = board.enrol("alice");
    let config = board.socket.path().join("unreachable.toml");
    std::fs::write(
        &config,
        format!(
            "[board]\nstate = {:?}\nsocket_folder = {:?}\n",
            board.socket.path().join("no-state"),
            board.socket.path().join("d"),
        ),
    )
    .unwrap();
    let output = Command::new(MALAK)
        .arg("--config")
        .arg(config)
        .args(["send", &inbox, "misrouted"])
        .env_remove("MALAK_HANDOVER")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Reason  MALAK_HANDOVER is unset"));
}

#[test]
#[cfg(target_os = "macos")]
fn a_sandboxed_participant_cannot_use_the_lead_fallback() {
    let board = Fixture::new();
    let (inbox, _) = board.enrol("alice");
    let state = board.socket.path().join("st");
    let policy = format!("(version 1)(allow default)(deny file-read* (subpath {state:?}))");
    let output = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &policy, MALAK, "--config"])
        .arg(&board.config)
        .args(["send", &inbox, "misrouted"])
        .env_remove("MALAK_HANDOVER")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Reason  MALAK_HANDOVER is unset"));
}

#[test]
fn the_lead_can_get_and_ack_using_the_state_handover() {
    let board = Fixture::new();
    call(&board.lead(), &["send", "lead", "message"]);
    let read = call(&board.lead(), &["read", "lead", "--json"]);
    let message = &messages(&read)[0];

    let got = run(&mut board.without_handover(&["get", "lead", "1", "--json"]));
    assert_eq!(messages(&got)[0]["id"], message["id"]);
    run(&mut board.without_handover(&[
        "ack",
        "lead",
        "1",
        "--creation",
        message["creation"].as_str().unwrap(),
    ]));
    assert!(messages(&call(&board.lead(), &["read", "--json", "lead"])).is_empty());
}

fn read_and_get_json(body: &[u8]) -> [Value; 2] {
    use std::io::Write;

    let board = Fixture::new();
    let lead = board.lead();
    let mut sender = Command::new(MALAK)
        .args(["send", "lead", "-"])
        .env("MALAK_HANDOVER", &lead)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    sender.stdin.take().unwrap().write_all(body).unwrap();
    assert!(sender.wait().unwrap().success());
    [
        messages(&call(&lead, &["read", "lead", "--json"])).remove(0),
        messages(&call(&lead, &["get", "lead", "1", "--json"])).remove(0),
    ]
}

#[test]
fn json_message_includes_text_for_utf8() {
    let text = "Hello, 🐈\n";
    for message in read_and_get_json(text.as_bytes()) {
        assert_eq!(message["body"], serde_json::json!(text.as_bytes()));
        assert_eq!(message["text"], text);
    }
}

#[test]
fn json_message_omits_text_for_invalid_utf8() {
    let body = [b'a', 0xff, b'b'];
    for message in read_and_get_json(&body) {
        assert_eq!(message["body"], serde_json::json!(body));
        assert!(message.get("text").is_none());
    }
}

#[test]
fn json_message_includes_empty_text_for_an_empty_body() {
    use malak_core::{HandoverFile, LocalClient};
    use malak_proto::encode_frame;
    use malak_transport_unix::UnixTransport;
    use std::sync::Arc;

    let board = Fixture::new();
    let handover = HandoverFile::read(board.lead()).unwrap();
    let mut lead =
        LocalClient::participant(Arc::new(UnixTransport::default()), handover, None).unwrap();
    // The CLI still displays empty records written by older versions or the library.
    let frame = encode_frame(
        1,
        "SEND",
        3,
        |out| {
            out.str("channel").unwrap().str("lead").unwrap();
            out.str("kind").unwrap().str("note").unwrap();
            out.str("id")
                .unwrap()
                .str("00000000000000000000000000000001")
                .unwrap();
        },
        b"",
    )
    .unwrap();
    lead.send_frame(frame, Duration::from_secs(5)).unwrap();
    assert_eq!(lead.read_frame(Duration::from_secs(5)).unwrap().op(), "OK");

    for words in [
        &["read", "lead", "--json"][..],
        &["get", "lead", "1", "--json"],
    ] {
        let message = messages(&call(&board.lead(), words)).remove(0);
        assert_eq!(message["body"], serde_json::json!([]));
        assert_eq!(message["text"], "");
    }
}

#[test]
fn a_named_board_reply_with_piped_text_requires_a_trailing_dash() {
    let board = Fixture::new();
    let (inbox, guest) = board.enrol("guest");
    let registry = board.socket.path().join(".malak/durative");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::copy(&board.config, registry.join("board.toml")).unwrap();
    let receipt = call(&guest, &["send", "--json", "lead", "guest question"]);
    let receipt: Value = serde_json::from_str(&receipt).unwrap();
    let creation = receipt["creation"].as_str().unwrap();
    let mut reply = Command::new("sh");
    let output = reply
        .args([
            "-c",
            "printf '%s\\n' 'First line' 'Second line' | \"$@\"",
            "malak-test",
            MALAK,
        ])
        .args([
            "--board",
            "durative",
            "send",
            "--reply-to",
            "lead",
            creation,
            "1",
            &inbox,
        ])
        .env("HOME", board.socket.path())
        .env("MALAK_HANDOVER", board.lead())
        .output()
        .unwrap();

    assert_empty_send_refused(output);
    assert!(messages(&call(&guest, &["read", "--json", &inbox])).is_empty());

    let output = reply.arg("-").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let received = messages(&call(&guest, &["read", "--json", &inbox]));
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["text"], "First line\nSecond line\n");
}

#[test]
fn send_refuses_an_explicit_empty_body() {
    let board = Fixture::new();
    for argument in ["", "-"] {
        let output = board
            .without_handover(&["send", "lead", argument])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_empty_send_refused(output);
    }
    assert!(messages(&call(&board.lead(), &["read", "--json", "lead"])).is_empty());
}

fn assert_empty_send_refused(output: std::process::Output) {
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{diagnostic}");
    for expected in ["Error", "Reason", "body", "Next", "pass -"] {
        assert!(diagnostic.contains(expected), "{diagnostic}");
    }
}

#[test]
fn the_lead_cli_completes_all_verbs_with_the_general_entrance_full() {
    malak_testkit::tasks::spawn(exercise_lead_with_full_general_entrance)
        .wait(Duration::from_secs(15))
        .expect("lead calls with a saturated general entrance timed out");
}

fn exercise_lead_with_full_general_entrance() {
    use malak_core::{HandoverFile, LocalClient};
    use malak_mechanism_key::Key;
    use malak_proto::encode_frame;
    use malak_transport_unix::UnixTransport;
    use std::sync::{Arc, mpsc};

    let board = Fixture::start(launch_with_one_general_place);
    let (_, worker) = board.enrol("holding-general");
    let mut held = LocalClient::participant(
        Arc::new(UnixTransport::default()),
        HandoverFile::read(&worker).unwrap(),
        Some(Arc::new(Key)),
    )
    .unwrap();
    let general = HandoverFile::read(worker).unwrap();
    let (observed, admitted) = mpsc::channel();
    let (stop, check) = mpsc::channel::<()>();
    let churn = std::thread::spawn(move || {
        for _ in 0..2 {
            assert_rejects_opening(&general);
        }
        if observed.send(()).is_err() {
            return;
        }
        while matches!(check.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            assert_rejects_opening(&general);
        }
    });
    // The rejected connections prove the worker has taken the only general
    // place. Keep that session and the churn live for each verb.
    admitted.recv().unwrap();
    let lead = board.lead();
    call(&lead, &["send", "lead", "--id", "saturated", "task"]);
    let read = call(&lead, &["read", "lead", "--json"]);
    let task = &messages(&read)[0];
    let position = task["position"].as_u64().unwrap().to_string();
    assert_eq!(
        messages(&call(&lead, &["get", "lead", &position, "--json"]))[0]["body"],
        task["body"]
    );
    call(
        &lead,
        &[
            "ack",
            "lead",
            &position,
            "--creation",
            task["creation"].as_str().unwrap(),
        ],
    );

    drop(stop);
    churn.join().unwrap();
    let wait = Duration::from_secs(5);
    held.send_frame(encode_frame(2, "PING", 0, |_| {}, &[]).unwrap(), wait)
        .unwrap();
    let reply = held.read_frame(wait).unwrap();
    assert_eq!((reply.op(), reply.rid()), ("OK", 2));
}

#[test]
fn board_raises_the_soft_descriptor_limit_to_admit_more_connections() {
    use malak_core::{HandoverFile, LocalClient};
    use malak_transport_unix::UnixTransport;
    use std::sync::Arc;

    let board = Fixture::start(|config| launch_with_descriptor_limits(config, "100", "256"));
    let transport = Arc::new(UnixTransport::default());
    let lead = || HandoverFile::read(board.lead()).unwrap();

    let _first = LocalClient::participant(transport.clone(), lead(), None).unwrap();
    LocalClient::participant(transport, lead(), None).unwrap();
}

#[test]
fn the_owner_cli_keeps_its_reserve_with_both_participant_entrances_full() {
    use malak_core::{HandoverFile, Limits, LocalClient};
    use malak_mechanism_key::Key;
    use malak_proto::encode_frame;
    use malak_transport_unix::UnixTransport;
    use std::sync::Arc;

    let board = Fixture::start(launch_with_one_general_place);
    let (inbox, worker) = board.enrol("waiting-worker");
    let (control, _) = board.enrol("control");
    let target = control.strip_prefix("to/").unwrap();
    let unix = Arc::new(UnixTransport::default());
    let lead_file = || HandoverFile::read(board.lead()).unwrap();
    let mut first_lead = LocalClient::participant(unix.clone(), lead_file(), None).unwrap();
    let mut second_lead = LocalClient::participant(unix.clone(), lead_file(), None).unwrap();
    let mut general = LocalClient::participant(
        unix,
        HandoverFile::read(&worker).unwrap(),
        Some(Arc::new(Key)),
    )
    .unwrap();
    let wait = Duration::from_secs(5);
    let excess = Limits::default().open_data_requests as u64 + 2;
    for (client, channel) in [
        (&mut first_lead, "lead"),
        (&mut second_lead, "lead"),
        (&mut general, inbox.as_str()),
    ] {
        for rid in 2..=excess {
            let read = encode_frame(
                rid,
                "READ",
                2,
                |header| {
                    header
                        .str("channels")
                        .unwrap()
                        .array(1)
                        .unwrap()
                        .str(channel)
                        .unwrap();
                    header.str("wait_ms").unwrap().u64(60_000).unwrap();
                },
                &[],
            )
            .unwrap();
            client.send_frame(read, wait).unwrap();
        }
        let rejected = client.read_frame(wait).unwrap();
        assert_eq!((rejected.op(), rejected.rid()), ("ERR", excess));
    }
    assert_rejects_opening(&lead_file());
    assert_rejects_opening(&HandoverFile::read(worker).unwrap());

    assert!(!board.status()["run"].as_str().unwrap().is_empty());
    run(&mut board.owner_command(&["grant", target, "lead:read"]));
    run(&mut board.owner_command(&["revoke", target]));

    assert!(
        board.status()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| { item["id"] == target && item["revoked"] == true })
    );
    for client in [&mut first_lead, &mut second_lead, &mut general] {
        client
            .send_frame(
                encode_frame(excess + 1, "PING", 0, |_| {}, &[]).unwrap(),
                wait,
            )
            .unwrap();
        let ping = client.read_frame(wait).unwrap();
        assert_eq!((ping.op(), ping.rid()), ("OK", excess + 1));
    }
}

#[test]
fn a_lead_sharing_the_board_account_can_use_the_owner_door() {
    let fixture = Fixture::new();
    let path = fixture.socket.path().join("shared-account-worker");
    let output = run(fixture
        .owner_command(&["enrol", "shared", "--out"])
        .arg(&path)
        .env("MALAK_HANDOVER", fixture.lead()));

    assert!(output.contains(path.to_str().unwrap()));
    assert!(path.is_file());
}

#[test]
fn simultaneous_owner_enrolments_with_one_name_create_one_participant() {
    let fixture = Fixture::new();
    let left = fixture.socket.path().join("left-handover");
    let right = fixture.socket.path().join("right-handover");
    let mut first = fixture.owner_command(&["enrol", "same-name", "--out"]);
    first.arg(&left);
    let mut second = fixture.owner_command(&["enrol", "same-name", "--out"]);
    second.arg(&right);

    let mut results = std::thread::scope(|tasks| {
        let left = tasks.spawn(move || first.output().unwrap().status.code());
        let right = second.output().unwrap().status.code();
        [left.join().unwrap(), right]
    });
    results.sort_unstable();
    assert_eq!(results, [Some(0), Some(2)]);
    assert_ne!(left.exists(), right.exists());
}

#[test]
fn revoke_accepts_the_unique_active_name() {
    let board = Fixture::new();
    let (inbox, _) = board.enrol("researcher");
    let id = inbox.strip_prefix("to/").unwrap();

    let result = run(&mut board.owner_command(&["revoke", "researcher"]));

    assert!(result.contains(id), "{result}");
    let status = run(&mut board.owner_command(&["board", "status", id, "--json"]));
    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["participants"][0]["revoked"], true);
}

#[test]
fn revoke_refuses_an_unknown_name() {
    let board = Fixture::new();

    let result = board
        .owner_command(&["revoke", "unknown-worker"])
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    let reason = String::from_utf8_lossy(&result.stderr);
    assert!(
        reason.contains("no active participant named \"unknown-worker\""),
        "{reason}"
    );
}

#[test]
fn simultaneous_grants_leave_one_complete_version() {
    let fixture = Fixture::new();
    let (inbox, _) = fixture.enrol("worker");
    let id = inbox.strip_prefix("to/").unwrap();
    let left = [("tasks", "send", 1), ("archive", "read", 2)];
    let right = [("review", "get", 4), ("archive", "ack", 8)];
    let grant_command = |rules: &[(&str, &str, u64)]| {
        let mut command = fixture.owner_command(&["grant", id]);
        command.args(
            rules
                .iter()
                .map(|(channel, action, _)| format!("{channel}:{action}")),
        );
        command
    };
    let mut first = grant_command(&left);
    let mut second = grant_command(&right);

    std::thread::scope(|tasks| {
        let one = tasks.spawn(move || run(&mut first));
        run(&mut second);
        one.join().unwrap();
    });
    let output = run(&mut fixture.owner_command(&["board", "status", id, "--json"]));
    let answer: Value = serde_json::from_str(&output).unwrap();
    let cases = [left, right].map(|rules| {
        serde_json::json!(rules.map(|(channel, _, action)| serde_json::json!([channel, action])))
    });
    assert!(cases.contains(&answer["participants"][0]["grant"][1]));
}

#[test]
fn the_owner_cli_reports_channel_creations_and_retention() {
    let board = Fixture::new();
    let channel = "review/tasks";
    run(&mut board.owner_command(&["channel", "create", channel]));
    call(&board.lead(), &["send", channel, "body"]);

    run(&mut board.owner_command(&["trim", channel, "2"]));
    let status = run(&mut board.owner_command(&["board", "status", channel, "--json"]));

    let status: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["channels"][0]["name"], channel);
    assert_eq!(status["channels"][0]["floor"], 2);
    let removed = run(&mut board.owner_command(&["channel", "remove", channel]));
    assert_eq!(
        removed.lines().next(),
        Some("Channel review/tasks removed.")
    );
}

#[test]
fn stopping_a_board_from_the_cli_allows_a_fresh_run() {
    let mut fixture = Fixture::new();
    let old = fixture.status()["run"].as_str().unwrap().to_owned();

    run(&mut fixture.owner_command(&["board", "stop"]));

    let next = launch(&fixture.config);
    let exit = std::mem::replace(&mut fixture.board, next).wait().unwrap();
    assert!(exit.success());
    fixture.wait_until_ready();
    assert_ne!(fixture.status()["run"], old);
    let repeat = fixture
        .owner_command(&["board", "stop", "--run", &old])
        .output()
        .unwrap();
    assert_eq!(repeat.status.code(), Some(2));
}

#[test]
fn the_tail_cli_streams_json_metadata_without_message_bodies() {
    let fixture = Fixture::new();
    let mut tail = fixture
        .owner_command(&["tail", "--channel", "tasks", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (found, events) = std::sync::mpsc::channel();
    let stdout = tail.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let line = BufReader::new(stdout).lines().next().transpose();
        let _ = found.send(line);
    });
    let received = (0..12).find_map(|index| {
        let id = format!("sample-{index}");
        call(
            &fixture.lead(),
            &["send", "tasks", "--id", &id, "private body"],
        );
        events.recv_timeout(Duration::from_millis(100)).ok()
    });
    let _ = tail.kill();
    let _ = tail.wait();
    reader.join().unwrap();

    let line = received.unwrap().unwrap().unwrap();
    let event: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(event["event"], "stored");
    assert_eq!(event["version"], 1);
    assert_eq!(event["kind"], "note");
    assert_eq!(event["channel"], "tasks");
    assert!(!line.contains("private body"));
}

#[test]
fn a_cli_retry_keeps_its_receipt_after_the_board_process_is_killed() {
    let mut board = Fixture::new();
    let (_, worker) = board.enrol("worker");
    let send = [
        "send", "--json", "lead", "--kind", "report", "--id", "retry-1", "saved",
    ];
    let mut first: Value = serde_json::from_str(&call(&worker, &send)).unwrap();
    board.restart();

    let retry: Value = serde_json::from_str(&call(&worker, &send)).unwrap();

    assert_eq!(first["duplicate"], false);
    first["duplicate"] = Value::Bool(true);
    assert_eq!(retry, first);
}

#[test]
fn starting_a_second_process_leaves_the_live_board_running() {
    let board = Fixture::new();

    let other = Command::new(MALAK)
        .env("HOME", board.socket.path())
        .args(["board", "start", "--config"])
        .arg(&board.config)
        .output()
        .unwrap();

    assert_eq!(other.status.code(), Some(2));
    assert!(!call(&board.lead(), &["send", "lead", "still running"]).is_empty());
}

#[test]
fn a_worker_cannot_read_another_workers_inbox() {
    let board = Fixture::new();
    let (_, alice) = board.enrol("alice");
    let (bob_inbox, _) = board.enrol("bob");
    call(&board.lead(), &["send", &bob_inbox, "task for Bob"]);

    let read = Command::new(MALAK)
        .args(["read", &bob_inbox, "--json"])
        .env("MALAK_HANDOVER", &alice)
        .output()
        .unwrap();

    assert_eq!(read.status.code(), Some(2));
    assert!(read.stdout.is_empty());
}

#[test]
fn board_start_refuses_a_symlinked_configuration() {
    let socket = SocketDir::new().unwrap();
    let config = write_config(socket.path(), "unknown");
    let link = socket.path().join("linked.toml");
    std::os::unix::fs::symlink(&config, &link).unwrap();

    let start = Command::new(MALAK)
        .env("HOME", socket.path())
        .args(["board", "start", "--config"])
        .arg(&link)
        .output()
        .unwrap();

    assert_eq!(start.status.code(), Some(2));
    assert!(!socket.path().join("st").exists());
}

fn assert_rejects_opening(credential: &malak_core::HandoverFile) {
    use malak_proto::{Initiator, SegmentDecoder, SegmentKind};
    use nix::errno::Errno;
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::io::Write;
    use std::os::fd::AsFd;

    // An opening that gets a place will produce a handshake reply even if it
    // never attaches. An idle socket could instead close at its deadline.
    let (_, opening) = Initiator::open(
        credential.binding(),
        credential.private_key(),
        credential.board_key(),
    )
    .unwrap();
    let prefix = SegmentDecoder::prefix(SegmentKind::Opening, &opening).unwrap();
    let address = credential.address().strip_prefix("unix:").unwrap();
    let mut stream = UnixStream::connect(address).unwrap();
    if let Err(error) = stream.write_all(&[prefix.as_slice(), &opening].concat()) {
        assert!(
            matches!(
                error.kind(),
                ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected
            ),
            "{error}"
        );
    }

    loop {
        let mut socket = [PollFd::new(stream.as_fd(), PollFlags::POLLIN)];
        match poll(&mut socket, PollTimeout::NONE) {
            Ok(_) => break,
            Err(Errno::EINTR) => {}
            Err(error) => panic!("wait for excess opening to close: {error}"),
        }
    }
    match stream.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
        result => panic!("unexpected response at the saturated entrance: {result:?}"),
    }
}

mod recovery {
    use super::*;
    use std::fs;

    #[cfg(target_os = "macos")]
    mod host {
        use super::*;
        use nix::errno::Errno;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::fd::AsFd;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::thread::{self, JoinHandle};
        use std::time::Instant;

        struct Host {
            attachment: std::sync::Arc<AtomicU8>,
            socket: SocketDir,
            worker: Option<JoinHandle<()>>,
        }

        impl Host {
            fn start(config: &Path) -> Self {
                let socket = SocketDir::new().unwrap();
                let path = socket.path().join("host");
                let listener = UnixListener::bind(&path).unwrap();
                writeln!(
                    OpenOptions::new().append(true).open(config).unwrap(),
                    "store_place_socket = {path:?}",
                )
                .unwrap();
                let attachment = std::sync::Arc::new(AtomicU8::new(1));
                let current = attachment.clone();
                let worker = thread::spawn(move || {
                    loop {
                        let (mut stream, _) = listener.accept().expect("accept host request");
                        let mark = current.load(Ordering::Acquire);
                        if mark == u8::MAX {
                            return;
                        }
                        let request = read_request(&stream);
                        let fields = request.split_whitespace().collect::<Vec<_>>();
                        assert_eq!(fields.len(), 4);
                        assert_eq!(fields[0], "malak-place-v1");
                        assert_eq!(fields[1].len(), 32);
                        assert!(fields[1].bytes().all(|byte| byte.is_ascii_hexdigit()));
                        assert_ne!(fields[1], "00000000000000000000000000000000");
                        fields[2].parse::<u64>().unwrap();
                        assert!(fields[3].parse::<u64>().unwrap() > 0);
                        if mark == 0 {
                            writeln!(stream, "not-established").unwrap();
                        } else {
                            writeln!(stream, "malak-place-v1 {mark:064x}").unwrap();
                        }
                    }
                });
                Self {
                    attachment,
                    socket,
                    worker: Some(worker),
                }
            }
        }

        impl Drop for Host {
            fn drop(&mut self) {
                self.attachment.store(u8::MAX, Ordering::Release);
                let _ = UnixStream::connect(self.socket.path().join("host"));
                self.worker.take().unwrap().join().unwrap();
            }
        }

        fn read_request(stream: &UnixStream) -> String {
            // The caller can connect before its request bytes are ready.
            stream.set_nonblocking(true).unwrap();
            let mut reader = BufReader::new(stream);
            let mut socket = [PollFd::new(stream.as_fd(), PollFlags::POLLIN)];
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut request = String::new();
            loop {
                let timeout =
                    PollTimeout::try_from(deadline.saturating_duration_since(Instant::now()))
                        .expect("two-second host request deadline");
                assert_ne!(timeout, PollTimeout::ZERO, "host request did not arrive");
                match poll(&mut socket, timeout) {
                    Ok(0) => panic!("host request did not arrive"),
                    Err(Errno::EINTR) => continue,
                    Err(error) => panic!("wait for host request: {error}"),
                    Ok(_) => {}
                }
                match reader.read_line(&mut request) {
                    Ok(_) => {
                        assert!(
                            request.ends_with('\n'),
                            "host request ended before its newline"
                        );
                        stream.set_nonblocking(false).unwrap();
                        return request;
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                    Err(error) => panic!("read host request: {error}"),
                }
            }
        }

        #[test]
        fn stock_markdown_applies_a_repair_with_host_attestation() {
            let mut board = Fixture::new();
            let file = stopped_damage(&mut board);
            let _host = Host::start(&board.config);
            let number = plan(&board);

            let applied = run(&mut board.owner_command(&["recover", "--apply", &number]));

            assert_repaired(&mut board, &file, &applied);
        }

        #[test]
        fn stock_recovery_refuses_a_copy_with_the_same_store_id() {
            let mut board = Fixture::new();
            let file = stopped_damage(&mut board);
            let _host = Host::start(&board.config);
            let number = plan(&board);
            replace_store_with_copy(&file);

            refusal(&board, &["recover", "--apply", &number], &file);
        }

        #[test]
        fn stock_recovery_refuses_when_the_host_reports_a_new_attachment() {
            let mut board = Fixture::new();
            let file = stopped_damage(&mut board);
            let host = Host::start(&board.config);
            let number = plan(&board);
            host.attachment.store(2, Ordering::Release);

            refusal(&board, &["recover", "--apply", &number], &file);
            assert_ne!(plan(&board), number);
        }

        #[test]
        fn stock_recovery_refuses_when_the_host_withdraws_continuity() {
            let mut board = Fixture::new();
            let file = stopped_damage(&mut board);
            let host = Host::start(&board.config);
            let number = plan(&board);
            host.attachment.store(0, Ordering::Release);

            refusal(&board, &["recover", "--apply", &number], &file);
            refusal(
                &board,
                &["recover", "--apply", &number, "--owner-vouches-same-volume"],
                &file,
            );
        }
    }

    fn stopped_damage(board: &mut Fixture) -> PathBuf {
        for id in ["repair-entry-one", "repair-entry-two", "repair-entry-three"] {
            call(&board.lead(), &["send", "lead", "--id", id, id]);
        }
        board.board.kill().unwrap();
        board.board.wait().unwrap();
        let file = fs::read_dir(board.socket.path().join("st/messages"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                fs::read(path).is_ok_and(|bytes| {
                    bytes
                        .windows(b"repair-entry-three".len())
                        .any(|part| part == b"repair-entry-three")
                })
            })
            .expect("lead channel file");
        let mut bytes = fs::read(&file).unwrap();
        let marker = b"<!-- malak 1 ";
        let starts = bytes
            .windows(marker.len())
            .enumerate()
            .filter_map(|(offset, bytes)| (bytes == marker).then_some(offset))
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 3);
        bytes.drain(starts[1]..starts[2]);
        fs::write(&file, bytes).unwrap();

        board.board = launch(&board.config);
        board.wait_until_ready();
        let damage = Command::new(MALAK)
            .env("MALAK_HANDOVER", board.lead())
            .args(["get", "lead", "2"])
            .output()
            .unwrap();
        assert_eq!(damage.status.code(), Some(3));
        run(&mut board.owner_command(&["board", "stop"]));
        assert!(board.board.wait().unwrap().success());
        file
    }

    fn plan(board: &Fixture) -> String {
        let report = run(&mut board.owner_command(&["recover"]));
        report
            .lines()
            .find_map(|line| line.strip_prefix("  Plan  "))
            .unwrap()
            .to_owned()
    }

    fn refusal(board: &Fixture, args: &[&str], file: &Path) -> String {
        let before = fs::read(file).unwrap();
        let output = board.owner_command(args).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read(file).unwrap(), before);
        String::from_utf8(output.stderr).unwrap()
    }

    #[test]
    fn stock_markdown_applies_a_repair_with_owner_confirmation() {
        damaged_board(|board, file| {
            let number = plan(board);

            let applied = run(&mut board.owner_command(&[
                "recover",
                "--apply",
                &number,
                "--owner-vouches-same-volume",
            ]));

            assert_repaired(board, file, &applied);
        });
    }

    #[test]
    fn stock_markdown_refuses_a_repair_without_owner_confirmation() {
        damaged_board(|board, file| {
            let number = plan(board);

            let reason = refusal(board, &["recover", "--apply", &number], file);

            assert!(reason.contains("--owner-vouches-same-volume"));
            assert_eq!(plan(board), number);
        });
    }

    #[test]
    fn owner_confirmation_cannot_authorize_a_copied_markdown_folder() {
        damaged_board(|board, file| {
            replace_store_with_copy(file);
            let number = plan(board);

            let reason = refusal(
                board,
                &["recover", "--apply", &number, "--owner-vouches-same-volume"],
                file,
            );

            assert!(reason.contains("filesystem ID or folder number has changed"));
            assert_eq!(plan(board), number);
        });
    }

    fn damaged_board(exercise: impl FnOnce(&mut Fixture, &Path) + Send + 'static) {
        malak_testkit::tasks::spawn(move || {
            let mut board = Fixture::new();
            let file = stopped_damage(&mut board);
            exercise(&mut board, &file);
        })
        .wait(Duration::from_secs(35))
        .expect("Markdown recovery fixture timed out");
    }

    fn replace_store_with_copy(file: &Path) {
        let directory = file.parent().unwrap();
        let original = directory.with_extension("original");
        fs::rename(directory, &original).unwrap();
        fs::create_dir(directory).unwrap();
        for entry in fs::read_dir(original).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), directory.join(entry.file_name())).unwrap();
        }
    }

    fn assert_repaired(board: &mut Fixture, file: &Path, applied: &str) {
        assert!(applied.contains("finished"));
        board.board = launch(&board.config);
        board.wait_until_ready();
        let read = messages(&call(&board.lead(), &["read", "lead", "--json"]));
        let positions = read
            .iter()
            .filter_map(|item| item["position"].as_u64())
            .collect::<Vec<_>>();
        assert_eq!(positions, [1, 3]);
        assert!(file.is_file());
    }
}

mod separate_accounts {
    use super::*;
    use malak_fs::{AccountAccess, Directory, share_with, trusted_parent_for_account};
    use nix::unistd::{User, geteuid};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Accounts {
        user: User,
        root: SocketDir,
        config: PathBuf,
    }

    impl Accounts {
        fn new() -> Self {
            let user = test_account("MALAK_TEST_SECOND_ACCOUNT");
            let root = match std::env::var_os("MALAK_TEST_SHARED_ROOT") {
                Some(directory) => SocketDir::under(Path::new(&directory)).unwrap(),
                None => SocketDir::new().unwrap(),
            };
            let folder = Directory::open(root.path()).unwrap();
            share_with(folder.file(), user.uid.as_raw(), AccountAccess::Search).unwrap();
            trusted_parent_for_account(
                &root.path().join("worker"),
                geteuid().as_raw(),
                user.uid.as_raw(),
            )
            .expect("use a trusted MALAK_TEST_SHARED_ROOT traversable by the second account");
            copy_executable(Path::new(MALAK), &root.path().join("m"));
            copy_executable(
                &std::env::current_exe().unwrap(),
                &root.path().join("probe"),
            );
            let config = write_config(root.path(), "markdown");
            Self { user, root, config }
        }

        fn start(self, separated_lead: bool) -> (Fixture, User) {
            let mut command = Command::new(MALAK);
            command
                .env("HOME", self.root.path())
                .args(["board", "start", "--config"])
                .arg(&self.config);
            if separated_lead {
                command
                    .arg("--lead-account")
                    .arg(&self.user.name)
                    .arg("--lead-handover")
                    .arg(self.root.path().join("lead"));
            }
            let child = command.stdout(Stdio::piped()).spawn().unwrap();
            let mut fixture = Fixture {
                board: child,
                config: self.config,
                socket: self.root,
            };
            fixture.wait_until_ready();
            (fixture, self.user)
        }
    }

    fn test_account(variable: &str) -> User {
        let name = std::env::var(variable).unwrap_or_else(|_| panic!("set {variable}"));
        let user = User::from_name(&name)
            .unwrap()
            .unwrap_or_else(|| panic!("{variable} does not name an existing account"));
        assert_ne!(
            user.uid,
            geteuid(),
            "{variable} must differ from the board account"
        );
        assert!(!user.uid.is_root(), "{variable} must be unprivileged");
        user
    }

    fn copy_executable(from: &Path, to: &Path) {
        fs::copy(from, to).unwrap();
        fs::set_permissions(to, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn probe(user: &User, root: &Path, role: &str) {
        let mut command = Command::new("sudo");
        command
            .args(["-n", "-u", &user.name, "--", "env"])
            .arg(format!("MALAK_PROBE_ROOT={}", root.display()))
            .arg(format!("MALAK_PROBE_UID={}", user.uid.as_raw()))
            .arg(format!("MALAK_PROBE_ROLE={role}"))
            .arg(root.join("probe"))
            .args([
                "--ignored",
                "--exact",
                "separate_accounts::account_probe",
                "--nocapture",
            ]);
        run(&mut command);
    }

    #[test]
    #[cfg_attr(
        not(malak_second_account),
        ignore = "set MALAK_TEST_SECOND_ACCOUNT to run with a real second account"
    )]
    fn a_separate_lead_account_uses_only_its_lead_entrance() {
        let (fixture, user) = Accounts::new().start(true);
        let (_, worker) = fixture.enrol("reporter");
        call(
            &worker,
            &["send", "lead", "--kind", "report", "worker report"],
        );
        probe(&user, fixture.socket.path(), "lead");
    }

    #[test]
    #[cfg_attr(
        not(malak_third_account),
        ignore = "set MALAK_TEST_SECOND_ACCOUNT and MALAK_TEST_THIRD_ACCOUNT for three real accounts"
    )]
    fn a_third_local_account_cannot_connect_to_the_separated_lead_entrance() {
        let accounts = Accounts::new();
        let third = test_account("MALAK_TEST_THIRD_ACCOUNT");
        assert_ne!(
            third.uid, accounts.user.uid,
            "the lead and third accounts must differ"
        );
        // Let the third account get to the entrance tree. The lead leaf
        // and its handover still have the stock access policy.
        let root = accounts.root.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o711)).unwrap();
        trusted_parent_for_account(&root.join("probe"), geteuid().as_raw(), third.uid.as_raw())
            .expect("use a trusted MALAK_TEST_SHARED_ROOT traversable by all three accounts");
        let (fixture, _) = accounts.start(true);

        probe(&third, fixture.socket.path(), "outsider");
    }

    #[test]
    #[cfg_attr(
        not(malak_second_account),
        ignore = "set MALAK_TEST_SECOND_ACCOUNT to run with a real second account"
    )]
    fn a_separate_participant_account_uses_only_its_general_credential() {
        let (fixture, user) = Accounts::new().start(false);
        let worker = fixture.socket.path().join("worker");
        run(fixture
            .owner_command(&[
                "enrol",
                "isolated",
                "--grant",
                "lead:send",
                "--account",
                &user.name,
                "--only-from",
                &user.name,
                "--out",
            ])
            .arg(&worker));

        probe(&user, fixture.socket.path(), "worker");
        let report = call(&fixture.lead(), &["read", "lead", "--json"]);
        assert_eq!(messages(&report)[0]["sender_name"], "isolated");
    }

    #[test]
    #[ignore = "subprocess probe run by the cross-account tests"]
    fn account_probe() {
        let root = PathBuf::from(std::env::var_os("MALAK_PROBE_ROOT").unwrap());
        let expected: u32 = std::env::var("MALAK_PROBE_UID").unwrap().parse().unwrap();
        assert_eq!(geteuid().as_raw(), expected);
        let role = std::env::var("MALAK_PROBE_ROLE").unwrap();
        if role == "outsider" {
            assert!(UnixStream::connect(root.join("d/p/s")).is_ok());
            assert_eq!(
                UnixStream::connect(root.join("d/l/s")).unwrap_err().kind(),
                ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::read(root.join("lead")).unwrap_err().kind(),
                ErrorKind::PermissionDenied
            );
            return;
        }
        assert_eq!(
            fs::read(root.join("st/owner.credential"))
                .unwrap_err()
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            UnixStream::connect(root.join("d/o/s")).unwrap_err().kind(),
            ErrorKind::PermissionDenied
        );
        if role == "worker" {
            assert_eq!(
                UnixStream::connect(root.join("d/l/s")).unwrap_err().kind(),
                ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::read(root.join("st/lead.handover")).unwrap_err().kind(),
                ErrorKind::PermissionDenied
            );
        }
        let handover = root.join(&role);
        let mut cli = Command::new(root.join("m"));
        cli.env("MALAK_HANDOVER", &handover).args([
            "send",
            "lead",
            "--kind",
            if role == "lead" { "task" } else { "report" },
            "cross-account message",
        ]);
        run(&mut cli);
        let mut read = Command::new(root.join("m"));
        read.env("MALAK_HANDOVER", handover)
            .args(["read", "lead", "--json"]);
        if role == "lead" {
            let result = run(&mut read);
            let received = messages(&result);
            assert!(received.iter().any(|entry| entry["kind"] == "report"));
            assert!(
                received
                    .iter()
                    .any(|entry| entry["sender"] == "lead" && entry["kind"] == "task")
            );
        } else {
            assert_eq!(read.output().unwrap().status.code(), Some(2));
        }
    }
}

#[test]
fn an_agent_read_labels_exactly_the_unread_positions_lost_before_delivery() {
    let board = Fixture::new();
    let handover = board.lead();
    for body in ["seen", "lost one", "lost two", "next", "last"] {
        call(&handover, &["send", "lead", body]);
    }
    let first = call(&handover, &["read", "lead", "--json", "--limit", "1"]);
    let receipt = &messages(&first)[0];
    call(
        &handover,
        &[
            "ack",
            "lead",
            "1",
            "--creation",
            receipt["creation"].as_str().unwrap(),
        ],
    );
    run(&mut board.owner_command(&["trim", "lead", "4"]));

    let output = call(&handover, &["read", "lead", "--agent", "--json"]);
    let shown: Value = serde_json::from_str(&output).unwrap();

    assert_eq!(shown["messages"][0]["lost_before"], 2);
    assert!(shown["messages"][1].get("lost_before").is_none());
}

mod mcp {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::process::{ChildStdin, ChildStdout};

    pub(super) struct Server {
        child: Child,
        input: ChildStdin,
        output: BufReader<ChildStdout>,
        next: u64,
    }

    impl Server {
        pub(super) fn start(handover: &Path) -> Self {
            Self::start_command(
                Command::new(MALAK)
                    .arg("mcp")
                    .env("MALAK_HANDOVER", handover),
            )
        }

        fn start_command(command: &mut Command) -> Self {
            let mut child = command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut server = Self {
                input: child.stdin.take().unwrap(),
                output: BufReader::new(child.stdout.take().unwrap()),
                child,
                next: 0,
            };
            assert_eq!(
                server.request(
                    "initialize",
                    json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": { "name": "tests", "version": "1" },
                    })
                )["protocolVersion"],
                "2025-11-25"
            );
            server.write(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
            server
        }

        fn write(&mut self, frame: Value) {
            writeln!(self.input, "{frame}").unwrap();
            self.input.flush().unwrap();
        }

        pub(super) fn request(&mut self, method: &str, params: Value) -> Value {
            self.next += 1;
            self.write(json!({
                "jsonrpc": "2.0",
                "id": self.next,
                "method": method,
                "params": params,
            }));
            let mut line = String::new();
            self.output.read_line(&mut line).unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["id"], self.next);
            assert!(reply.get("error").is_none(), "{reply}");
            reply["result"].clone()
        }

        fn call(&mut self, name: &str, arguments: Value) -> Value {
            let reply = self.request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            );
            assert_ne!(reply["isError"], true, "{reply}");
            reply["structuredContent"].clone()
        }
    }

    #[test]
    #[ignore = "stdio tool server ships in v0.2"]
    fn mcp_can_attach_using_the_state_handover() {
        let board = Fixture::new();
        let mut server = Server::start_command(&mut board.without_handover(&["mcp"]));

        let result = server.call("send", json!({"channel": "lead", "body": "from MCP"}));

        let read = call(&board.lead(), &["read", "lead", "--json"]);
        assert_eq!(messages(&read)[0]["id"], result["id"]);
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    #[ignore = "stdio tool server ships in v0.2"]
    fn stdio_mcp_exposes_exactly_four_participant_tools() {
        let board = Fixture::new();
        let mut server = Server::start(&board.lead());

        let methods = server.request("tools/list", json!({}));
        let names: Vec<_> = methods["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();

        assert_eq!(names, ["send", "read", "get", "ack"]);
    }

    #[test]
    #[ignore = "stdio tool server ships in v0.2"]
    fn all_four_mcp_verbs_reuse_one_participant_connection() {
        let board = Fixture::new();
        let (inbox, worker) = board.enrol("worker");
        call(&board.lead(), &["send", &inbox, "--kind", "task", "reply"]);
        let mut server = Server::start(&worker);
        // Removing the file leaves the open connection usable. Reopening
        // the board connection for each tool would now fail to attach.
        std::fs::remove_file(&worker).unwrap();

        let read = server.call("read", json!({ "channels": [&inbox] }));
        let shown = &read["messages"][0];
        assert_eq!(shown["presentation"], "task");
        let address = json!({
            "channel": inbox, "position": shown["position"], "creation": shown["creation"],
        });
        let fetched = server.call(
            "get",
            json!({
                "channel": address["channel"], "position": address["position"],
            }),
        );
        assert_eq!(fetched["messages"][0]["body"], "reply");
        assert_eq!(server.call("ack", address)["done"], true);
        let sent = server.call(
            "send",
            json!({ "channel": "lead", "kind": "report", "body": "-" }),
        );

        let delivered = call(&board.lead(), &["read", "lead", "--json"]);
        assert_eq!(messages(&delivered)[0]["id"], sent["id"]);
        assert_eq!(messages(&delivered)[0]["body"], json!([45]));
    }
}

#[test]
fn owner_status_recovers_the_reader_acknowledgement() {
    let mut board = Fixture::new();
    call(&board.lead(), &["send", "lead", "body"]);
    let status = board.status();
    let creation = status["channels"][0]["creation"].as_str().unwrap();
    call(&board.lead(), &["read", "lead"]);
    call(&board.lead(), &["ack", "--creation", creation, "lead", "1"]);

    board.restart();

    assert_eq!(board.status()["channels"][0]["readers"]["lead"], 1);
}

#[test]
fn ack_without_creation_uses_the_current_creation() {
    let board = Fixture::new();
    let (inbox, worker) = board.enrol("worker");
    let id = inbox.strip_prefix("to/").unwrap();
    run(&mut board.owner_command(&["grant", id, &format!("{inbox}:read,ack")]));
    call(&board.lead(), &["send", &inbox, "delivered"]);
    call(&worker, &["read", &inbox]);

    call(&worker, &["ack", &inbox, "1"]);

    assert!(messages(&call(&worker, &["read", "--json", &inbox])).is_empty());
}

fn recreated_lead_channel() -> (Fixture, String) {
    let board = Fixture::new();
    call(&board.lead(), &["send", "lead", "old"]);
    let read = call(&board.lead(), &["read", "lead", "--json"]);
    let creation = messages(&read)[0]["creation"].as_str().unwrap().to_owned();
    run(&mut board.owner_command(&["channel", "remove", "lead"]));
    run(&mut board.owner_command(&["channel", "create", "lead"]));
    call(&board.lead(), &["send", "lead", "new"]);
    (board, creation)
}

#[test]
fn ack_without_creation_does_not_deliver_a_recreated_channels_message() {
    let (board, _) = recreated_lead_channel();

    let result = board
        .without_handover(&["ack", "lead", "1"])
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("position was not given"));
    assert_eq!(
        messages(&call(&board.lead(), &["read", "--json", "lead"])).len(),
        1
    );
}

#[test]
fn ack_with_an_old_creation_refuses_even_if_the_new_position_was_delivered() {
    let (board, old_creation) = recreated_lead_channel();
    call(&board.lead(), &["read", "lead"]);
    let result = board
        .without_handover(&["ack", "lead", "1", "--creation", &old_creation])
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("creation no longer matches"));
    assert_eq!(
        messages(&call(&board.lead(), &["read", "--json", "lead"])).len(),
        1
    );
}

#[test]
fn ack_without_read_permission_requires_an_explicit_creation() {
    let board = Fixture::new();
    let (inbox, worker) = board.enrol("worker");
    let id = inbox.strip_prefix("to/").unwrap();
    run(&mut board.owner_command(&["grant", id, &format!("{inbox}:ack")]));
    call(&board.lead(), &["send", &inbox, "message"]);

    let result = Command::new(MALAK)
        .args(["ack", &inbox, "1"])
        .env("MALAK_HANDOVER", &worker)
        .output()
        .unwrap();

    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("Pass --creation with the creation ID")
    );
}

fn participant_status(board: &Fixture, id: &str) -> Value {
    let output = run(&mut board.owner_command(&["board", "status", id, "--json"]));
    let selected: Value = serde_json::from_str(&output).unwrap();
    selected["participants"][0].clone()
}

#[test]
fn enrolment_refuses_the_reserved_participant_names() {
    let board = Fixture::new();
    for name in ["lead", "stuck"] {
        let handover = board.socket.path().join(name);
        let result = board
            .owner_command(&["enrol", name, "--out"])
            .arg(&handover)
            .output()
            .unwrap();

        assert_eq!(result.status.code(), Some(2), "{result:?}");
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        assert!(
            diagnostic.contains(&format!("participant name {name} is reserved")),
            "{diagnostic}"
        );
        assert!(!handover.exists());
    }
}

#[test]
fn enrolment_time_is_preserved_across_restart() {
    let mut board = Fixture::new();
    let (inbox, _) = board.enrol("worker");
    let id = inbox.strip_prefix("to/").unwrap();
    let first = participant_status(&board, id)["enrolled_at_ms"]
        .as_u64()
        .unwrap();

    board.restart();

    assert_eq!(participant_status(&board, id)["enrolled_at_ms"], first);
}

#[test]
fn grant_change_time_is_preserved_across_restart() {
    let mut board = Fixture::new();
    let (inbox, _) = board.enrol("worker");
    let id = inbox.strip_prefix("to/").unwrap();
    run(&mut board.owner_command(&["grant", id, "lead:get"]));
    let changed = participant_status(&board, id)["grant_changed_at_ms"]
        .as_u64()
        .unwrap();

    board.restart();

    assert_eq!(
        participant_status(&board, id)["grant_changed_at_ms"],
        changed
    );
}

#[test]
fn revocation_time_is_preserved_across_restart() {
    let mut board = Fixture::new();
    let (inbox, _) = board.enrol("worker");
    let id = inbox.strip_prefix("to/").unwrap();
    run(&mut board.owner_command(&["revoke", id]));
    let revoked = participant_status(&board, id)["revoked_at_ms"]
        .as_u64()
        .unwrap();

    board.restart();

    assert_eq!(participant_status(&board, id)["revoked_at_ms"], revoked);
}

fn roster_last_message(board: &Fixture, name: &str, viewer: &Path) -> String {
    let roster = run(board
        .owner_command(&["mailbox", "--who", "--for", name])
        .env("MALAK_HANDOVER", viewer));
    roster
        .lines()
        .find_map(|line| line.strip_prefix("  Last message  "))
        .unwrap()
        .to_owned()
}

#[test]
fn the_who_roster_finds_messages_sent_before_the_view_opened() {
    let board = Fixture::new();
    let (_, handover) = board.enrol("worker");
    call(&handover, &["send", "lead", "earlier"]);

    assert_ne!(
        roster_last_message(&board, "worker", &board.lead()),
        "Unavailable"
    );
}

#[test]
fn the_who_roster_cannot_backfill_from_a_channel_the_viewer_cannot_get() {
    let board = Fixture::new();
    let (_, handover) = board.enrol("worker");
    call(&handover, &["send", "lead", "earlier"]);

    assert_eq!(
        roster_last_message(&board, "worker", &handover),
        "Unavailable"
    );
}

#[test]
fn tail_reports_the_route_and_reason_of_a_refused_send() {
    let board = Fixture::new();
    let (_, handover) = board.enrol("worker");
    let mut tail = board
        .owner_command(&["tail", "--channel", "blocked", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (send, lines) = std::sync::mpsc::channel();
    let stdout = tail.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let line = BufReader::new(stdout).lines().next().transpose();
        let _ = send.send(line);
    });

    let found = (0..4).find_map(|_| {
        let result = Command::new(MALAK)
            .env("MALAK_HANDOVER", &handover)
            .args(["send", "blocked", "body"])
            .output()
            .unwrap();
        assert!(!result.status.success());
        lines.recv_timeout(Duration::from_millis(1200)).ok()
    });
    let _ = tail.kill();
    let _ = tail.wait();
    reader.join().unwrap();

    let event: Value = serde_json::from_str(&found.unwrap().unwrap().unwrap()).unwrap();
    assert_eq!(event["event"], "denied");
    assert_eq!(event["channel"], "blocked");
    assert_eq!(event["operation"], "send");
    assert_eq!(event["outcome"], "refused");
    assert!(!event["reason"].as_str().unwrap().is_empty());
}

mod recipes {
    use super::*;
    use serde_json::json;

    fn path(recipe: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("recipes")
            .join(recipe)
    }

    fn command(program: &str) -> Command {
        let mut command = Command::new(program);
        let bin = Path::new(MALAK).parent().unwrap().to_owned();
        let paths = std::env::var_os("PATH").unwrap();
        let paths = std::iter::once(bin).chain(std::env::split_paths(&paths));
        command.env("PATH", std::env::join_paths(paths).unwrap());
        command
    }

    fn wrap(board: &Fixture, handover: &Path, script: &str) -> std::process::Output {
        command("sh")
            .arg(path("wrapper.sh"))
            .env("MALAK_WORKER_HANDOVER", handover)
            .env("MALAK_REPORT_ID", "worker-result")
            .env("MALAK_WRAPPER_STATE", board.socket.path())
            .args(["sh", "-c", script])
            .output()
            .unwrap()
    }

    #[test]
    fn a_wrapper_reports_exit_status_and_stdout_under_the_workers_identity() {
        let board = Fixture::new();
        let (_, worker) = board.enrol("wrapped-worker");

        let result = wrap(&board, &worker, "printf result; printf secret >&2");

        assert!(result.status.success(), "{:?}", result.stderr);
        let delivered = call(&board.lead(), &["read", "lead", "--json"]);
        let report = &messages(&delivered)[0];
        assert_eq!(report["sender_name"], "wrapped-worker");
        assert_eq!(
            report["body"],
            json!(b"Worker exit status: 0\n\nresult".as_slice())
        );
    }

    #[test]
    fn a_wrapper_reports_an_exit_status_when_the_worker_has_no_result() {
        let board = Fixture::new();
        let (_, worker) = board.enrol("wrapped-worker");

        let result = wrap(&board, &worker, "printf secret >&2; exit 7");

        assert_eq!(result.status.code(), Some(7));
        let delivered = call(&board.lead(), &["read", "lead", "--json"]);
        assert_eq!(
            messages(&delivered)[0]["body"],
            json!(b"Worker exited without a result (exit status 7).".as_slice())
        );
    }
}

mod inbox_limits {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::process::Output;

    const RELAY: &str = "relay/target";

    fn board() -> Fixture {
        configured_board(
            r#"
[board.inbox_limits]
enabled = true
max_messages = 3
max_per_sender = 2
"#,
        )
    }

    fn configured_board(settings: &str) -> Fixture {
        Fixture::start(|config| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(config)
                .unwrap();
            writeln!(file, "{settings}").unwrap();
            launch(config)
        })
    }

    fn send(handover: &Path, id: &str) -> Output {
        send_to(handover, "lead", id)
    }

    fn send_to(handover: &Path, channel: &str, id: &str) -> Output {
        Command::new(MALAK)
            .args(["send", channel, "--kind", "report", "--id", id, id])
            .env("MALAK_HANDOVER", handover)
            .output()
            .unwrap()
    }

    fn filled_share() -> (Fixture, PathBuf) {
        let board = board();
        let (_, worker) = board.enrol("sender");
        for id in ["one", "two"] {
            assert!(send(&worker, id).status.success());
        }
        (board, worker)
    }

    fn filled_inbox() -> (Fixture, PathBuf, PathBuf) {
        let (board, first) = filled_share();
        let (_, second) = board.enrol("second");
        assert!(send(&second, "first").status.success());
        (board, first, second)
    }

    fn filled_relay() -> (Fixture, PathBuf) {
        let board = configured_board(
            r#"
[board.inbox_limits]
enabled = true
max_messages = 1
[[board.inbox_limits.inbox]]
pattern = "relay/*"
max_messages = 2
"#,
        );
        let (inbox, worker) = board.enrol("relay-sender");
        let id = inbox.strip_prefix("to/").unwrap();
        run(&mut board.owner_command(&["grant", id, "lead:send", "relay/*:send,create,read,ack"]));
        for id in ["one", "two"] {
            assert!(send_to(&worker, RELAY, id).status.success());
        }
        (board, worker)
    }

    #[test]
    fn configured_limits_are_off_until_enabled() {
        let board = configured_board("\n[board.inbox_limits]\nmax_messages = 1");
        let (_, sender) = board.enrol("sender");
        assert!(send(&sender, "one").status.success());

        assert!(send(&sender, "two").status.success());
    }

    #[test]
    fn the_last_matching_override_sets_the_inbox_total() {
        let board = configured_board(
            r#"
[board.inbox_limits]
enabled = true
max_messages = 3
[[board.inbox_limits.inbox]]
pattern = "**"
max_messages = 1
[[board.inbox_limits.inbox]]
pattern = "lead"
max_messages = 2
"#,
        );
        let (_, sender) = board.enrol("sender");
        for id in ["one", "two"] {
            assert!(send(&sender, id).status.success());
        }

        let refused = send(&sender, "three");

        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (2 of 2"));
    }

    #[test]
    fn an_override_inherits_the_boards_per_sender_cap() {
        let board = configured_board(
            r#"
[board.inbox_limits]
enabled = true
max_messages = 1
max_per_sender = 1
[[board.inbox_limits.inbox]]
pattern = "lead"
max_messages = 2
"#,
        );
        let (_, sender) = board.enrol("sender");
        assert!(send(&sender, "one").status.success());

        let refused = send(&sender, "two");

        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  share_full (1 of 1"));
    }

    #[test]
    fn the_canonical_inbox_counts_messages_due_to_its_participant() {
        let board = configured_board("\n[board.inbox_limits]\nenabled = true\nmax_messages = 1");
        let (inbox, recipient) = board.enrol("recipient");
        let lead = board.lead();
        assert!(send_to(&lead, &inbox, "one").status.success());
        let refused = send_to(&lead, &inbox, "two");
        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (1 of 1"));

        call(&recipient, &["read", &inbox]);
        call(&recipient, &["ack", &inbox, "1"]);

        assert!(send_to(&lead, &inbox, "two").status.success());
    }

    #[test]
    fn an_inbox_limit_does_not_trim_acknowledged_history() {
        let board = configured_board("\n[board.inbox_limits]\nenabled = true\nmax_messages = 1");
        let (_, sender) = board.enrol("sender");
        assert!(send(&sender, "one").status.success());
        ack_first(&board.lead(), "lead");
        assert!(send(&sender, "two").status.success());

        let history = call(&board.lead(), &["get", "lead", "1", "--json"]);

        assert_eq!(messages(&history)[0]["id"], "one");
    }

    #[test]
    fn trimming_a_waiting_record_releases_its_inbox_place() {
        let board = configured_board("\n[board.inbox_limits]\nenabled = true\nmax_messages = 1");
        let (_, sender) = board.enrol("sender");
        assert!(send(&sender, "one").status.success());

        run(&mut board.owner_command(&["trim", "lead", "2"]));

        assert!(send(&sender, "two").status.success());
    }

    #[test]
    fn inbox_limits_do_not_apply_to_unmatched_ordinary_channels() {
        let board = configured_board(
            r#"
[board.inbox_limits]
enabled = true
max_messages = 1
[[board.inbox_limits.inbox]]
pattern = "relay/*"
"#,
        );
        let lead = board.lead();
        for channel in ["tasks", "to/alias", "relay/target/extra"] {
            assert!(send_to(&lead, channel, "one").status.success());

            assert!(send_to(&lead, channel, "two").status.success());
        }
    }

    #[test]
    fn a_relay_override_counts_messages_until_the_lead_acknowledges() {
        let (board, worker) = filled_relay();
        ack_first(&worker, RELAY);

        let refused = send_to(&worker, RELAY, "three");

        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (2 of 2"));
        ack_first(&board.lead(), RELAY);
        assert!(send_to(&worker, RELAY, "three").status.success());
    }

    #[test]
    fn a_relay_channel_has_its_own_waiting_count() {
        let (board, worker) = filled_relay();

        assert!(send(&worker, "one").status.success());
        let refused = send(&worker, "two");
        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (1 of 1"));
        assert!(send_to(&worker, "relay/another", "one").status.success());

        ack_first(&board.lead(), "lead");
        let refused = send_to(&worker, RELAY, "three");
        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (2 of 2"));
    }

    #[test]
    fn a_relay_waiting_count_recovers_the_leads_acknowledgement() {
        let (mut board, worker) = filled_relay();
        ack_first(&board.lead(), RELAY);
        board.restart();

        assert!(send_to(&worker, RELAY, "three").status.success());
        let refused = send_to(&worker, RELAY, "four");
        assert_eq!(refused.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (2 of 2"));
    }

    #[test]
    fn one_sender_hits_its_share_and_can_retry_a_stored_message() {
        let (_board, worker) = filled_share();

        let refused = send(&worker, "three");

        assert_eq!(refused.status.code(), Some(3));
        let reason = String::from_utf8(refused.stderr).unwrap();
        assert!(reason.contains("Reason  share_full (2 of 2 sender places"));
        assert!(reason.contains("2 messages waiting"));
        assert!(send(&worker, "two").status.success());
    }

    #[test]
    fn the_mailbox_waits_for_its_designated_reader_to_ack() {
        let (board, first, second) = filled_inbox();
        let first_id = board.status()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "sender")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        run(&mut board.owner_command(&["grant", &first_id, "lead:send,read,ack"]));
        ack_first(&first, "lead");

        let refused = send(&second, "second");

        assert_eq!(refused.status.code(), Some(3));
        let reason = String::from_utf8(refused.stderr).unwrap();
        assert!(reason.contains("Reason  inbox_full (3 of 3 channel places"));
        assert!(reason.contains("3 messages waiting"));
        ack_first(&board.lead(), "lead");
        assert!(send(&second, "second").status.success());
        let remaining = messages(&call(&board.lead(), &["read", "lead", "--json"]));
        assert_eq!(remaining[0]["id"], "two");
    }

    fn ack_first(reader: &Path, channel: &str) {
        let read = call(reader, &["read", channel, "--json", "--limit", "1"]);
        let message = &messages(&read)[0];
        call(
            reader,
            &[
                "ack",
                channel,
                "1",
                "--creation",
                message["creation"].as_str().unwrap(),
            ],
        );
    }

    #[test]
    fn a_designated_ack_frees_sender_places_without_trimming_history() {
        let (board, worker) = filled_share();
        assert_eq!(send(&worker, "three").status.code(), Some(3));

        ack_first(&board.lead(), "lead");

        assert!(send(&worker, "three").status.success());
        let history = call(&board.lead(), &["get", "lead", "1", "--json"]);
        assert_eq!(messages(&history)[0]["id"], "one");
    }

    #[test]
    fn a_saved_designated_ack_frees_the_same_share_after_restart() {
        let (mut board, worker) = filled_share();
        ack_first(&board.lead(), "lead");
        board.restart();

        assert!(send(&worker, "three").status.success());
    }

    #[test]
    fn sender_usage_survives_a_board_restart() {
        let (mut board, worker) = filled_share();
        board.restart();

        let refused = send(&worker, "three");

        assert_eq!(refused.status.code(), Some(3));
        assert!(
            String::from_utf8(refused.stderr)
                .unwrap()
                .contains("Reason  share_full")
        );
    }

    #[test]
    #[ignore = "stdio tool server ships in v0.2"]
    fn the_mcp_tool_shows_the_sender_share_reason() {
        let (_board, worker) = filled_share();
        assert_mcp_reason(&worker, "share_full");
    }

    #[test]
    #[ignore = "stdio tool server ships in v0.2"]
    fn the_mcp_tool_shows_the_full_inbox_reason() {
        let (_board, _, worker) = filled_inbox();
        assert_mcp_reason(&worker, "inbox_full");
    }

    fn assert_mcp_reason(worker: &Path, expected: &str) {
        let mut mcp = super::mcp::Server::start(worker);
        let result = mcp.request(
            "tools/call",
            json!({
                "name": "send", "arguments": { "channel": "lead", "id": "three", "body": "report" },
            }),
        );

        assert_eq!(result["isError"], true);
        let reason = result["content"][0]["text"].as_str().unwrap();
        assert!(reason.starts_with(expected));
        assert!(reason.contains("retry with the same message ID"));
    }

    #[test]
    fn refusal_for_a_full_share_reaches_owner_tail() {
        let (board, worker) = filled_share();
        assert_tail_refusal(&board, &worker, "share_full");
    }

    #[test]
    fn refusal_for_a_full_inbox_reaches_owner_tail() {
        let (board, _, worker) = filled_inbox();
        assert_tail_refusal(&board, &worker, "inbox_full");
    }

    fn assert_tail_refusal(board: &Fixture, worker: &Path, expected: &str) {
        let mut tail = board
            .owner_command(&["tail", "--channel", "lead", "--json"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = tail.stdout.take().unwrap();
        let (send_event, events) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let line = BufReader::new(stdout).lines().next().transpose();
            let _ = send_event.send(line);
        });

        let event = (0..12).find_map(|_| {
            assert_eq!(send(worker, "three").status.code(), Some(3));
            events.recv_timeout(Duration::from_millis(100)).ok()
        });
        let _ = tail.kill();
        let _ = tail.wait();
        reader.join().unwrap();

        let event: Value = serde_json::from_str(&event.unwrap().unwrap().unwrap()).unwrap();
        assert_eq!(event["event"], "denied");
        assert_eq!(event["channel"], "lead");
        assert_eq!(event["outcome"], "unavailable");
        assert!(event["reason"].as_str().unwrap().starts_with(expected));
    }
}
