use super::*;
use std::io::{BufRead, BufReader};
use std::time::SystemTime;

#[path = "../fixtures/codex_notice.rs"]
mod codex_notice;

const USER_HOOK: &str = r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"keep-me"}]}]},"key":true}"#;
const SKILL_PATH: &str = ".claude/skills/malak/SKILL.md";
const CLAUDE_SETTINGS: &str = ".claude/settings.json";
const CODEX_HOOKS: &str = ".codex/hooks.json";
const MANIFEST: &str = ".malak/agents.json";

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
    for (skill, settings_path) in [
        (SKILL_PATH, CLAUDE_SETTINGS),
        (".codex/skills/malak/SKILL.md", CODEX_HOOKS),
    ] {
        let skill = fs::read_to_string(root.join(skill)).unwrap();
        assert!(skill.starts_with("---\nname: malak\n"));
        assert!(skill.contains("malak recipe --help"));
        let hooks = settings(&board, settings_path);
        for event in ["UserPromptSubmit", "SubagentStart", "PreToolUse", "Stop"] {
            let command = hooks["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(command.ends_with(" recipe observe --reminder"));
        }
        assert!(hooks["hooks"]["PreToolUse"][0].get("matcher").is_none());
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
    assert!(output.contains("Removed UserPromptSubmit"));
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
fn lead_status_reminder_is_quiet_when_all_is_clear() {
    let board = fixture();
    board.start();
    let _watcher = lead_watcher(&board);
    assert!(context(&reminder(&board, prompt())).starts_with("Malak is installed"));
    expire_status_check(&board);

    assert!(reminder(&board, tool_use()).is_empty());
}

#[test]
fn lead_status_reminder_reports_waiting_lead_messages_in_one_line() {
    let board = fixture();
    board.start();
    let _watcher = lead_watcher(&board);
    board.lead(&["send", "lead", "First waiting message"]);
    board.lead(&["send", "lead", "Second waiting message"]);

    let notice = context(&reminder(&board, tool_use()));

    assert!(
        notice.contains("2 lead messages unacknowledged (oldest"),
        "{notice}"
    );
    assert_eq!(notice.lines().count(), 1);
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
    let mut command = board.recipe("lead-up");
    command
        .arg("--config")
        .arg(&board.config)
        .env("CODEX_SESSION_ID", id);
    run(command);
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
        notice.starts_with(r#"Malak board "malak": 1 lead message unacknowledged"#),
        "{notice}"
    );
    assert!(!notice.contains("review"), "{notice}");
    assert!(!notice.contains("watcher is not running"), "{notice}");
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
    assert!(!notice.contains("malak"), "{notice}");
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
    assert!(context(&reminder(&board, prompt())).contains("watcher is not running"));

    assert!(reminder(&board, tool_use()).is_empty());
    expire_status_check(&board);
    assert!(context(&reminder(&board, tool_use())).contains("watcher is not running"));
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

    assert!(!notice.contains("watcher is not running"), "{notice}");
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

    assert!(notice.contains("watcher is not running"), "{notice}");
}

#[test]
fn lead_status_reminder_notices_when_its_watcher_exits() {
    let board = fixture();
    board.start();
    let watcher = lead_watcher(&board);
    assert!(!context(&reminder(&board, prompt())).contains("watcher is not running"));
    drop(watcher);
    expire_status_check(&board);

    let notice = context(&reminder(&board, tool_use()));

    assert!(notice.contains("watcher is not running"), "{notice}");
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

    assert!(!notice.contains("watcher is not running"), "{notice}");
    assert!(
        !notice.contains("could not tell whether your watcher is running"),
        "{notice}"
    );
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
fn lead_status_reminder_surfaces_only_owner_questions_past_three_minutes() {
    let board = fixture();
    board.start();
    let _watcher = lead_watcher(&board);
    let worker = board.enrol("alice", &["questions:send"]);
    run(participant_command(
        &worker,
        &["send", "questions", "Agent question."],
    ));
    board.lead(&["send", "questions", "Owner question."]);
    assert!(reminder(&board, tool_use()).is_empty());
    expire_status_check(&board);
    fs::write(
        board.home.path().join("sitecustomize.py"),
        "import time\nreal_time = time.time\ntime.time = lambda: real_time() + 181\n",
    )
    .unwrap();

    let notice = context(&reminder(&board, tool_use()));

    assert!(
        notice.contains("1 owner question open (oldest 3 min)"),
        "{notice}"
    );
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

    let output = reminder_with_handover(&board, stop("worker-chat", false), Some(&worker));

    assert_eq!(output, "");
}

#[test]
fn turn_end_gives_a_chat_that_does_not_lead_the_board_nothing() {
    let board = turn_end_board();
    worker_says(&board, "lead", "Pending for the lead");

    assert_eq!(reminder(&board, stop("another-chat", false)), "");
}

#[test]
fn turn_end_with_nothing_pending_says_nothing() {
    let board = turn_end_board();

    assert_eq!(reminder(&board, stop("lead-reminder", false)), "");
}

#[test]
fn the_generic_format_gets_a_continue_reply() {
    let board = turn_end_board();
    worker_says(&board, "lead", "Generic pending");

    let answer = reply(&reminder(
        &board,
        json!({"event": "turn_end", "session": "lead-reminder"}),
    ));

    assert_eq!(answer["decision"], "continue");
    assert!(
        answer["context"]
            .as_str()
            .unwrap()
            .contains("Generic pending")
    );
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
}

const WITHOUT_STOP: &str = r#"
import base64, json, sys
manifest = sys.argv[1]
state = json.load(open(manifest))
for change in state["settings"]:
    change["added"].pop("Stop")
    doc = json.load(open(change["path"]))
    del doc["hooks"]["Stop"]
    data = (json.dumps(doc, indent=2) + "\n").encode()
    open(change["path"], "wb").write(data)
    change["after"] = base64.b64encode(data).decode()
open(manifest, "w").write(json.dumps(state, indent=2) + "\n")
"#;

#[test]
fn a_refresh_adds_the_stop_hook_to_an_install_made_before_it_existed() {
    let board = fixture();
    run(agents(&board, &["--yes"]));
    let manifest = board.home.path().join(MANIFEST);
    let mut doctor = Command::new("python3");
    doctor.args(["-c", WITHOUT_STOP]).arg(&manifest);
    run(doctor);
    assert!(
        settings(&board, CLAUDE_SETTINGS)["hooks"]
            .get("Stop")
            .is_none()
    );

    let status = agents(&board, &["--yes", "--upgrade-refresh"])
        .status()
        .unwrap();

    assert_eq!(status.code(), Some(10));
    for path in [CLAUDE_SETTINGS, CODEX_HOOKS] {
        let command = settings(&board, path)["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(command.ends_with(" recipe observe --reminder"), "{command}");
    }
}
