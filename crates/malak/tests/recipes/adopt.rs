use super::*;

fn adopt(board: &Fixture, pid: Option<u32>) -> Command {
    let mut command = board.recipe("adopt");
    command.args(["researcher", "--board", BOARD]);
    if let Some(pid) = pid {
        command.args(["--session", &pid.to_string()]);
    }
    command
}

pub(super) struct Agent(Child);

impl Agent {
    pub(super) fn start(board: &Fixture) -> Self {
        Self(
            Command::new("sleep")
                .arg("120")
                .current_dir(board.home.path())
                .spawn()
                .unwrap(),
        )
    }

    pub(super) fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn adopt_requires_an_explicit_pid_off_a_terminal() {
    let board = Fixture::new();
    let result = output(adopt(&board, None));
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("use --session PID"));
}

#[test]
fn adopt_process_lookup_reports_the_candidates_working_folder() {
    let board = Fixture::new();
    let agent = Agent::start(&board);
    let mut lookup = board.recipe("__session-process");
    lookup.arg(agent.pid().to_string());

    let process: Value = serde_json::from_str(&run(lookup)).unwrap();
    assert_eq!(process["cwd"].as_str(), board.home.path().to_str());
}

#[test]
fn adopt_refuses_a_dead_pid() {
    let board = Fixture::new();
    board.start();
    let agent = Agent::start(&board);
    let pid = agent.pid();
    drop(agent);

    let result = output(adopt(&board, Some(pid)));
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("export MALAK_HANDOVER"));
    assert!(
        board.status()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .all(|person| person["name"] != "researcher")
    );
}

#[test]
fn workers_status_shows_an_adopted_session_without_a_launcher() {
    let board = Fixture::new();
    board.start();
    let agent = Agent::start(&board);
    run(adopt(&board, Some(agent.pid())));

    let mut workers = board.recipe("workers");
    workers.args(["researcher", "--board", BOARD]);
    assert!(run(workers).contains("researcher adopted (no launcher)"));
}

#[test]
fn mailbox_roster_shows_an_adopted_session_without_a_launcher() {
    let board = Fixture::new();
    board.start();
    let agent = Agent::start(&board);
    run(adopt(&board, Some(agent.pid())));

    let mut roster = board.owner(&["mailbox", "--who"]);
    board.recipe_environment(&mut roster);
    assert!(run(roster).contains("researcher adopted (no launcher)"));
}

#[test]
fn adopt_adds_requested_grants_to_the_worker_defaults() {
    let board = Fixture::new();
    board.start();
    let agent = Agent::start(&board);
    let mut command = adopt(&board, Some(agent.pid()));
    command.args(["--grant", "extra:send"]);
    run(command);

    let status = board.status();
    let worker = participant(&status, "researcher");
    let id = worker["id"].as_str().unwrap();
    let rules = worker["grant"][1].as_array().unwrap();
    for rule in [
        json!(["extra", 1]),
        json!(["lead", 1]),
        json!(["questions", 1]),
        json!(["relay/*", 1]),
        json!(["orders", 2 | 4]),
        json!([format!("to/{id}"), 2 | 4 | 8]),
        json!([format!("selftest/{id}"), 1 | 2 | 8]),
    ] {
        assert!(rules.contains(&rule), "missing rule {rule} in {rules:?}");
    }
}

#[test]
fn adopting_again_reissues_the_existing_worker_handover() {
    let board = Fixture::new();
    board.start();
    let first = Agent::start(&board);
    let original = run({
        let mut command = adopt(&board, Some(first.pid()));
        command.args(["--grant", "extra:send"]);
        command
    });
    let before = board.status();
    let previous = participant(&before, "researcher");
    let id = previous["id"].as_str().unwrap();
    drop(first);
    wait_for_event("the adopted session's ended claim", || {
        (participant(&board.status(), "researcher")["claim"]["state"] == "ended").then_some(())
    });
    let second = Agent::start(&board);
    let reissued = run(adopt(&board, Some(second.pid())));

    let after = board.status();
    let worker = participant(&after, "researcher");
    assert_eq!(worker["id"], id);
    assert_eq!(worker["grant"], previous["grant"]);
    assert_eq!(worker["claim"]["session"]["pid"], second.pid());
    let history = worker["claim"]["history"].as_array().unwrap();
    assert_eq!(history[1]["event"], "ended");
    assert_eq!(history[2]["event"], "issued");
    assert!(reissued.contains(&format!("read --ack to/{id}")));
    assert_ne!(export(&reissued), export(&original));
}

#[test]
fn workers_status_names_the_adopt_command_once_the_adopted_session_ends() {
    let board = Fixture::new();
    board.start();
    let agent = Agent::start(&board);
    run(adopt(&board, Some(agent.pid())));
    drop(agent);
    wait_for_event("the adopted session's ended claim", || {
        (participant(&board.status(), "researcher")["claim"]["state"] == "ended").then_some(())
    });

    let mut workers = board.recipe("workers");
    workers.args(["researcher", "--board", BOARD]);
    let text = run(workers);

    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        words.contains("Re-issue malak recipe adopt researcher"),
        "{text}"
    );
}

fn export(output: &str) -> &str {
    output
        .lines()
        .find(|line| line.starts_with("export MALAK_HANDOVER="))
        .expect("the bootstrap names the handover path")
}

fn wait_for_worker_inbox(board: &Fixture, name: &str, handover: &Path) -> Command {
    let mut command = participant_command(handover, &[]);
    command.arg("--config").arg(&board.config).args([
        "mailbox",
        "--for",
        name,
        "--next",
        "--timeout",
        "2",
    ]);
    command
}

#[test]
fn mailbox_next_shows_the_adopted_callers_inbox() {
    let board = Fixture::new();
    board.start();
    run(adopt(&board, Some(std::process::id())));
    let status = board.status();
    let id = participant(&status, "researcher")["id"].as_str().unwrap();
    let record: Value = serde_json::from_slice(
        &fs::read(board.state.join("launches").join(format!("{id}.json"))).unwrap(),
    )
    .unwrap();
    let handover = Path::new(record["directory"].as_str().unwrap()).join("handover");
    board.lead(&["send", &format!("to/{id}"), "Review the new findings."]);

    let text = run(wait_for_worker_inbox(&board, "researcher", &handover));
    assert!(text.contains("Review the new findings."));
}

#[test]
fn mailbox_next_refuses_a_different_worker_name() {
    let board = Fixture::new();
    board.start();
    let handover = board.enrol("researcher", &["to/*:read,get,ack"]);
    board.enrol("reviewer", &["to/*:read,get,ack"]);

    let result = output(wait_for_worker_inbox(&board, "reviewer", &handover));
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("own worker inbox"));
}

#[test]
fn mailbox_next_still_conflicts_with_who() {
    let board = Fixture::new();
    let result = output(board.owner(&["mailbox", "--for", "researcher", "--next", "--who"]));
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("cannot be used with"));
}
