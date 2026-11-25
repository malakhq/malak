use super::*;
use std::io::{BufRead, BufReader};
use std::time::SystemTime;

#[path = "../fixtures/codex_notice.rs"]
mod codex_notice;
#[path = "agents/hook_ports.rs"]
mod hook_ports;

const USER_HOOK: &str = r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"keep-me"}]}]},"key":true}"#;
const SKILL_PATH: &str = ".claude/skills/malak/SKILL.md";
const CLAUDE_SETTINGS: &str = ".claude/settings.json";
const CODEX_HOOKS: &str = ".codex/hooks.json";
const MANIFEST: &str = ".malak/agents.json";

/// The same hook events are registered for each agent tool.
const FIXED_EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];

fn fixture() -> Fixture {
    let board = Fixture::new();
    for directory in [".claude", ".codex"] {
        fs::create_dir(board.home.path().join(directory)).unwrap();
    }
    board
}

fn agents(board: &Fixture, flags: &[&str]) -> Command {
    let mut command = board.recipe("agents");
    command.args(flags);
    command
}

fn settings(board: &Fixture, path: &str) -> Value {
    serde_json::from_slice(&fs::read(board.home.path().join(path)).unwrap()).unwrap()
}

#[test]
fn installs_skills_and_hooks_for_existing_clients() {
    let board = fixture();
    let output = run(agents(&board, &["--yes"]));
    let root = board.home.path();
    for (skill, settings_path, tool) in [
        (SKILL_PATH, CLAUDE_SETTINGS, "claude"),
        (".codex/skills/malak/SKILL.md", CODEX_HOOKS, "codex"),
    ] {
        let skill = fs::read_to_string(root.join(skill)).unwrap();
        assert!(skill.starts_with("---\nname: malak\n"));
        assert!(skill.contains("malak recipe --help"));
        let hooks = settings(&board, settings_path);
        assert_eq!(
            hooks["hooks"].as_object().unwrap().len(),
            FIXED_EVENTS.len()
        );
        for event in FIXED_EVENTS {
            let group = &hooks["hooks"][event][0];
            let command = group["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.ends_with(&format!(" recipe observe --reminder --tool {tool}")),
                "{event}: {command}"
            );
            // An entry with no matcher fires on every occurrence of its event.
            assert!(group.get("matcher").is_none(), "{event}");
        }
    }
    assert!(output.contains("/hooks"));
    assert_eq!(
        fs::metadata(root.join(MANIFEST))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn a_worker_session_makes_no_refused_calls_when_a_guest_posts_to_lead() {
    let board = fixture();
    board.start();
    run(agents(&board, &["--yes"]));
    let guest = board.enrol("guest", &["lead:send"]);
    let root = board.home.path();
    let brief = root.join("brief");
    fs::write(&brief, "Check the mailbox").unwrap();
    let agent = root.join("codex");
    fs::write(&agent, include_bytes!("../fixtures/worker_mailbox.py")).unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();

    let tail_path = root.join("tail.jsonl");
    let events = || {
        fs::read_to_string(&tail_path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>()
    };
    let mut tail = RunningWorker(
        board
            .owner(&["tail", "--json"])
            .stdout(fs::File::create(&tail_path).unwrap())
            .spawn()
            .unwrap(),
    );
    wait_for_event("the tail to start", || {
        board.lead(&["send", "lead", "Tail readiness"]);
        events()
            .iter()
            .any(|event| event["event"] == "stored")
            .then_some(())
    });

    let mut command = board.worker_start(
        &["--brief", brief.to_str().unwrap()],
        &[agent.to_str().unwrap()],
    );
    command.env("TMPDIR", root);
    let mut worker = RunningWorker(command.spawn().unwrap());
    wait_for_event("the worker mailbox to start", || {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "worker exited before starting its mailbox"
        );
        root.join("worker-mailbox-ready").is_file().then_some(())
    });
    run(participant_command(
        &guest,
        &["send", "--kind", "note", "lead", "Guest note"],
    ));
    let status = wait_for_event("the worker to see the guest note", || {
        worker.0.try_wait().unwrap()
    });
    assert!(status.success(), "worker exited with {status}");
    let worker_id = board.worker_id();
    wait_for_event("the tail to see the worker's report", || {
        events()
            .iter()
            .any(|event| {
                event["event"] == "stored" && event["id"] == worker_id && event["kind"] == "report"
            })
            .then_some(())
    });
    run(board.owner(&["board", "stop"]));
    assert!(tail.0.wait().unwrap().success());

    let refused = events()
        .into_iter()
        .filter(|event| event["id"] == worker_id && event["outcome"] == "refused")
        .collect::<Vec<_>>();
    assert!(refused.is_empty(), "{refused:?}");
}

#[test]
fn installs_into_empty_settings_files() {
    let board = fixture();
    let path = board.home.path().join(CLAUDE_SETTINGS);
    fs::write(&path, "").unwrap();
    run(agents(&board, &["--yes"]));
    assert!(settings(&board, CLAUDE_SETTINGS)["hooks"].is_object());
    run(agents(&board, &["--yes", "--remove"]));
    assert!(fs::read(&path).unwrap().is_empty());
}

#[test]
fn preserves_existing_settings_and_hooks() {
    let board = fixture();
    for path in [CLAUDE_SETTINGS, CODEX_HOOKS] {
        fs::write(board.home.path().join(path), USER_HOOK).unwrap();
    }
    run(agents(&board, &["--yes"]));
    for path in [CLAUDE_SETTINGS, CODEX_HOOKS] {
        let doc = settings(&board, path);
        assert_eq!(doc["key"], true);
        let groups = doc["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["hooks"][0]["command"], "keep-me");
    }
}

#[test]
fn repeated_install_does_not_duplicate_entries() {
    let board = fixture();
    run(agents(&board, &["--yes"]));
    let path = board.home.path().join(CLAUDE_SETTINGS);
    let installed = fs::read(&path).unwrap();
    assert!(run(agents(&board, &["--yes"])).contains("already installed"));
    assert_eq!(fs::read(&path).unwrap(), installed);
}

#[test]
fn removal_restores_settings_bytes_and_removes_created_files() {
    let board = fixture();
    let root = board.home.path();
    fs::write(root.join(CLAUDE_SETTINGS), USER_HOOK).unwrap();
    run(agents(&board, &["--yes"]));
    let output = run(agents(&board, &["--remove", "--yes"]));
    assert!(
        output.contains("Removed SessionStart, UserPromptSubmit, SubagentStart, SubagentStop,")
    );
    assert_eq!(
        fs::read(root.join(CLAUDE_SETTINGS)).unwrap(),
        USER_HOOK.as_bytes()
    );
    for path in [CODEX_HOOKS, MANIFEST, ".claude/skills", ".codex/skills"] {
        assert!(!root.join(path).exists(), "{path}");
    }
}

#[test]
fn removal_preserves_user_edits_after_install() {
    let board = fixture();
    let root = board.home.path();
    run(agents(&board, &["--yes"]));
    let mut updated = settings(&board, CLAUDE_SETTINGS);
    updated["key"] = json!(true);
    updated["hooks"]["UserPromptSubmit"][0]["hooks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "command", "command": "added-later"}));
    fs::write(root.join(CLAUDE_SETTINGS), updated.to_string()).unwrap();
    let notes = root.join(".claude/skills/malak/notes.md");
    fs::write(&notes, "Keep this note").unwrap();
    run(agents(&board, &["--remove", "--yes"]));
    assert_eq!(
        settings(&board, CLAUDE_SETTINGS),
        json!({"key": true, "hooks": {"UserPromptSubmit": [{"hooks": [
            {"type": "command", "command": "added-later"}
        ]}]}})
    );
    assert_eq!(fs::read_to_string(notes).unwrap(), "Keep this note");
}

#[test]
fn removal_preserves_a_preexisting_skill() {
    let board = fixture();
    let path = board.home.path().join(SKILL_PATH);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "my Malak skill").unwrap();
    run(agents(&board, &["--yes"]));
    run(agents(&board, &["--remove", "--yes"]));
    assert_eq!(fs::read_to_string(&path).unwrap(), "my Malak skill");
}

#[test]
fn dry_run_leaves_installation_and_removal_unchanged() {
    let board = fixture();
    let root = board.home.path();
    assert!(run(agents(&board, &["--dry-run", "--notify-open"])).contains("Would notify"));
    for path in [CLAUDE_SETTINGS, CODEX_HOOKS, SKILL_PATH, MANIFEST] {
        assert!(!root.join(path).exists(), "{path}");
    }
    run(agents(&board, &["--yes"]));
    let installed = fs::read(root.join(CLAUDE_SETTINGS)).unwrap();
    assert!(run(agents(&board, &["--dry-run", "--remove"])).contains("Would remove"));
    assert_eq!(fs::read(root.join(CLAUDE_SETTINGS)).unwrap(), installed);
    assert!(root.join(SKILL_PATH).is_file());
    assert!(root.join(MANIFEST).is_file());
}

#[test]
fn uninstall_removes_the_integration() {
    let board = fixture();
    run(agents(&board, &["--yes"]));
    let mut uninstall = board.owner(&["uninstall", "--yes"]);
    board.recipe_environment(&mut uninstall);
    uninstall
        .env("CARGO_HOME", board.home.path().join("cargo-home"))
        .env_remove("CARGO_INSTALL_ROOT");
    run(uninstall);
    for path in [CLAUDE_SETTINGS, CODEX_HOOKS, SKILL_PATH, MANIFEST] {
        assert!(!board.home.path().join(path).exists(), "{path}");
    }
}

#[test]
fn help_accepts_each_recipe_help_form() {
    let board = fixture();
    for arg in ["-h", "--help", "help"] {
        assert!(run(agents(&board, &[arg])).contains("usage: malak recipe agents"));
    }
    assert!(
        run(agents(&board, &["--notify-open", "--help"])).contains("usage: malak recipe agents")
    );
}

fn reminder(board: &Fixture, payload: Value) -> String {
    reminder_with_handover(board, payload, None)
}

fn reminder_with_handover(board: &Fixture, payload: Value, handover: Option<&Path>) -> String {
    let mut command = board.recipe("observe");
    command
        .arg("--reminder")
        .env("TMPDIR", board.home.path())
        .env("PYTHONPATH", board.home.path())
        .env_remove("MALAK_HANDOVER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(handover) = handover {
        command.env("MALAK_HANDOVER", handover);
    }
    let mut child = command.spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "{payload}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

fn context(output: &str) -> String {
    let reply: Value = serde_json::from_str(output).unwrap_or_else(|_| panic!("{output:?}"));
    reply["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn prompt() -> Value {
    json!({"session_id":"lead-reminder", "hook_event_name":"UserPromptSubmit"})
}

#[test]
fn session_end_removes_installed_reminder_markers() {
    let board = fixture();
    assert!(!reminder(&board, prompt()).is_empty());
    let subagent = json!({"session_id":"lead-reminder", "hook_event_name":"SubagentStart", "agent_id":"worker"});
    assert!(!reminder(&board, subagent).is_empty());

    let end = json!({"session_id":"lead-reminder", "hook_event_name":"SessionEnd"});
    assert!(reminder(&board, end).is_empty());

    assert!(!board.home.path().read_dir().unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("malak-observe-")
    }));
}

fn tool_use() -> Value {
    json!({"session_id":"lead-reminder", "hook_event_name":"PreToolUse", "tool_name":"Read"})
}

fn expire_status_check(board: &Fixture) {
    let directory = fs::read_dir(board.home.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("malak-observe-")
        })
        .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(directory.join("lead-status"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(181))
        .unwrap();
}

pub(super) fn lead_watcher(board: &Fixture) -> RunningWorker {
    let mut command = board.owner(&["mailbox", "--plain", "--for", "lead"]);
    board.recipe_environment(&mut command);
    let mut watcher = RunningWorker(command.stdout(Stdio::piped()).spawn().unwrap());
    let mut heading = String::new();
    BufReader::new(watcher.0.stdout.as_mut().unwrap())
        .read_line(&mut heading)
        .unwrap();
    assert!(heading.contains("Mailbox activity for lead"), "{heading}");
    watcher
}

#[test]
fn lead_reminder_shows_only_the_asking_line_when_all_is_clear() {
    let board = fixture();
    board.start();
    let _watcher = lead_watcher(&board);
    assert!(context(&reminder(&board, prompt())).starts_with("Malak is installed"));
    expire_status_check(&board);

    let notice = context(&reminder(&board, tool_use()));
    assert_eq!(
        notice,
        "Asking the owner something? Use the question tool, or run `malak --board malak recipe ask ...` before writing it in chat."
    );
}

#[test]
fn lead_reminder_gives_the_read_and_ack_commands_for_waiting_mail() {
    let board = fixture();
    board.start();
    let _watcher = lead_watcher(&board);
    board.lead(&["send", "lead", "First waiting message"]);
    board.lead(&["send", "lead", "Second waiting message"]);

    let notice = context(&reminder(&board, tool_use()));

    assert!(
        notice.contains("Lead mail waiting (2): `malak --board malak read lead`, handle, then `malak --board malak ack --creation"),
        "{notice}"
    );
    assert_eq!(notice.lines().count(), 2);
}

fn status_boards() -> (Fixture, Fixture) {
    let first = fixture();
    let second = first.sibling("review", "r");
    first.start();
    second.start();
    (first, second)
}

fn session(board: &Fixture, script: &str) -> String {
    // xargs stands in for an agent process above both the shell and the recipe.
    let mut command = Command::new("xargs");
    board.recipe_environment(&mut command);
    command
        .args(["-0", "sh", "-c", script, "--"])
        .env("TMPDIR", board.home.path())
        .env_remove("MALAK_HANDOVER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"run\0").unwrap();
    stdout(child.wait_with_output().unwrap())
}

#[test]
fn a_fresh_process_gets_the_neutral_reminder() {
    let (board, _) = status_boards();

    let notice = session(
        &board,
        r#"printf '%s\n' '{"session_id":"fresh","hook_event_name":"UserPromptSubmit"}' |
            malak recipe observe --reminder"#,
    );

    assert_eq!(
        context(&notice),
        "Malak is installed. If this chat already leads a board, keep leading it with its --board name. Otherwise ask the owner before leading or joining a board."
    );
}

#[test]
fn two_lead_processes_each_see_only_their_own_board() {
    let (first, second) = status_boards();
    for (board, own, other) in [(&first, "malak", "review"), (&second, "review", "malak")] {
        let notice = session(
            board,
            &format!(
                r#"malak recipe lead-up {own} >/dev/null &&
                printf '%s\n' '{{"session_id":"{own}","hook_event_name":"UserPromptSubmit"}}' |
                malak recipe observe --reminder"#,
            ),
        );

        let notice = context(&notice);
        assert!(
            notice.contains(&format!("You lead board {own:?}")),
            "{notice}"
        );
        assert!(!notice.contains(&format!("board {other:?}")), "{notice}");
    }
}

fn lead_up_with_agent_id(board: &Fixture, id: &str) {
    let prior = board
        .owner(&["board", "status", "--json"])
        .output()
        .unwrap();
    let already_bound = prior.status.success()
        && participant(&serde_json::from_slice(&prior.stdout).unwrap(), "lead")["claim"]["state"]
            == "bound";
    let mut command = board.recipe("lead-up");
    command
        .arg("--config")
        .arg(&board.config)
        .env("CODEX_SESSION_ID", id);
    run(command);
    if !already_bound {
        let read = board.lead(&["read", "--json", "--wait", "5", "lead"]);
        let messages = messages(&read);
        let notice = messages
            .iter()
            .find(|message| {
                message["id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("claim-lead-") && id.ends_with("-accepted"))
            })
            .expect("lead claim notice");
        board.lead(&[
            "ack",
            "--creation",
            notice["creation"].as_str().unwrap(),
            "lead",
            &notice["position"].as_u64().unwrap().to_string(),
        ]);
    }
}

#[test]
fn two_codex_threads_in_one_process_are_distinguished_by_session_id() {
    let (first, second) = status_boards();
    lead_up_with_agent_id(&first, "first-thread");
    lead_up_with_agent_id(&second, "second-thread");

    let payload = json!({"session_id":"first-thread", "hook_event_name":"UserPromptSubmit"});
    let notice = context(&reminder(&first, payload));

    assert!(notice.contains("You lead board \"malak\""), "{notice}");
    assert!(!notice.contains("review"), "{notice}");
}

#[test]
fn reminders_only_report_waiting_work_and_watchers_on_the_matching_board() {
    let (first, second) = status_boards();
    lead_up_with_agent_id(&first, "lead-reminder");
    lead_up_with_agent_id(&second, "different-thread");
    let _watcher = lead_watcher(&first);
    for (board, name, count) in [
        (&first, "first-reviewer", 1),
        (&second, "second-reviewer", 2),
    ] {
        let worker = board.enrol(name, &["lead:send"]);
        for _ in 0..count {
            run(participant_command(
                &worker,
                &["send", "lead", "Waiting message"],
            ));
        }
    }

    let notice = context(&reminder(&first, tool_use()));

    assert!(
        notice.contains("Lead mail waiting (1): `malak --board malak read lead`"),
        "{notice}"
    );
    assert!(!notice.contains("review"), "{notice}");
    assert!(!notice.contains("Restart your watcher"), "{notice}");
}

#[test]
fn a_worker_handover_names_only_its_board() {
    let (first, second) = status_boards();
    let worker = second.enrol("reviewer", &["lead:send"]);

    let notice = context(&reminder_with_handover(&first, prompt(), Some(&worker)));

    assert!(
        notice.contains(r#"You are reviewer on board "review""#),
        "{notice}"
    );
    assert!(!notice.contains("--board malak"), "{notice}");
    assert!(notice.contains("Report when done"), "{notice}");
    assert!(!notice.contains("Asking the owner"), "{notice}");
}

#[test]
fn a_worker_reminder_gives_the_command_for_its_waiting_inbox() {
    let board = fixture();
    board.start();
    let worker = board.enrol("alice", &["lead:send"]);
    let inbox = format!("to/{}", board.worker_id());
    board.lead(&["send", &inbox, "Worker mail"]);

    let notice = context(&reminder_with_handover(&board, prompt(), Some(&worker)));

    assert!(
        notice.contains(&format!(
            "Inbox waiting: `malak --board malak read --ack {inbox}`"
        )),
        "{notice}"
    );
}

#[test]
fn a_subagent_in_the_lead_process_gets_the_board_context() {
    let board = fixture();
    board.start();

    let payload = json!({
        "session_id": "lead-reminder", "hook_event_name": "SubagentStart", "agent_id": "child",
    });
    let notice = context(&reminder(&board, payload));

    assert!(notice.contains(r#"You lead board "malak""#), "{notice}");
}

#[test]
fn the_lead_session_survives_a_restart_after_its_message_is_removed() {
    let board = fixture();
    lead_up_with_agent_id(&board, "original-thread");
    lead_up_with_agent_id(&board, "lead-reminder");
    let session = board.status()["lead_session"].clone();
    assert_eq!(session["agent_session_id"], "lead-reminder");
    assert_eq!(session["pid"], std::process::id());
    assert!(session["start_time"].as_u64().unwrap() > 0);

    run(board.owner(&["channel", "remove", "selftest/lead"]));
    run(board.owner(&["board", "stop"]));
    run(board.owner(&["board", "start", "--detach"]));

    assert_eq!(board.status()["lead_session"], session);
    let notice = context(&reminder(&board, prompt()));
    assert!(notice.contains(r#"You lead board "malak""#), "{notice}");
}

#[test]
fn lead_status_reminder_shares_the_cooldown_across_prompt_and_tool_hooks() {
    let board = fixture();
    board.start();
    assert!(context(&reminder(&board, prompt())).contains("Restart your watcher"));

    assert!(reminder(&board, tool_use()).is_empty());
    expire_status_check(&board);
    assert!(context(&reminder(&board, tool_use())).contains("Restart your watcher"));
}

fn finish_lead_tick(board: &Fixture) {
    let mut command = board.owner(&[
        "mailbox",
        "--plain",
        "--for",
        "lead",
        "--next",
        "--timeout",
        "1",
    ]);
    board.recipe_environment(&mut command);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("No events."));
}

#[test]
fn lead_status_reminder_counts_a_lead_between_one_shot_ticks_as_watching() {
    let board = fixture();
    board.start();
    finish_lead_tick(&board);

    let notice = context(&reminder(&board, prompt()));

    assert!(!notice.contains("Restart your watcher"), "{notice}");
}

#[test]
fn lead_status_reminder_reports_a_lead_whose_last_tick_is_old() {
    let board = fixture();
    board.start();
    finish_lead_tick(&board);
    fs::OpenOptions::new()
        .write(true)
        .open(board.home.path().join("d/o/lead-next"))
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(11 * 60))
        .unwrap();

    let notice = context(&reminder(&board, prompt()));

    assert!(notice.contains("Restart your watcher"), "{notice}");
}

#[test]
fn lead_status_reminder_notices_when_its_watcher_exits() {
    let board = fixture();
    board.start();
    let watcher = lead_watcher(&board);
    assert!(!context(&reminder(&board, prompt())).contains("Restart your watcher"));
    drop(watcher);
    expire_status_check(&board);

    let notice = context(&reminder(&board, tool_use()));

    assert!(notice.contains("Restart your watcher"), "{notice}");
    assert!(
        notice.contains("malak --board malak mailbox --plain --for lead"),
        "{notice}"
    );
}

#[test]
fn lead_status_reminder_recognizes_a_one_shot_watcher() {
    let board = fixture();
    board.start();
    let mut command = board.owner(&["mailbox", "--plain", "--for", "lead", "--next"]);
    board.recipe_environment(&mut command);
    let _watcher = RunningWorker(command.spawn().unwrap());
    let door = fs::File::open(board.home.path().join("d/o")).unwrap();
    wait_for_event("the one-shot watcher to register", || {
        match door.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => Some(()),
            Ok(()) => {
                door.unlock().unwrap();
                None
            }
            Err(error) => panic!("could not check watcher lock: {error}"),
        }
    });

    let notice = context(&reminder(&board, prompt()));

    assert!(!notice.contains("Restart your watcher"), "{notice}");
}

#[test]
fn lead_status_reminder_is_silent_when_the_board_stops_responding() {
    let board = fixture();
    board.start();
    reminder(&board, prompt());
    run(board.owner(&["board", "stop"]));
    expire_status_check(&board);

    assert!(reminder(&board, tool_use()).is_empty());
}

#[test]
fn lead_status_reminder_stays_neutral_if_status_times_out() {
    let board = fixture();
    board.start();
    fs::write(
        board.home.path().join("sitecustomize.py"),
        r#"import subprocess
import time

real_run = subprocess.run

def slow_status(argv, *args, **kwargs):
    if list(argv[-3:]) == ["board", "status", "--json"]:
        time.sleep(10)
    return real_run(argv, *args, **kwargs)

subprocess.run = slow_status
"#,
    )
    .unwrap();

    let notice = context(&reminder(&board, prompt()));

    assert!(
        notice.starts_with("Malak is installed. If this chat already leads a board"),
        "{notice}"
    );
    assert_eq!(notice.lines().count(), 1);
}

#[test]
fn reminder_does_not_ask_to_record_an_owner_answer_it_has_not_seen() {
    let board = fixture();
    board.start();
    let _watcher = lead_watcher(&board);
    let worker = board.enrol("alice", &["questions:send"]);
    run(participant_command(
        &worker,
        &["send", "questions", "Agent question."],
    ));
    board.lead(&["send", "questions", "Owner question."]);
    let notice = context(&reminder(&board, tool_use()));

    assert!(!notice.contains("Record the answer:"), "{notice}");
}

fn claude_question() -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "session_id": "lead-reminder",
        "tool_name": "AskUserQuestion",
        "tool_use_id": "toolu_01TEST",
        "tool_input": {"questions": [
            {"header": "Review", "question": "Which review should run?", "multiSelect": false,
             "options": [
                {"label": "Quick (Recommended)", "description": "Checks the changed files."},
                {"label": "Full", "description": "Checks the workspace."}
             ]},
            {"header": "When", "question": "When should it run?", "options": [
                {"label": "Now"}, {"label": "Tomorrow"}
            ]}
        ]}
    })
}

fn claude_answer() -> Value {
    let mut payload = claude_question();
    payload["hook_event_name"] = json!("PostToolUse");
    payload["tool_response"] = json!({
        "questions": payload["tool_input"]["questions"],
        "answers": {"Which review should run?": "Quick, with every changed file"}
    });
    payload
}

fn question_board() -> Fixture {
    let board = fixture();
    board.start();
    lead_up_with_agent_id(&board, "lead-reminder");
    board
}

fn questions(board: &Fixture) -> Vec<Value> {
    messages(&board.lead(&["read", "--json", "questions"]))
}

#[test]
fn claude_hook_posts_each_question_with_its_options() {
    let board = question_board();
    reminder(&board, claude_question());

    let posted = questions(&board);
    assert_eq!(posted.len(), 2);
    assert_eq!(
        posted[0]["text"],
        "Which review should run?\nOptions:\n- Quick: Checks the changed files.\n- Full: Checks the workspace.\nRecommendation: Quick\n"
    );
    assert_eq!(
        posted[1]["text"],
        "When should it run?\nOptions:\n- Now\n- Tomorrow\n"
    );
}

#[test]
fn repeating_a_question_call_does_not_repost_it() {
    let board = question_board();
    reminder(&board, claude_question());
    reminder(&board, claude_question());

    assert_eq!(questions(&board).len(), 2);
}

#[test]
fn a_retry_recovers_a_question_receipt_lost_after_board_acceptance() {
    let board = question_board();
    let fault = board.home.path().join("sitecustomize.py");
    fs::write(
        &fault,
        r#"import subprocess
run = subprocess.run
def lose_receipt(argv, *args, **kwargs):
    result = run(argv, *args, **kwargs)
    if "send" in argv and list(argv[-2:]) == ["questions", "-"]:
        return subprocess.CompletedProcess(argv, 3, b"", b"lost receipt")
    return result
subprocess.run = lose_receipt
"#,
    )
    .unwrap();
    reminder(&board, claude_question());
    fs::remove_file(fault).unwrap();

    reminder(&board, claude_question());
    reminder(&board, claude_answer());

    let posted = questions(&board);
    assert_eq!(posted.len(), 3);
    assert_eq!(
        posted[2]["headers"]["reply_to"],
        json!(["questions", posted[0]["creation"], posted[0]["position"]])
    );
}

#[test]
fn claude_hook_quotes_and_links_the_selected_answer() {
    let board = question_board();
    reminder(&board, claude_question());
    reminder(&board, claude_answer());
    reminder(&board, claude_answer());

    let posted = questions(&board);
    assert_eq!(posted.len(), 3);
    assert_eq!(
        posted[2]["headers"]["reply_to"],
        json!(["questions", posted[0]["creation"], posted[0]["position"]])
    );
    let answer = posted[2]["text"].as_str().unwrap();
    assert!(answer.starts_with("Owner, in chat at "), "{answer}");
    assert!(
        answer.ends_with(": \"Quick, with every changed file\"\n"),
        "{answer}"
    );
}

#[test]
fn generic_question_hooks_record_the_question_and_its_answer() {
    let board = question_board();
    let mut payload = json!({
        "event": "question_start", "session": "lead-reminder", "tool_use_id": "generic-1",
        "tool_input": {"questions": [
            {"id": "timing", "question": "When should it run?", "blocks": "The handoff.",
             "options": [{"label": "Now"}, {"label": "Tomorrow"}], "recommendation": "Now"}
        ]}
    });
    reminder(&board, payload.clone());
    payload["event"] = json!("question_end");
    payload["tool_response"] = json!({"answers": {"timing": "Tomorrow"}});
    reminder(&board, payload);

    let posted = questions(&board);
    assert_eq!(posted.len(), 2);
    assert_eq!(
        posted[0]["text"],
        "When should it run?\nBlocks: The handoff.\nOptions:\n- Now\n- Tomorrow\nRecommendation: Now\n"
    );
    assert!(
        posted[1]["text"]
            .as_str()
            .unwrap()
            .ends_with(": \"Tomorrow\"\n")
    );
}

#[test]
fn codex_question_hooks_use_the_question_id_and_parse_the_json_result() {
    let board = question_board();
    let mut payload = json!({
        "hook_event_name": "PreToolUse", "session_id": "lead-reminder",
        "turn_id": "turn-1", "tool_name": "request_user_input", "tool_use_id": "call_test",
        "permission_mode": "default", "tool_input": {"questions": [
            {"id": "storage", "header": "Storage", "question": "Which store should we use?",
             "options": [{"label": "SQLite (Recommended)", "description": "Persists the records."},
                         {"label": "Memory", "description": "Starts empty each run."}]}
        ]}
    });
    reminder(&board, payload.clone());
    payload["hook_event_name"] = json!("PostToolUse");
    payload["tool_response"] =
        json!(r#"{"answers":{"storage":{"answers":["SQLite (Recommended)"]}}}"#);
    reminder(&board, payload);

    let posted = questions(&board);
    assert_eq!(posted.len(), 2);
    assert!(
        posted[0]["text"]
            .as_str()
            .unwrap()
            .contains("Recommendation: SQLite")
    );
    assert!(
        posted[1]["text"]
            .as_str()
            .unwrap()
            .ends_with(": \"SQLite (Recommended)\"\n")
    );
}

fn codex_async_question() -> Value {
    json!({
        "hook_event_name": "PreToolUse", "session_id": "lead-reminder",
        "turn_id": "turn-1", "tool_name": "request_user_input_async", "tool_use_id": "call_test_async",
        "tool_input": {"questions": [
            {"title": "Which review should we use?", "options": ["Quick (Recommended)", "Full"]},
            {"title": "What should we do after the review?"}
        ]}
    })
}

#[test]
fn codex_async_question_hooks_leave_the_question_open_after_acceptance() {
    let board = question_board();
    let mut payload = codex_async_question();
    reminder(&board, payload.clone());
    reminder(&board, payload.clone());
    payload["hook_event_name"] = json!("PostToolUse");
    payload["tool_response"] = json!(r#"{"accepted":true}"#);
    reminder(&board, payload);

    let posted = questions(&board);
    assert_eq!(posted.len(), 2);
    assert_eq!(
        posted[0]["text"],
        "Which review should we use?\nOptions:\n- Quick\n- Full\nRecommendation: Quick\n"
    );
    assert_eq!(posted[1]["text"], "What should we do after the review?\n");
}

#[test]
fn a_codex_async_card_reply_prompts_the_lead_to_record_the_matching_answer() {
    let board = question_board();
    reminder(&board, codex_async_question());
    let mut reply = prompt();
    let response = json!([{
        "questionItemId": r#"["request_user_input_async","call_test_async",1]"#,
        "question": "What should we do after the review?",
        "answer": "Run the checks again"
    }]);
    reply["prompt"] = json!(format!(
        "<send_user_message_question_reply>\n{response}\n</send_user_message_question_reply>"
    ));

    let notice = context(&reminder(&board, reply));

    assert!(
        notice.contains("Record the answer: `malak --board malak recipe answer questions 2`"),
        "{notice}"
    );
    assert_eq!(questions(&board).len(), 2);
}

#[test]
fn plain_chat_does_not_mark_an_async_question_answered() {
    let board = question_board();
    reminder(&board, codex_async_question());
    expire_status_check(&board);
    let mut next_prompt = prompt();
    next_prompt["prompt"] = json!("Run the checks again");

    let notice = context(&reminder(&board, next_prompt));

    assert!(!notice.contains("Record the answer:"), "{notice}");
}

#[test]
fn a_different_chat_does_not_post_owner_questions() {
    let board = question_board();
    let mut payload = claude_question();
    payload["session_id"] = json!("unrelated-chat");
    reminder(&board, payload);

    assert!(questions(&board).is_empty());
}

#[test]
fn a_worker_does_not_post_owner_questions_even_with_the_same_session_id() {
    let board = question_board();
    let worker = board.enrol("tester", &["questions:send"]);
    reminder_with_handover(&board, claude_question(), Some(&worker));

    assert!(questions(&board).is_empty());
}

#[test]
fn a_multi_board_lead_is_told_to_select_the_question_board() {
    let (first, second) = status_boards();
    lead_up_with_agent_id(&first, "lead-reminder");
    lead_up_with_agent_id(&second, "lead-reminder");

    let notice = context(&reminder(&first, claude_question()));

    assert!(
        notice.contains("malak recipe ask --board BOARD"),
        "{notice}"
    );
    assert!(questions(&first).is_empty());
    assert!(questions(&second).is_empty());
}

#[test]
fn a_failed_answer_write_prompts_the_lead_to_record_it() {
    let board = question_board();
    reminder(&board, claude_question());
    fs::write(
        board.home.path().join("sitecustomize.py"),
        r#"import subprocess
run = subprocess.run
def refuse_answer(argv, *args, **kwargs):
    if "send" in argv and "--reply-to" in argv and list(argv[-2:]) == ["questions", "-"]:
        return subprocess.CompletedProcess(argv, 2, b"", b"answer refused")
    return run(argv, *args, **kwargs)
subprocess.run = refuse_answer
"#,
    )
    .unwrap();

    let notice = context(&reminder(&board, claude_answer()));

    assert!(notice.contains("Asking the owner something?"), "{notice}");
    assert!(
        notice.contains("Record the answer: `malak --board malak recipe answer questions 1`"),
        "{notice}"
    );
    assert!(notice.len() <= 600, "{notice}");
}

#[test]
fn an_exited_workers_unread_final_report_is_named_in_the_reminder() {
    let board = question_board();
    run(board.worker_start(&[], &["/bin/echo", "Worker done"]));

    let notice = context(&reminder(&board, prompt()));

    assert!(
        notice.contains("alice exited: read its final report"),
        "{notice}"
    );
    assert!(notice.contains("Asking the owner something?"), "{notice}");
    assert!(notice.len() <= 600, "{notice}");
    assert!(notice.lines().count() <= 5, "{notice}");
}

#[test]
fn reminder_runs_once_per_chat() {
    let board = fixture();
    let prompt = json!({"session_id":"a", "hook_event_name":"UserPromptSubmit"});
    assert!(reminder(&board, prompt.clone()).contains("Malak is installed."));
    assert!(reminder(&board, prompt).is_empty());
    let background = json!({
        "session_id":"b", "hook_event_name":"PreToolUse",
        "tool_name":"Bash", "tool_input":{"run_in_background": true}
    });
    assert!(reminder(&board, background).contains("Malak is installed."));
    let prompt = json!({"session_id":"b", "hook_event_name":"UserPromptSubmit"});
    assert!(reminder(&board, prompt).is_empty());
}

#[test]
fn starting_subagent_gets_its_own_reminder() {
    let board = fixture();
    let parent = json!({"session_id":"a", "hook_event_name":"UserPromptSubmit"});
    reminder(&board, parent);
    let subagent = json!({"session_id":"a", "hook_event_name":"SubagentStart", "agent_id":"child"});
    assert!(reminder(&board, subagent.clone()).contains("Malak is installed."));
    assert!(reminder(&board, subagent).is_empty());
}

#[test]
fn install_requires_confirmation_without_a_terminal() {
    let board = fixture();
    let rejected = output(agents(&board, &[]));
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("use --yes"));
    assert!(!board.home.path().join(MANIFEST).exists());
}

fn fake_codex(board: &Fixture, script: &str) {
    let path = board.home.path().join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn notify(board: &Fixture) -> Command {
    let mut command = agents(board, &["--yes", "--notify-open"]);
    let path = env::join_paths(
        std::iter::once(board.home.path().to_path_buf())
            .chain(env::split_paths(&env::var_os("PATH").unwrap())),
    )
    .unwrap();
    command.env("PATH", path);
    command
}

#[test]
fn unavailable_app_server_does_not_block_installation() {
    let board = fixture();
    fake_codex(&board, "#!/bin/sh\nexit 1\n");
    assert!(run(notify(&board)).contains("No reachable Codex app-server"));
    assert!(board.home.path().join(SKILL_PATH).is_file());
}

#[test]
fn missing_client_directories_still_report_the_notice_attempt() {
    let board = Fixture::new();
    fake_codex(&board, "#!/bin/sh\nexit 1\n");
    assert!(run(notify(&board)).contains("No reachable Codex app-server"));
    assert!(!board.home.path().join(MANIFEST).exists());
}

#[test]
fn opt_in_injects_a_user_notice_once_per_loaded_thread() {
    let board = fixture();
    let home = board.home.path();
    codex_notice::install(home, home);
    run(agents(&board, &["--yes"]));
    let log = home.join("injected.jsonl");
    assert!(!log.exists());

    let notify_ready = || {
        let mut command = notify(&board);
        codex_notice::wait_for_proxy(&mut command, home);
        run(command)
    };
    let first = notify_ready();
    assert!(
        first.contains("Notified 1 open Codex thread(s)."),
        "{first}"
    );
    let second = notify_ready();
    assert!(
        second.contains("No new Codex threads were notified.")
            && !second.contains("No reachable Codex app-server"),
        "{second}"
    );
    let content = fs::read_to_string(log).unwrap();
    let injected: Value = serde_json::from_str(&content).unwrap();
    assert_eq!(injected["threadId"], "opened-thread");
    let item = &injected["items"][0];
    assert_eq!(item["type"], "message");
    assert_eq!(item["role"], "user");
    assert!(
        item["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Malak is installed")
    );
}

fn stop(session: &str, active: bool) -> Value {
    json!({"session_id": session, "hook_event_name": "Stop", "stop_hook_active": active})
}

fn turn_end_board() -> Fixture {
    let board = fixture();
    board.start();
    lead_up_with_agent_id(&board, "lead-reminder");
    board
}

const RESTART_WATCHER: &str = "Your watcher for board malak is not running. Restart it: malak --board malak mailbox --plain --for lead --next";

fn worker_says(board: &Fixture, channel: &str, text: &str) -> PathBuf {
    let worker = board.enrol("turn-worker", &["lead:send", "questions:send"]);
    run(participant_command(&worker, &["send", channel, text]));
    worker
}

fn reply(output: &str) -> Value {
    serde_json::from_str(output).unwrap_or_else(|_| panic!("{output:?}"))
}

#[test]
fn turn_end_hands_a_lead_its_pending_mail_once_with_the_bodies() {
    let board = turn_end_board();
    let _watcher = lead_watcher(&board);
    worker_says(&board, "lead", "Report: the parser is done");

    let answer = reply(&reminder(&board, stop("lead-reminder", false)));

    assert_eq!(answer["decision"], "block");
    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.contains("lead #"), "{reason}");
    assert!(reason.contains("Report: the parser is done"), "{reason}");
    assert!(reason.contains("acknowledge"), "{reason}");
}

#[test]
fn a_second_turn_end_with_the_same_pending_set_ends_with_a_reminder() {
    let board = turn_end_board();
    let _watcher = lead_watcher(&board);
    worker_says(&board, "lead", "Still pending");
    let _ = reminder(&board, stop("lead-reminder", false));

    let answer = reply(&reminder(&board, stop("lead-reminder", true)));

    assert!(answer.get("decision").is_none(), "{answer}");
    assert!(
        answer["systemMessage"]
            .as_str()
            .unwrap()
            .contains("1 lead message unacknowledged"),
        "{answer}"
    );
}

#[test]
fn a_new_pending_set_is_handed_over_again() {
    let board = turn_end_board();
    let worker = worker_says(&board, "lead", "First");
    let _ = reminder(&board, stop("lead-reminder", false));
    run(participant_command(&worker, &["send", "lead", "Second"]));

    let answer = reply(&reminder(&board, stop("lead-reminder", true)));

    assert_eq!(answer["decision"], "block");
    assert!(answer["reason"].as_str().unwrap().contains("Second"));
}

#[test]
fn turn_end_hands_over_an_unanswered_agent_question() {
    let board = turn_end_board();
    worker_says(&board, "questions", "Which parser should I use?");

    let answer = reply(&reminder(&board, stop("lead-reminder", false)));

    assert_eq!(answer["decision"], "block");
    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.contains("questions #"), "{reason}");
    assert!(reason.contains("Which parser should I use?"), "{reason}");
}

#[test]
fn open_owner_questions_alone_do_not_continue_the_turn() {
    let board = turn_end_board();
    let _watcher = lead_watcher(&board);
    board.lead(&["send", "questions", "Should we ship?"]);

    assert_eq!(reminder(&board, stop("lead-reminder", false)), "");
}

#[test]
fn turn_end_mentions_a_waiting_owner_question_without_waiting_on_it() {
    let board = turn_end_board();
    board.lead(&["send", "questions", "Should we ship?"]);
    worker_says(&board, "lead", "Done");

    let answer = reply(&reminder(&board, stop("lead-reminder", false)));

    let reason = answer["reason"].as_str().unwrap();
    assert!(
        reason.contains("1 owner question is waiting on the owner"),
        "{reason}"
    );
}

#[test]
fn turn_end_gives_a_worker_nothing() {
    let board = turn_end_board();
    let worker = worker_says(&board, "lead", "Pending for the lead");

    let output = reminder_with_handover(&board, stop("lead-reminder", false), Some(&worker));

    assert_eq!(output, "");
}

#[test]
fn turn_end_gives_a_chat_that_does_not_lead_the_board_nothing() {
    let board = turn_end_board();
    worker_says(&board, "lead", "Pending for the lead");

    assert_eq!(reminder(&board, stop("another-chat", false)), "");
}

#[test]
fn turn_end_with_a_running_watcher_and_no_pending_mail_says_nothing() {
    let board = turn_end_board();
    let _watcher = lead_watcher(&board);

    assert_eq!(reminder(&board, stop("lead-reminder", false)), "");
}

#[test]
fn turn_end_between_recent_watcher_ticks_says_nothing() {
    let board = turn_end_board();
    finish_lead_tick(&board);

    assert_eq!(reminder(&board, stop("lead-reminder", false)), "");
}

#[test]
fn turn_end_continues_once_to_restart_a_missing_watcher() {
    let board = turn_end_board();

    let answer = reply(&reminder(&board, stop("lead-reminder", false)));

    assert_eq!(
        answer,
        json!({"decision": "block", "reason": RESTART_WATCHER})
    );
}

#[test]
fn turn_end_with_an_active_stop_hook_only_reminds_about_the_watcher() {
    let board = turn_end_board();

    let answer = reply(&reminder(&board, stop("lead-reminder", true)));

    assert_eq!(answer, json!({"systemMessage": RESTART_WATCHER}));
}

#[test]
fn turn_end_hands_over_pending_mail_before_the_watcher_restart_line() {
    let board = turn_end_board();
    worker_says(&board, "lead", "Pending mail comes first");

    let answer = reply(&reminder(&board, stop("lead-reminder", false)));

    assert_eq!(answer["decision"], "block");
    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.ends_with(RESTART_WATCHER), "{reason}");
    assert!(
        reason
            .strip_suffix(RESTART_WATCHER)
            .unwrap()
            .contains("Pending mail comes first")
    );
}

#[test]
fn turn_end_can_continue_for_a_new_watcher_outage_after_recovery() {
    let board = turn_end_board();
    let stop = stop("lead-reminder", false);
    assert_eq!(reply(&reminder(&board, stop.clone()))["decision"], "block");
    let watcher = lead_watcher(&board);
    assert!(reminder(&board, stop.clone()).is_empty());
    drop(watcher);

    let answer = reply(&reminder(&board, stop));

    assert_eq!(answer["decision"], "block");
}

fn generic_turn_end() -> Value {
    json!({"event": "turn_end", "session": "lead-reminder"})
}

#[test]
fn the_generic_format_gets_a_continue_reply_for_a_missing_watcher() {
    let board = turn_end_board();

    let answer = reply(&reminder(&board, generic_turn_end()));

    assert_eq!(
        answer,
        json!({"decision": "continue", "context": RESTART_WATCHER})
    );
}

#[test]
fn a_second_generic_turn_end_with_the_same_watcher_outage_only_reminds() {
    let board = turn_end_board();
    let _ = reminder(&board, generic_turn_end());

    let answer = reply(&reminder(&board, generic_turn_end()));

    assert_eq!(answer, json!({"systemMessage": RESTART_WATCHER}));
}

#[test]
fn changing_pending_mail_does_not_rearm_the_watcher_restart() {
    let board = turn_end_board();
    let _ = reminder(&board, generic_turn_end());
    worker_says(&board, "lead", "New mail during the outage");
    let with_mail = reply(&reminder(&board, generic_turn_end()));
    assert_eq!(with_mail["decision"], "continue");
    board.lead(&["read", "--ack", "lead"]);

    let answer = reply(&reminder(&board, generic_turn_end()));

    assert_eq!(answer, json!({"systemMessage": RESTART_WATCHER}));
}

#[test]
fn a_long_pending_message_is_shortened_within_the_32_kib_limit() {
    let board = turn_end_board();
    let worker = board.enrol("turn-worker", &["lead:send"]);
    let mut command = participant_command(&worker, &["send", "lead", "-"]);
    command.stdin(Stdio::piped()).stdout(Stdio::null());
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all("x".repeat(60 * 1024).as_bytes()).unwrap();
    drop(stdin);
    assert!(child.wait().unwrap().success());

    let answer = reply(&reminder(&board, stop("lead-reminder", false)));

    let reason = answer["reason"].as_str().unwrap();
    assert!(reason.len() <= 32 * 1024, "{}", reason.len());
    assert!(reason.contains("[shortened: "), "{reason}");
    assert!(reason.ends_with(RESTART_WATCHER));
}

const WITHOUT_HOOK: &str = r#"
import base64, json, sys
manifest, event = sys.argv[1:]
state = json.load(open(manifest))
for change in state["settings"]:
    change["added"].pop(event)
    doc = json.load(open(change["path"]))
    del doc["hooks"][event]
    data = (json.dumps(doc, indent=2) + "\n").encode()
    open(change["path"], "wb").write(data)
    change["after"] = base64.b64encode(data).decode()
open(manifest, "w").write(json.dumps(state, indent=2) + "\n")
"#;

#[test]
fn a_refresh_adds_the_stop_hook_to_an_install_made_before_it_existed() {
    refresh_adds_hook("Stop");
}

#[test]
fn a_refresh_adds_the_question_answer_hook_to_an_older_install() {
    refresh_adds_hook("PostToolUse");
}

fn refresh_adds_hook(event: &str) {
    let board = fixture();
    run(agents(&board, &["--yes"]));
    let manifest = board.home.path().join(MANIFEST);
    let mut doctor = Command::new("python3");
    doctor.args(["-c", WITHOUT_HOOK]).arg(&manifest).arg(event);
    run(doctor);
    assert!(
        settings(&board, CLAUDE_SETTINGS)["hooks"]
            .get(event)
            .is_none()
    );

    let status = agents(&board, &["--yes", "--upgrade-refresh"])
        .status()
        .unwrap();

    assert_eq!(status.code(), Some(10));
    for path in [CLAUDE_SETTINGS, CODEX_HOOKS] {
        let command = settings(&board, path)["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            command.contains(" recipe observe --reminder --tool "),
            "{command}"
        );
    }
}
