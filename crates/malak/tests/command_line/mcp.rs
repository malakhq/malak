//! `malak mcp`: every tool, driven through a real stdio MCP client.

use super::{Fixture, MALAK, call, run};
use malak_core::HandoverFile;
use rmcp::{
    RoleClient, ServiceExt,
    model::CallToolRequestParams,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PARTICIPANT_TOOLS: [&str; 5] = ["inbox", "ack", "get", "report", "ask_owner"];

struct Seat {
    client: RunningService<RoleClient, ()>,
    /// Every tool reply so far, for the check that none carries a secret.
    seen: Vec<String>,
}

/// What a tool returned: the whole reply as JSON, so a test reads any part of it.
struct Reply(Value);

impl Reply {
    fn failed(&self) -> bool {
        self.0["isError"] == true
    }

    fn text(&self) -> &str {
        self.0["content"][0]["text"].as_str().unwrap()
    }

    fn value(&self) -> &Value {
        &self.0["structuredContent"]
    }
}

impl Seat {
    async fn take(board: &Fixture, handover: &Path) -> Self {
        Self::take_with(board, handover, |_| {}).await
    }

    async fn take_with(
        board: &Fixture,
        handover: &Path,
        adjust: impl FnOnce(&mut Command),
    ) -> Self {
        let mut command = board.without_handover(&["mcp"]);
        command.env("MALAK_HANDOVER", handover);
        adjust(&mut command);
        let transport =
            TokioChildProcess::new(tokio::process::Command::from(command).configure(|command| {
                command.kill_on_drop(true);
            }))
            .unwrap();
        Self {
            client: ().serve(transport).await.unwrap(),
            seen: Vec::new(),
        }
    }

    async fn tool_names(&self) -> Vec<String> {
        let tools = self.client.list_all_tools().await.unwrap();
        tools.iter().map(|tool| tool.name.to_string()).collect()
    }

    async fn call(&mut self, name: &'static str, arguments: Value) -> Reply {
        let Value::Object(arguments) = arguments else {
            panic!("tool arguments are an object");
        };
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
            .await
            .unwrap();
        let reply = serde_json::to_value(result).unwrap();
        self.seen.push(reply.to_string());
        Reply(reply)
    }

    async fn ok(&mut self, name: &'static str, arguments: Value) -> Reply {
        let reply = self.call(name, arguments).await;
        assert!(!reply.failed(), "{name}: {}", reply.text());
        reply
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The text of the error `report` returns for a participant the board refuses.
pub(super) fn report_refusal(board: &Fixture, worker: &Path) -> String {
    runtime().block_on(async {
        let mut seat = Seat::take(board, worker).await;
        let reply = seat
            .call("report", json!({ "id": "three", "body": "report" }))
            .await;
        assert!(reply.failed());
        reply.text().to_owned()
    })
}

fn board_with_worker() -> (Fixture, String, PathBuf) {
    let board = Fixture::new();
    let (inbox, handover) = board.enrol("worker");
    (board, inbox, handover)
}

fn send_task(board: &Fixture, inbox: &str, body: &str) {
    call(&board.lead(), &["send", inbox, "--kind", "task", body]);
}

#[test]
fn a_worker_sees_the_participant_tools_and_no_lead_tool() {
    let (board, _, worker) = board_with_worker();

    let names = runtime().block_on(async { Seat::take(&board, &worker).await.tool_names().await });

    assert_eq!(names, PARTICIPANT_TOOLS);
}

#[test]
fn the_lead_sees_the_lead_tools_and_no_answer_tool() {
    let board = Fixture::new();

    let names =
        runtime().block_on(async { Seat::take(&board, &board.lead()).await.tool_names().await });

    assert_eq!(
        names,
        [
            "inbox",
            "ack",
            "get",
            "report",
            "ask_owner",
            "delegate",
            "start_agent",
            "agents"
        ]
    );
}

#[test]
fn a_worker_cannot_call_a_lead_tool() {
    let (board, _, worker) = board_with_worker();

    let refused = runtime().block_on(async {
        let seat = Seat::take(&board, &worker).await;
        let request = CallToolRequestParams::new("agents");
        seat.client.call_tool(request).await.is_err()
    });

    assert!(refused);
}

#[test]
fn inbox_shows_a_lead_task_as_an_order_from_its_sender() {
    let (board, inbox, worker) = board_with_worker();
    send_task(&board, &inbox, "Build it.");

    let reply = runtime().block_on(async {
        Seat::take(&board, &worker)
            .await
            .ok("inbox", json!({}))
            .await
    });

    let message = &reply.value()["messages"][0];
    assert_eq!(message["sender_name"], "lead");
    assert_eq!(message["presentation"], "task");
    assert_eq!(message["body"], "Build it.");
    assert!(
        reply.text().contains("Presentation task"),
        "{}",
        reply.text()
    );
}

#[test]
fn inbox_shows_a_worker_note_as_information() {
    let board = Fixture::new();
    let (_, peer) = board.enrol("peer");
    run(Command::new(MALAK)
        .env("HOME", board.socket.path())
        .env("MALAK_HANDOVER", &peer)
        .args(["--config"])
        .arg(&board.config)
        .args(["send", "lead", "note from peer"]));

    let reply = runtime().block_on(async {
        Seat::take(&board, &board.lead())
            .await
            .ok("inbox", json!({}))
            .await
    });

    let message = &reply.value()["messages"][0];
    assert_eq!(message["sender_name"], "peer");
    assert_eq!(message["presentation"], "information");
}

#[test]
fn inbox_waits_for_a_message() {
    let (board, inbox, worker) = board_with_worker();

    let reply = runtime().block_on(async {
        let mut seat = Seat::take(&board, &worker).await;
        send_task(&board, &inbox, "Late task.");
        seat.ok("inbox", json!({ "wait": 5 })).await
    });

    assert_eq!(reply.value()["messages"][0]["body"], "Late task.");
}

#[test]
fn inbox_acknowledges_nothing_unless_asked() {
    let (board, inbox, worker) = board_with_worker();
    call(
        &board.lead(),
        &["send", &inbox, "--kind", "report", "Notice."],
    );

    let reply = runtime().block_on(async {
        Seat::take(&board, &worker)
            .await
            .ok("inbox", json!({}))
            .await
    });

    assert!(reply.value().get("acknowledged").is_none());
    assert_eq!(unacknowledged(&board, &inbox), 1);
}

#[test]
fn inbox_with_ack_acknowledges_a_notice_it_returned() {
    let (board, inbox, worker) = board_with_worker();
    call(
        &board.lead(),
        &["send", &inbox, "--kind", "report", "Notice."],
    );

    let reply = runtime().block_on(async {
        Seat::take(&board, &worker)
            .await
            .ok("inbox", json!({ "ack": true }))
            .await
    });

    assert_eq!(reply.value()["acknowledged"][0]["position"], 1);
    assert_eq!(unacknowledged(&board, &inbox), 0);
}

#[test]
fn inbox_with_ack_leaves_a_task_and_what_follows_it() {
    let (board, inbox, worker) = board_with_worker();
    call(
        &board.lead(),
        &["send", &inbox, "--kind", "report", "Notice."],
    );
    send_task(&board, &inbox, "Task.");
    call(
        &board.lead(),
        &["send", &inbox, "--kind", "report", "Later notice."],
    );

    let reply = runtime().block_on(async {
        Seat::take(&board, &worker)
            .await
            .ok("inbox", json!({ "ack": true }))
            .await
    });

    assert_eq!(reply.value()["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        reply.value()["acknowledged"],
        json!([{
            "channel": inbox, "creation": reply.value()["messages"][0]["creation"], "position": 1,
        }])
    );
    assert_eq!(unacknowledged(&board, &inbox), 2);
}

#[test]
fn ack_acknowledges_a_task_after_the_work() {
    let (board, inbox, worker) = board_with_worker();
    send_task(&board, &inbox, "Task.");

    runtime().block_on(async {
        let mut seat = Seat::take(&board, &worker).await;
        let read = seat.ok("inbox", json!({})).await;
        let message = &read.value()["messages"][0];
        let reply = seat
            .ok(
                "ack",
                json!({
                    "channel": message["channel"], "position": message["position"],
                    "creation": message["creation"],
                }),
            )
            .await;
        assert_eq!(reply.value()["done"], true);
    });

    assert_eq!(unacknowledged(&board, &inbox), 0);
}

#[test]
fn get_returns_one_message_by_channel_and_position() {
    let (board, inbox, worker) = board_with_worker();
    send_task(&board, &inbox, "Task.");

    let reply = runtime().block_on(async {
        Seat::take(&board, &worker)
            .await
            .ok("get", json!({ "channel": inbox, "position": 1 }))
            .await
    });

    assert_eq!(reply.value()["messages"][0]["body"], "Task.");
}

#[test]
fn report_sends_a_report_to_the_lead() {
    let (board, _, worker) = board_with_worker();

    runtime().block_on(async {
        let mut seat = Seat::take(&board, &worker).await;
        seat.ok("report", json!({ "body": "Done." })).await;
    });

    let read = call(&board.lead(), &["read", "lead", "--json"]);
    let report = &super::messages(&read)[0];
    assert_eq!(report["kind"], "report");
    assert_eq!(report["body"], json!(b"Done."));
}

#[test]
fn ask_owner_posts_the_question_with_options_and_a_recommendation() {
    let board = Fixture::new();
    let (_, asker) = enrol_asker(&board);

    runtime().block_on(async {
        let mut seat = Seat::take(&board, &asker).await;
        seat.ok(
            "ask_owner",
            json!({
                "question": "Which database?",
                "options": [
                    { "option": "SQLite", "pros": "Simple.", "cons": "One writer." },
                    { "option": "Postgres", "pros": "Scales.", "cons": "A server." },
                ],
                "recommendation": "SQLite",
                "blocks": "The schema.",
            }),
        )
        .await;
    });

    let posted = call(&board.lead(), &["read", "questions", "--agent"]);
    for part in [
        "Which database?",
        "- SQLite Pros: Simple. Cons: One writer.",
        "- Postgres Pros: Scales. Cons: A server.",
        "Blocks: The schema.",
        "Recommendation: SQLite",
    ] {
        assert!(posted.contains(part), "{part}: {posted}");
    }
}

#[test]
fn a_refusal_comes_back_as_a_tool_error_with_the_boards_reason() {
    let board = Fixture::new();
    let (_, outsider) = enrol_without_lead_access(&board);

    let reply = runtime().block_on(async {
        Seat::take(&board, &outsider)
            .await
            .call("report", json!({ "body": "Not allowed." }))
            .await
    });

    assert!(reply.failed());
    assert!(!reply.text().is_empty());
}

#[test]
fn the_lead_lists_an_agent_it_started() {
    let board = Fixture::new();
    start_stub_worker(&board, "builder");

    let reply = runtime().block_on(async {
        Seat::take(&board, &board.lead())
            .await
            .ok("agents", json!({}))
            .await
    });

    assert_eq!(reply.value()["agents"][0]["name"], "builder");
    assert!(reply.text().contains("builder"), "{}", reply.text());
}

#[test]
fn the_lead_delegates_a_task_the_agent_receives_as_an_order() {
    let board = Fixture::new();
    start_stub_worker(&board, "builder");

    runtime().block_on(async {
        let mut seat = Seat::take(&board, &board.lead()).await;
        seat.ok(
            "delegate",
            json!({ "agent": "builder", "task": "Run the checks." }),
        )
        .await;
    });

    let inbox = worker_inbox(&board, "builder");
    let read = call(&board.lead(), &["get", &inbox, "1", "--json"]);
    let task = &super::messages(&read)[0];
    assert_eq!(task["kind"], "task");
    assert_eq!(task["sender"], "lead");
}

#[test]
fn a_lead_without_a_watcher_is_refused_when_it_starts_an_agent() {
    let board = Fixture::new();
    let stubs = stub_tools(&board);

    let reply = runtime().block_on(async {
        let mut seat = Seat::take_with(&board, &board.lead(), |command| {
            command.env("PATH", path_with(&stubs));
        })
        .await;
        seat.call(
            "start_agent",
            json!({ "name": "helper", "brief": "Help.", "tool": "claude" }),
        )
        .await
    });

    assert!(reply.failed());
    assert!(reply.text().contains("no lead watcher"), "{}", reply.text());
}

#[test]
fn a_lead_that_waits_on_its_inbox_counts_as_watching_so_start_agent_works() {
    let board = Fixture::new();
    let stubs = stub_tools(&board);

    let started = runtime().block_on(async {
        let mut seat = Seat::take_with(&board, &board.lead(), |command| {
            command.env("PATH", path_with(&stubs));
        })
        .await;
        seat.ok("inbox", json!({ "wait": 1 })).await;
        let started = seat
            .ok(
                "start_agent",
                json!({ "name": "helper", "brief": "Report back.", "tool": "claude" }),
            )
            .await;
        // The launcher reports the stub tool's end to the lead, named for the agent.
        let mut senders = Vec::new();
        while !senders.iter().any(|name| name == "helper") {
            let read = seat.ok("inbox", json!({ "wait": 30 })).await;
            let messages = read.value()["messages"].as_array().unwrap().clone();
            assert!(!messages.is_empty(), "no report arrived: {senders:?}");
            senders.extend(
                messages
                    .iter()
                    .map(|m| m["sender_name"].as_str().unwrap().to_owned()),
            );
        }
        (started, senders)
    });

    assert_eq!(started.0.value()["started"], "helper");
    assert!(started.1.contains(&"helper".to_owned()));
}

#[test]
fn no_tool_output_contains_the_handover_or_a_key() {
    let (board, inbox, worker) = board_with_worker();
    send_task(&board, &inbox, "Task.");

    let seen = runtime().block_on(async {
        let mut seat = Seat::take(&board, &worker).await;
        let read = seat.ok("inbox", json!({})).await;
        let message = read.value()["messages"][0].clone();
        seat.ok(
            "get",
            json!({ "channel": message["channel"], "position": 1 }),
        )
        .await;
        seat.ok("report", json!({ "body": "Done." })).await;
        seat.call("get", json!({ "channel": "lead", "position": 1 }))
            .await;
        seat.seen.join("\n")
    });

    let handover = HandoverFile::read(&worker).unwrap();
    let secrets = [
        hex(handover.private_key()),
        hex(handover.board_key()),
        worker.display().to_string(),
    ];
    for secret in secrets {
        assert!(!seen.contains(&secret), "output contains {secret}");
    }
}

#[test]
fn a_handover_bound_to_another_session_is_refused() {
    let board = Fixture::new();
    let mut other = Command::new("sleep")
        .arg("600")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let bound = board.socket.path().join("bound");
    run(board
        .without_handover(&[
            "enrol",
            "bound",
            "--session",
            &other.id().to_string(),
            "--grant",
            "lead:send",
        ])
        .arg("--out")
        .arg(&bound));

    let attempt = board
        .without_handover(&["mcp"])
        .env("MALAK_HANDOVER", &bound)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    other.kill().unwrap();
    other.wait().unwrap();

    assert!(!attempt.status.success());
    assert!(
        String::from_utf8_lossy(&attempt.stderr).contains("another session"),
        "{attempt:?}"
    );
}

#[test]
fn a_handover_bound_to_the_agent_works_for_its_child_server() {
    let board = Fixture::new();
    let bound = board.socket.path().join("bound");
    run(board
        .without_handover(&[
            "enrol",
            "bound",
            "--session",
            &std::process::id().to_string(),
            "--grant",
            "lead:send",
        ])
        .arg("--out")
        .arg(&bound));

    let reply = runtime().block_on(async {
        Seat::take(&board, &bound)
            .await
            .ok("report", json!({ "body": "Bound." }))
            .await
    });

    assert_eq!(reply.value()["position"], 1);
}

fn unacknowledged(board: &Fixture, inbox: &str) -> u64 {
    let status = board.status();
    let channel = status["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|channel| channel["name"] == inbox)
        .unwrap();
    let reader = inbox.strip_prefix("to/").unwrap();
    let acknowledged = channel["readers"][reader].as_u64().unwrap_or(0);
    channel["head"].as_u64().unwrap() - 1 - acknowledged
}

/// Enrol a participant that may do only what `grants` say.
fn enrol_with(board: &Fixture, name: &str, grants: &[&str]) -> (String, PathBuf) {
    let path = board.socket.path().join(name);
    let receipt = run(board
        .without_handover(&["enrol", name, "--json"])
        .arg("--out")
        .arg(&path));
    let receipt: Value = serde_json::from_str(&receipt).unwrap();
    let id = receipt["id"].as_str().unwrap();
    run(board.without_handover(&["grant", id]).args(grants));
    (format!("to/{id}"), path)
}

fn enrol_asker(board: &Fixture) -> (String, PathBuf) {
    enrol_with(board, "asker", &["questions:send"])
}

fn enrol_without_lead_access(board: &Fixture) -> (String, PathBuf) {
    enrol_with(board, "outsider", &["questions:send"])
}

/// Start a worker whose command ends at once, so it is on the board and exited.
fn start_stub_worker(board: &Fixture, name: &str) {
    run(&mut board.without_handover(&[
        "recipe",
        "worker-start",
        name,
        "--no-watcher",
        "--detach",
        "--",
        "true",
    ]));
}

fn worker_inbox(board: &Fixture, name: &str) -> String {
    let listing = run(&mut board.without_handover(&["recipe", "workers", name, "--json"]));
    let listing: Value = serde_json::from_str(&listing).unwrap();
    listing["workers"][0]["inbox"].as_str().unwrap().to_owned()
}

/// A folder with a `claude` that prints a line and exits, for `start_agent`.
fn stub_tools(board: &Fixture) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let folder = board.socket.path().join("stubs");
    fs::create_dir(&folder).unwrap();
    let claude = folder.join("claude");
    fs::write(&claude, "#!/bin/sh\necho stub claude finished\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    folder
}

fn path_with(folder: &Path) -> std::ffi::OsString {
    let mut paths = vec![folder.to_path_buf()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    std::env::join_paths(paths).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
