use super::*;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

struct NamedBoard {
    fixture: Fixture,
    name: &'static str,
}

impl NamedBoard {
    fn pair() -> [Self; 2] {
        let home = Rc::new(SocketDir::new().unwrap());
        let first = Fixture::in_home(Rc::clone(&home), "alpha", "sa", "a");
        let second = Fixture::in_home(home, "folder-beta", "sb", "b");
        second.configure(r#"name = "beta""#);
        [
            Self {
                fixture: first,
                name: "alpha",
            },
            Self {
                fixture: second,
                name: "beta",
            },
        ]
    }

    fn command(&self, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        self.fixture.recipe_environment(&mut command);
        command
            .current_dir(self.fixture.home.path())
            .args(["--board", self.name])
            .args(words);
        command
    }

    fn run(&self, words: &[&str]) -> String {
        run(self.command(words))
    }

    fn recipe(&self, name: &str, args: &[&str]) -> Command {
        let mut command = self.fixture.recipe(name);
        command.args(["--board", self.name]).args(args);
        command
    }

    fn with_input(&self, command: &mut Command, body: &str) -> String {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(body.as_bytes()).unwrap();
        drop(input);
        stdout(child.wait_with_output().unwrap())
    }
}

#[test]
fn board_commands_run_against_each_named_board() {
    let boards = NamedBoard::pair();
    for board in &boards {
        board.run(&["board", "start", "--detach"]);
    }
    for board in &boards {
        let run_id = board.fixture.status()["run"].clone();
        let status: Value =
            serde_json::from_str(&board.run(&["board", "status", "--json"])).unwrap();
        assert_eq!(status["run"], run_id);
        let handover = board.fixture.home.path().join(board.name);
        let enrolled = stdout(
            board
                .command(&["enrol", "worker", "--json", "--out"])
                .arg(handover)
                .output()
                .unwrap(),
        );
        let enrolled: Value = serde_json::from_str(&enrolled).unwrap();
        let worker = enrolled["id"].as_str().unwrap();
        let rekey = board
            .fixture
            .home
            .path()
            .join(format!("{}-rekey", board.name));

        let commands: &[(&[&str], &str)] = &[
            (&["channel", "create", "sample"], "sample"),
            (&["grant", worker, "lead:send"], "Grant replaced"),
            (&["send", "sample", board.name], "sample"),
            (&["read", "sample"], board.name),
            (&["get", "sample", "1"], board.name),
            (&["ack", "sample", "1"], "Acknowledged"),
            (&["trim", "sample", "2"], "sample"),
            (&["channel", "remove", "sample"], "sample"),
            (
                &["channel", "create", "selftest/lead-hook"],
                "selftest/lead-hook",
            ),
            (&["mailbox", "--waiting", "--json"], "waiting"),
        ];
        for (words, expected) in commands {
            let output = board.run(words);
            assert!(output.contains(expected), "{words:?}: {output}");
        }
        let result = stdout(
            board
                .command(&["hook", "--check"])
                .env("MALAK_HOOK_INBOX", "lead")
                .env(
                    "MALAK_HOOK_STATE",
                    board
                        .fixture
                        .home
                        .path()
                        .join(format!("{}-hook", board.name)),
                )
                .output()
                .unwrap(),
        );
        assert!(
            result.contains("Hook probe received and acknowledged"),
            "{result}"
        );
        assert!(
            board
                .fixture
                .lead(&["read", "selftest/lead-hook"])
                .contains("No messages")
        );
        assert_tail_follows(board);
        stdout(
            board
                .command(&["rekey", worker, "--out"])
                .arg(&rekey)
                .output()
                .unwrap(),
        );
        assert!(rekey.is_file());
        board.run(&["revoke", worker]);
        assert_eq!(
            participant(&board.fixture.status(), "worker")["revoked"],
            true
        );
    }
    for board in &boards {
        board.run(&["board", "stop"]);
        assert!(board.run(&["recover"]).contains("Plan"));
    }
}

fn assert_tail_follows(board: &NamedBoard) {
    let mut tail = board
        .command(&["tail", "--channel", "lead", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (send, receive) = mpsc::channel();
    let mut output = BufReader::new(tail.stdout.take().unwrap());
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        let _ = send.send(output.read_line(&mut line).map(|_| line));
    });
    let line = (0..12).find_map(|index| {
        board.run(&["send", "lead", "--id", &format!("tail-{index}"), board.name]);
        receive.recv_timeout(Duration::from_millis(100)).ok()
    });
    let _ = tail.kill();
    tail.wait().unwrap();
    reader.join().unwrap();
    let event: Value = serde_json::from_str(&line.unwrap().unwrap()).unwrap();
    assert_eq!(event["channel"], "lead");
    assert_eq!(event["event"], "stored");
}

#[test]
fn board_recipes_run_against_each_named_board() {
    let boards = NamedBoard::pair();
    for board in &boards {
        board.run(&["board", "start", "--detach"]);
    }
    for board in &boards {
        let setup = output(board.recipe("lead-up", &[]));
        let diagnostics = String::from_utf8_lossy(&setup.stderr).into_owned();
        assert!(stdout(setup).contains(board.fixture.state.to_str().unwrap()));
        assert!(diagnostics.contains(&format!("Pass --board {}", board.name)));
        board.run(&["send", "lead", board.name]);

        let question = format!("What should {} do?", board.name);
        let recipes: &[(&str, &[&str], &str, &str)] = &[
            (
                "ask",
                &[
                    "--blocks",
                    "setup",
                    "--option",
                    "test",
                    "--recommend",
                    "test",
                ],
                &question,
                "questions",
            ),
            ("answer", &["questions", "1"], board.name, "questions"),
            ("order", &["--for", "10m"], board.name, "orders"),
        ];
        for (recipe, options, body, expected) in recipes {
            let result = board.with_input(&mut board.recipe(recipe, options), body);
            assert!(result.contains(expected), "{recipe}: {result}");
        }

        let root = board.fixture.home.path().join(board.name);
        fs::create_dir(&root).unwrap();
        let brief = root.join("brief");
        fs::write(&brief, board.name).unwrap();
        let brief = brief.to_str().unwrap();
        let launches: &[(&str, &[&str])] = &[
            (
                "worker-start",
                &[
                    "alice",
                    "--no-watcher",
                    "--brief",
                    brief,
                    "--",
                    "printf",
                    "Alice finished",
                ],
            ),
            (
                "queue",
                &[
                    "bob",
                    "--no-watcher",
                    "--after",
                    "alice",
                    "--brief",
                    brief,
                    "--",
                    "printf",
                    "Bob finished",
                ],
            ),
            ("handoff", &["alice", "1", "--to", "bob"]),
            ("forward", &["lead", "1", "bob"]),
            ("agents-md", &["--dir", root.to_str().unwrap()]),
        ];
        for (recipe, args) in launches {
            run(board.recipe(recipe, args));
        }
        let guidance = fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(guidance.contains(&format!("Pass `--board {}`", board.name)));
        let bob = participant(&board.fixture.status(), "bob")["id"].clone();
        let inbox = format!("to/{}", bob.as_str().unwrap());
        let delivery = messages(&board.run(&["read", "--json", &inbox]));
        let forwarded = delivery.last().unwrap();
        assert_eq!(forwarded["headers"]["reply_to"][0], "lead");
        assert!(forwarded["text"].as_str().unwrap().ends_with(board.name));

        let reminder = json!({"session_id": board.name, "hook_event_name": "SessionStart", "source": "startup"});
        let shown = board.with_input(
            board
                .recipe("observe", &[])
                .env("MALAK_OBSERVE", "1")
                .env("TMPDIR", &root),
            &reminder.to_string(),
        );
        assert!(
            shown.contains(&format!("--board {}", board.name)),
            "{shown}"
        );

        let worker = board
            .fixture
            .enrol(&format!("wrapped-{}", board.name), &["lead:send"]);
        stdout(
            board
                .recipe("wrapper", &["--", "printf", board.name])
                .env("MALAK_WORKER_HANDOVER", worker)
                .env("MALAK_REPORT_ID", format!("wrapper-{}", board.name))
                .env("MALAK_WRAPPER_STATE", &root)
                .output()
                .unwrap(),
        );
        let reports = messages(&board.run(&["read", "--json", "lead"]));
        assert_eq!(
            reports.last().unwrap()["text"],
            format!("Worker exit status: 0\n\n{}", board.name)
        );

        let mut demo = board.recipe("demo", &["claude", "Fixture goal"]);
        let path = env::var_os("PATH").unwrap();
        let path = env::join_paths(
            std::iter::once(board.fixture.home.path().to_owned()).chain(env::split_paths(&path)),
        )
        .unwrap();
        demo.env("PATH", path);
        configure_demo_agent(
            &board.fixture,
            &mut demo,
            "claude",
            "#!/bin/sh\nmalak get --json lead 1 || exit 39\nexit 37\n",
        );
        demo.env("MALAK_TEST_DEMO_WAIT_FOR_LEAD_EXIT", "1");
        let result = output(demo);
        assert_eq!(result.status.code(), Some(37), "{result:?}");
        let log = fs::read_to_string(demo_workspace(&board.fixture).join("lead.log")).unwrap();
        assert!(
            log.contains(&format!(r#""text":"{}""#, board.name)),
            "{log}"
        );
    }
}

#[test]
fn unknown_board_names_list_the_known_names() {
    let [mut board, _other] = NamedBoard::pair();
    board.name = "missing";
    for command in [
        board.command(&["board", "status"]),
        board.recipe("order", &[]),
    ] {
        let result = output(command);
        let error = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(2), "{error}");
        assert!(error.contains("unknown board name \"missing\""), "{error}");
        assert!(error.contains("Known boards: alpha, beta"), "{error}");
    }
}

#[test]
fn ambiguous_board_names_list_the_matching_configurations() {
    let [board, other] = NamedBoard::pair();
    board.fixture.configure(r#"name = "beta""#);
    for command in [
        other.command(&["board", "status"]),
        other.recipe("worker-start", &["worker", "--", "true"]),
    ] {
        let result = output(command);
        let error = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(2), "{error}");
        assert!(error.contains("ambiguous board name \"beta\""), "{error}");
        for item in [&board, &other] {
            assert!(
                error.contains(item.fixture.config.to_str().unwrap()),
                "{error}"
            );
        }
    }
}

#[test]
fn board_and_config_cannot_be_combined() {
    let [board, other] = NamedBoard::pair();
    for words in [
        &["board", "status", "--config"][..],
        &["recipe", "agents-md", "--config"],
    ] {
        let result = board
            .command(words)
            .arg(&other.fixture.config)
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.code(), Some(2), "{error}");
        assert!(
            error.contains("--board") && error.contains("--config"),
            "{error}"
        );
    }
}

#[test]
fn positional_lead_up_creates_a_project_board_and_takeover_reuses_it() {
    let [mut board, _other] = NamedBoard::pair();
    board.name = "new-project";
    board.fixture.config = board
        .fixture
        .home
        .path()
        .join(".malak/new-project/board.toml");
    for takeover in [false, true] {
        assert_eq!(board.fixture.config.exists(), takeover);
        let mut lead_up = board.fixture.recipe("lead-up");
        if takeover {
            lead_up.arg("--");
        }
        let result = lead_up
            .arg(board.name)
            .env_remove("MALAK_HANDOVER")
            .output()
            .unwrap();
        let diagnostic = String::from_utf8_lossy(&result.stderr).into_owned();
        assert!(stdout(result).contains("new-project/state/lead.handover"));
        assert!(diagnostic.contains("Pass --board new-project on every board command"));
        if takeover {
            let received = stdout(
                board
                    .command(&["read", "lead"])
                    .env_remove("MALAK_HANDOVER")
                    .output()
                    .unwrap(),
            );
            assert!(received.contains("Waiting for the next lead"));
        } else {
            let result = board
                .command(&["send", "lead", "Waiting for the next lead"])
                .env_remove("MALAK_HANDOVER")
                .output()
                .unwrap();
            stdout(result);
        }
    }
}

#[test]
fn the_default_config_resolves_as_default() {
    let [mut board, _other] = NamedBoard::pair();
    board.name = "default";
    let mut start = Command::new(MALAK);
    board.fixture.recipe_environment(&mut start);
    start.args(["board", "start", "--detach"]);
    run(start);
    board.fixture.config = board.fixture.home.path().join(".malak/malak/board.toml");
    assert!(
        board
            .run(&["board", "status", "--json"])
            .contains("\"run\"")
    );
}

#[test]
fn a_named_board_can_be_found_from_the_recorded_config() {
    let [_other, mut board] = NamedBoard::pair();
    let external = board.fixture.home.path().join("other-config");
    fs::create_dir(&external).unwrap();
    let config = external.join("malak.toml");
    fs::rename(&board.fixture.config, &config).unwrap();
    board.fixture.config = config;
    let mut start = board.fixture.owner(&["board", "start", "--detach"]);
    board.fixture.recipe_environment(&mut start);
    run(start);
    assert!(
        board
            .run(&["board", "status", "--json"])
            .contains("\"run\"")
    );
}

#[test]
fn board_stop_all_still_visits_every_board_with_a_selector() {
    let boards = NamedBoard::pair();
    for selected in &boards {
        for board in &boards {
            board.run(&["board", "start", "--detach"]);
        }
        let stopped = selected.run(&["board", "stop", "--all"]);
        assert_eq!(stopped.matches("Board stopped.").count(), 2, "{stopped}");
    }
}

#[test]
fn a_named_hook_refuses_a_handover_from_another_board() {
    let [board, other] = NamedBoard::pair();
    board.run(&["board", "start", "--detach"]);
    other.run(&["board", "start", "--detach"]);
    let refused = board
        .command(&["hook", "--check"])
        .env("MALAK_HANDOVER", other.fixture.state.join("lead.handover"))
        .env("MALAK_HOOK_INBOX", "lead")
        .env("MALAK_HOOK_STATE", board.fixture.home.path().join("hook"))
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(2), "{error}");
    assert!(
        error.contains("does not match the selected board"),
        "{error}"
    );
    assert!(!board.fixture.home.path().join("hook").exists());
}
