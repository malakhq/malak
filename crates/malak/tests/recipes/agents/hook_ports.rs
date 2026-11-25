//! Each agent tool registers a fixed set of hook events. The `recipe observe --reminder`
//! command reads each event's payload to decide how to respond.

use super::*;

/// Vendor-schema payloads, for events and tools the live captures did not produce.
const FIXTURES: &str = include_str!("../../../recipes/tests/observe-payloads.json");
/// Payloads captured from `claude -p` 2.1.292 and `codex exec` 0.169, ids and paths scrubbed.
const LIVE: &str = include_str!("../../../recipes/tests/live-payloads.json");
const LEAD: &str = "lead-reminder";
const HOOK_BUDGET: Duration = Duration::from_millis(50);

type Break<'a> = Box<dyn Fn(&mut Command) + 'a>;

fn fixture_payload(name: &str) -> Value {
    serde_json::from_str::<Value>(FIXTURES).unwrap()[name].clone()
}

fn live_payload(name: &str) -> Value {
    serde_json::from_str::<Value>(LIVE).unwrap()[name].clone()
}

fn observe(board: &Fixture, input: &[u8], vary: impl FnOnce(&mut Command)) -> Output {
    let mut command = board.recipe("observe");
    command
        .arg("--reminder")
        .env_remove("MALAK_HANDOVER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    vary(&mut command);
    let mut child = command.spawn().unwrap();
    // A recipe that never reads its input leaves a broken pipe, which is not the test's concern.
    let _ = child.stdin.take().unwrap().write_all(input);
    child.wait_with_output().unwrap()
}

fn delegation(tool: &str, tool_input: Value) -> Value {
    json!({
        "session_id": LEAD, "hook_event_name": "PreToolUse",
        "tool_name": tool, "tool_input": tool_input,
    })
}

fn bash(command: &str) -> Value {
    delegation("Bash", json!({ "command": command }))
}

fn worker_start_line(output: &str) -> Option<String> {
    context(output)
        .lines()
        .find(|line| line.starts_with("Starting a worker?"))
        .map(str::to_owned)
}

#[test]
fn events_malak_does_not_use_stay_silent_and_answer_within_the_budget() {
    let board = fixture();
    for payload in [
        live_payload("claude_agent_done"),
        live_payload("claude_subagent_bash_done"),
        live_payload("claude_subagent_stop"),
        live_payload("codex_ls_done"),
        fixture_payload("codex_subagent_stop"),
        fixture_payload("codex_permission_request"),
    ] {
        let name = payload["hook_event_name"].to_string();
        let payload = payload.to_string();
        let mut fastest = Duration::MAX;
        for _ in 0..5 {
            let start = Instant::now();
            let result = observe(&board, payload.as_bytes(), |_| ());
            fastest = fastest.min(start.elapsed());
            assert!(result.status.success(), "{name}");
            assert!(
                result.stdout.is_empty() && result.stderr.is_empty(),
                "{name}"
            );
        }
        assert!(fastest < HOOK_BUDGET, "{name} took {fastest:?}");
    }
}

#[test]
fn a_chat_with_no_board_hears_nothing_after_its_first_nudge() {
    let board = fixture();
    let first = reminder(&board, live_payload("claude_prompt"));
    assert!(context(&first).starts_with("Malak is installed"));
    for name in [
        "claude_stop",
        "claude_subagent_stop",
        "claude_subagent_bash_done",
    ] {
        assert_eq!(reminder(&board, live_payload(name)), "", "{name}");
    }
}

#[test]
fn session_start_gives_the_first_nudge_and_the_first_prompt_then_adds_none() {
    let board = fixture();
    let start =
        json!({"session_id":"fresh-chat", "hook_event_name":"SessionStart", "source":"startup"});
    let reply: Value = serde_json::from_str(&reminder(&board, start)).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(context(&reply.to_string()).starts_with("Malak is installed"));

    let prompt = json!({"session_id":"fresh-chat", "hook_event_name":"UserPromptSubmit"});
    assert_eq!(reminder(&board, prompt), "");
}

#[test]
fn session_start_after_a_clear_or_compact_says_nothing() {
    let board = fixture();
    for source in ["clear", "compact"] {
        let start =
            json!({"session_id": source, "hook_event_name":"SessionStart", "source": source});
        assert_eq!(reminder(&board, start), "", "{source}");
    }
}

#[test]
fn the_unfiltered_post_tool_hook_still_records_the_lead_s_owner_answer() {
    let board = question_board();
    reminder(&board, claude_question());
    reminder(&board, claude_answer());
    let posted = questions(&board);
    assert_eq!(posted.len(), 3, "two questions and their quoted answer");
}

#[test]
fn the_lead_is_shown_the_worker_start_form_before_delegating_outside_the_board() {
    let board = question_board();
    for (call, tail) in [
        (
            delegation(
                "Agent",
                json!({"prompt": "Survey", "subagent_type": "Explore"}),
            ),
            "-- claude --model claude-sonnet-5-5 ...",
        ),
        (
            delegation("Task", json!({"prompt": "Survey"})),
            "-- claude --model claude-sonnet-5-5 ...",
        ),
        (bash("codex exec 'Survey the tests'"), "-- codex exec ..."),
        (
            bash("cd work && codex exec -m gpt-5.4 'Survey'"),
            "-- codex exec ...",
        ),
        (
            bash("durative exec abc -- codex exec 'Survey'"),
            "-- durative exec ID -- codex exec ...",
        ),
    ] {
        let line =
            worker_start_line(&reminder(&board, call.clone())).unwrap_or_else(|| panic!("{call}"));
        assert!(
            line.contains("`malak --board malak recipe worker-start NAME --brief FILE "),
            "{line}"
        );
        assert!(line.contains(tail), "{line}");
        assert!(line.contains("exit report"), "{line}");
        // Not limited by the status cooldown: the next call gets it too.
        assert!(worker_start_line(&reminder(&board, call)).is_some());
    }
}

#[test]
fn the_lead_hears_nothing_for_other_tools_or_for_worker_start_itself() {
    let board = question_board();
    // The first call of a chat carries the one-time nudge, so spend it.
    reminder(&board, bash("true"));
    for call in [
        delegation("Read", json!({"file_path": "/workspace/README.md"})),
        bash("ls"),
        bash("echo codex exec"),
        bash("grep -r 'codex exec' ."),
        bash("malak --board malak recipe worker-start w --brief b.md -- codex exec 'Survey'"),
        bash(
            "malak --board malak recipe worker-start w --brief b.md -- durative exec abc -- codex exec 'Survey'",
        ),
    ] {
        assert_eq!(reminder(&board, call.clone()), "", "{call}");
    }
}

#[test]
fn only_the_lead_hears_about_delegating() {
    let board = question_board();
    reminder(&board, bash("true"));
    let worker = board.enrol("tester", &["lead:send"]);
    for (label, mut call, handover) in [
        ("another chat", bash("codex exec 'Survey'"), None),
        (
            "a worker with the lead's session id",
            bash("codex exec 'Survey'"),
            Some(&worker),
        ),
        ("the lead's own subagent", bash("codex exec 'Survey'"), None),
    ] {
        match label {
            "another chat" => call["session_id"] = json!("unrelated-chat"),
            "the lead's own subagent" => call["agent_id"] = json!("child"),
            _ => (),
        }
        let output = reminder_with_handover(&board, call, handover.map(PathBuf::as_path));
        assert!(!output.contains("Starting a worker?"), "{label}: {output}");
    }
}

#[test]
fn the_lead_of_two_boards_is_told_to_name_the_board() {
    let (first, second) = status_boards();
    lead_up_with_agent_id(&first, LEAD);
    lead_up_with_agent_id(&second, LEAD);
    reminder(&first, bash("true"));
    let line = worker_start_line(&reminder(&first, bash("codex exec 'Survey'"))).unwrap();
    assert!(
        line.contains("`malak --board BOARD recipe worker-start NAME"),
        "{line}"
    );
}

#[test]
fn a_failure_in_the_hook_never_blocks_the_tool_call() {
    let board = question_board();
    let call = bash("codex exec 'Survey'").to_string();
    let broken: [(&str, Break); 3] = [
        (
            "an unusable temporary folder",
            Box::new(|c| {
                c.env("TMPDIR", "/nonexistent-malak-hook-tmp")
                    .env("HOME", "/nonexistent-malak-hook-home");
            }),
        ),
        (
            "a recipe that cannot start Python",
            Box::new(|c| {
                c.env("PYTHONHOME", "/nonexistent-malak-python");
            }),
        ),
        (
            "a recipe that crashes",
            Box::new(|c| {
                c.env("PYTHONPATH", board.home.path())
                    .env("MALAK_RECIPE_CONFIG", "/nonexistent/board.toml");
            }),
        ),
    ];
    for (label, vary) in broken {
        let result = observe(&board, call.as_bytes(), vary);
        // Exit code 2 is how both tools are told to block the call.
        assert_eq!(result.status.code(), Some(0), "{label}");
        assert!(result.stderr.is_empty(), "{label}: {:?}", result.stderr);
    }
    for input in ["", "not json", "[1]", r#"{"hook_event_name":false}"#] {
        let result = observe(&board, input.as_bytes(), |_| ());
        assert_eq!(result.status.code(), Some(0), "{input}");
        assert!(
            result.stdout.is_empty() && result.stderr.is_empty(),
            "{input}"
        );
    }
}

#[test]
fn a_stopped_board_does_not_block_the_tool_call() {
    let board = question_board();
    run(board.owner(&["board", "stop"]));
    let result = observe(
        &board,
        delegation("Agent", json!({})).to_string().as_bytes(),
        |_| (),
    );
    assert_eq!(result.status.code(), Some(0));
    assert!(result.stderr.is_empty(), "{:?}", result.stderr);
}

#[test]
fn each_tool_gets_its_reply_format_for_each_event_it_uses() {
    let board = fixture();
    let live: Value = serde_json::from_str(LIVE).unwrap();
    for (name, event) in [
        ("claude_session_start", "SessionStart"),
        ("codex_session_start", "SessionStart"),
        ("claude_prompt", "UserPromptSubmit"),
        ("codex_prompt", "UserPromptSubmit"),
        ("claude_subagent_start", "SubagentStart"),
        ("codex_subagent", "SubagentStart"),
        ("claude_agent", "PreToolUse"),
        ("codex_bash", "PreToolUse"),
        ("claude_background", "PreToolUse"),
        ("claude_task", "PostToolUse"),
        ("codex_plan", "PostToolUse"),
    ] {
        let mut payload = live
            .get(name)
            .cloned()
            .unwrap_or_else(|| fixture_payload(name));
        payload["session_id"] = json!(format!("format-{name}"));
        let reply: Value = serde_json::from_str(&reminder(&board, payload)).unwrap_or(Value::Null);
        assert_eq!(
            reply["hookSpecificOutput"]["hookEventName"], event,
            "{name}"
        );
    }
}

fn reload(board: &Fixture) -> Command {
    let mut command = agents(board, &["--yes", "--upgrade-reload"]);
    let path = env::join_paths(
        std::iter::once(board.home.path().to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    command.env("PATH", path);
    command
}

#[test]
fn an_upgrade_asks_open_codex_threads_to_reload_their_config() {
    let board = fixture();
    let home = board.home.path();
    codex_notice::install(home, home);
    run(agents(&board, &["--yes"]));
    let mut command = reload(&board);
    codex_notice::wait_for_proxy(&mut command, home);

    let output = run(command);

    assert!(
        output.contains("Asked open Codex threads to reload their hooks."),
        "{output}"
    );
    let log = fs::read_to_string(home.join("reloaded.jsonl")).unwrap();
    let asked: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    // An empty batch writes no setting; it only makes the app-server reread the config.
    assert_eq!(asked, [json!({"edits": [], "reloadUserConfig": true})]);
    assert!(
        !home.join("injected.jsonl").exists(),
        "the reload sends no notice"
    );
}

#[test]
fn an_upgrade_without_a_reachable_codex_app_server_still_succeeds() {
    let board = fixture();
    fake_codex(&board, "#!/bin/sh\nexit 1\n");
    run(agents(&board, &["--yes"]));

    let output = run(reload(&board));

    assert!(
        output.contains("Could not ask open Codex threads to reload"),
        "{output}"
    );
}

#[test]
fn an_upgrade_leaves_a_claude_only_install_without_a_codex_call() {
    let board = Fixture::new();
    let home = board.home.path();
    fs::create_dir(home.join(".claude")).unwrap();
    codex_notice::install(home, home);
    run(agents(&board, &["--yes"]));

    let mut command = reload(&board);
    codex_notice::wait_for_proxy(&mut command, home);
    let output = run(command);

    assert_eq!(output, "");
    assert!(!home.join("reloaded.jsonl").exists());
}

#[test]
fn every_live_payload_gets_a_reply_in_its_tool_s_format_or_silence() {
    let board = fixture();
    let live: Value = serde_json::from_str(LIVE).unwrap();
    for (name, payload) in live.as_object().unwrap() {
        let event = payload["hook_event_name"].as_str().unwrap();
        let output = reminder(&board, payload.clone());
        if output.is_empty() {
            continue;
        }
        let reply: Value = serde_json::from_str(&output).unwrap_or_else(|_| panic!("{name}"));
        assert_eq!(
            reply["hookSpecificOutput"]["hookEventName"], event,
            "{name}"
        );
    }
}

#[test]
fn the_live_payloads_use_only_events_the_installer_registers() {
    let registered: Vec<&str> = FIXED_EVENTS.to_vec();
    let live: Value = serde_json::from_str(LIVE).unwrap();
    for (name, payload) in live.as_object().unwrap() {
        let event = payload["hook_event_name"].as_str().unwrap();
        assert!(registered.contains(&event), "{name}: {event}");
    }
}
