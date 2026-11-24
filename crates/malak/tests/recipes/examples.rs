//! Run the exact commands from the guide and check the results they describe.

use super::*;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::collections::BTreeSet;
use std::os::unix::process::CommandExt;
use terminal::{TerminalOutput, terminal_pair, terminal_text};

#[path = "../../src/cli/examples.rs"]
mod guide;

struct Example {
    board: Fixture,
    code: &'static str,
}

impl Example {
    fn stopped(name: &str) -> Self {
        Self {
            board: Fixture::new(),
            code: guide::snippet(name),
        }
    }

    fn running(name: &str) -> Self {
        let example = Self::stopped(name);
        run(example.shell("malak board start --detach"));
        example
    }

    fn shell(&self, code: &str) -> Command {
        let mut command = Command::new("sh");
        for (name, _) in env::vars_os() {
            if name.as_encoded_bytes().starts_with(b"MALAK_") {
                command.env_remove(name);
            }
        }
        let root = self.board.home.path();
        let bin = Path::new(MALAK).parent().unwrap();
        let path = env::var_os("PATH").unwrap();
        let path = env::join_paths(
            [root.to_path_buf(), bin.to_path_buf()]
                .into_iter()
                .chain(env::split_paths(&path)),
        )
        .unwrap();
        command
            .args(["-ec", code])
            .current_dir(root)
            .env("HOME", root)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env("TMPDIR", root)
            .env("XDG_CONFIG_HOME", root.join(".malak"))
            .env("PATH", path);
        command
    }

    fn run(&self) -> String {
        run(self.shell(self.code))
    }

    fn terminal(&self) -> (Watched, TerminalOutput) {
        terminal(self.shell(self.code))
    }

    fn inbox(&self, name: &str) -> String {
        format!(
            "to/{}",
            participant(&self.board.status(), name)["id"]
                .as_str()
                .unwrap()
        )
    }

    fn read(&self, channel: &str) -> Vec<Value> {
        messages(&self.board.lead(&["read", "--json", channel]))
    }

    fn first_on(&self, channel: &str) -> Value {
        serde_json::from_str(&self.board.lead(&["get", "--json", channel, "1"])).unwrap()
    }
}

struct Watched(Child);

impl Drop for Watched {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            let _ = killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGKILL);
            let _ = self.0.wait();
        }
    }
}

fn terminal(mut command: Command) -> (Watched, TerminalOutput) {
    let (master, slave) = terminal_pair(32, 160);
    let child = command
        .env("TERM", "xterm")
        .env("NO_COLOR", "1")
        .process_group(0)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave)
        .spawn()
        .unwrap();
    (Watched(child), TerminalOutput::new(master))
}

fn quit_mailbox(mut child: Watched, mut screen: TerminalOutput) {
    screen.press(b"q");
    screen.wait_for_output("leave alternate screen", |bytes| {
        bytes.windows(8).any(|part| part == b"\x1b[?1049l")
    });
    assert!(child.0.wait().unwrap().success());
}

#[test]
fn a_guest_posts_to_the_receiving_lead_as_itself() {
    let (_, section) = include_str!("../../examples.md")
        .split_once("## Message another board's lead\n")
        .unwrap();
    let (_, section) = section.split_once("\n```sh\n").unwrap();
    let (code, _) = section.split_once("\n```").unwrap();
    let sender = Example::running("send");
    let receiver = sender.board.sibling("review", "r");
    receiver.start();

    run(sender.shell(code));

    let status = receiver.status();
    let guest = participant(&status, "team-to-review");
    let notes = messages(&receiver.lead(&["read", "--json", "lead"]));
    let notes = notes
        .iter()
        .filter(|note| note["sender"] == guest["id"])
        .collect::<Vec<_>>();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["grade"], "proven");
    assert_eq!(notes[0]["text"], "The team build is ready for review.\n");
    assert_eq!(guest["revoked"], true);
    assert!(sender.read("lead").is_empty());
}

macro_rules! examples {
    ($($test:ident: $name:literal [$setup:ident] |$example:ident| $check:block)*) => {
        const TESTED: &[&str] = &[$($name),*];

        $(
            #[test]
            fn $test() {
                let $example = Example::$setup($name);
                $check
            }
        )*
    };
}

examples! {
    send_delivers_a_note: "send" [running] |example| {
        let output = example.run();
        assert!(output.contains("Build finished."), "{output}");
        assert!(output.contains("A second note."), "{output}");
    }

    read_acknowledges_the_displayed_note: "read" [running] |example| {
        let output = example.run();
        assert!(output.contains("Ready for review."), "{output}");
        assert!(example.read("lead").is_empty());
    }

    get_leaves_the_note_waiting_for_read: "get" [running] |example| {
        let output = example.run();
        assert_eq!(output.matches("Keep this note.").count(), 2, "{output}");
    }

    ack_advances_past_the_read_note: "ack" [running] |example| {
        let output = example.run();
        assert!(output.contains("Please confirm."), "{output}");
        assert!(output.contains("No messages returned from lead."), "{output}");
    }

    board_start_waits_for_readiness: "board start" [stopped] |example| {
        example.run();
        assert!(example.board.status()["run"].is_string());
    }

    board_list_describes_the_discovered_board: "board list" [running] |example| {
        let boards: Value = serde_json::from_str(&example.run()).unwrap();
        assert_eq!(boards[0]["status"], "running");
    }

    board_status_shows_the_running_boards_channels: "board status" [running] |example| {
        let output = example.run();
        assert!(output.contains("Board") && output.contains("Channels") && output.contains("lead"), "{output}");
    }

    board_stop_releases_the_store: "board stop" [running] |example| {
        example.run();
        run(example.shell("malak recover"));
    }

    board_stop_all_stops_the_disposable_board: "board stop --all" [running] |example| {
        assert!(example.run().contains("Board stopped."));
        run(example.shell("malak recover"));
    }

    upgrade_previews_the_source_and_board: "upgrade" [running] |example| {
        let root = example.board.home.path();
        let package = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace = package.join("../..");
        fs::create_dir_all(root.join("crates/malak")).unwrap();
        fs::copy(workspace.join("Cargo.toml"), root.join("Cargo.toml")).unwrap();
        fs::copy(package.join("Cargo.toml"), root.join("crates/malak/Cargo.toml")).unwrap();
        let before = example.board.status()["run"].clone();

        let output = example.run();

        assert!(output.contains("Malak upgrade plan"), "{output}");
        assert!(output.contains("Cargo must finish installing"), "{output}");
        assert!(output.contains("Will restart"), "{output}");
        assert_eq!(example.board.status()["run"], before);
    }

    uninstall_dry_run_leaves_the_disposable_board_running: "uninstall" [running] |example| {
        let output = example.run();
        assert!(output.contains("Would stop board."), "{output}");
        assert_eq!(output.matches("Will delete").count(), 2, "{output}");
        assert!(example.board.status()["run"].is_string());
    }

    enrol_writes_a_usable_alice_handover: "enrol" [running] |example| {
        example.run();
        let note = example.first_on("lead");
        assert_eq!(note["sender"], participant(&example.board.status(), "alice")["id"]);
        assert_eq!(note["text"], "Hello from Alice.");
    }

    grant_lets_alice_send_to_lead: "grant" [running] |example| {
        example.run();
        let note = example.first_on("lead");
        assert_eq!(note["sender"], participant(&example.board.status(), "alice")["id"]);
        assert_eq!(note["text"], "Grant works.");
    }

    rekey_replaces_alices_working_key: "rekey" [running] |example| {
        example.run();
        assert_eq!(example.first_on("lead")["text"], "New key works.");
        let old = output(example.shell(r#"MALAK_HANDOVER="$PWD/alice.key" malak send lead retired"#));
        assert!(!old.status.success(), "{old:?}");
    }

    release_shows_alices_claim_as_released: "release" [running] |example| {
        example.run();
        assert_eq!(participant(&example.board.status(), "alice")["claim"]["state"], "released");
    }

    revoke_shows_alice_as_revoked: "revoke" [running] |example| {
        let output = example.run();
        assert!(output.contains("Revoked"), "{output}");
        assert_eq!(participant(&example.board.status(), "alice")["revoked"], true);
    }

    channel_create_accepts_the_first_note: "channel create" [running] |example| {
        example.run();
        assert_eq!(example.first_on("updates")["text"], "First update.");
    }

    channel_remove_makes_the_scratch_channel_absent: "channel remove" [running] |example| {
        example.run();
        let status = example.board.status();
        let channels = status["channels"].as_array().unwrap();
        assert!(channels.iter().all(|channel| channel["name"] != "scratch"));
    }

    trim_keeps_only_the_note_at_the_new_floor: "trim" [running] |example| {
        let output = example.run();
        assert!(output.contains("Keep this note."), "{output}");
        assert!(!output.contains("Old note."), "{output}");
        let notes = example.read("lead");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0]["position"], 2);
    }

    tail_prints_a_live_event_for_lead: "tail" [running] |example| {
        let (_child, mut screen) = example.terminal();
        screen.wait_for("Following events for lead");
        example.board.lead(&["send", "lead", "Tail example note."]);
        screen.wait_for("Note stored in lead #1");
    }

    recover_inspects_the_stopped_markdown_board: "recover" [running] |example| {
        let output = example.run();
        assert!(output.contains("No stopped channels."), "{output}");
    }

    hook_delivers_a_waiting_note_as_generic_session_context: "hook" [running] |example| {
        let output = example.run();
        let reply: Value = serde_json::from_str(output.lines().last().unwrap()).unwrap();
        assert!(reply["context"].as_str().unwrap().contains("Check the build."), "{reply}");
    }

    mailbox_opens_a_live_board_and_activity_view: "mailbox" [running] |example| {
        let (child, mut screen) = example.terminal();
        screen.wait_for("BOARDS");
        screen.wait_for("Activity");
        example.board.lead(&["send", "lead", "Live example note."]);
        screen.wait_for("@lead → @lead · note #1");
        quit_mailbox(child, screen);
    }

    mailbox_plain_streams_a_note_without_opening_the_alternate_screen: "mailbox --plain" [running] |example| {
        let (_child, mut screen) = example.terminal();
        screen.wait_for("Mailbox activity");
        example.board.lead(&["send", "lead", "Plain example note."]);
        screen.wait_for("Plain example note.");
        assert!(!screen.bytes.windows(b"\x1b[?1049h".len()).any(|window| window == b"\x1b[?1049h"));
    }

    mailbox_next_can_wait_for_a_second: "mailbox --next" [running] |example| {
        assert_eq!(example.run(), "No events.\n");
    }

    mailbox_for_filters_activity_to_alice: "mailbox --for" [running] |example| {
        let (child, mut screen) = example.terminal();
        screen.wait_for("Activity · @alice");
        example.board.lead(&["send", "lead", "Unrelated note."]);
        example.board.lead(&["send", &example.inbox("alice"), "Note for Alice."]);
        screen.wait_for("@lead → @alice · note #1");
        assert!(!terminal_text(&screen.bytes).contains("@lead→@lead·note#1"));
        quit_mailbox(child, screen);
    }

    mailbox_who_lists_alices_enrolment_and_grant_dates: "mailbox --who" [running] |example| {
        let output = example.run();
        for text in ["alice", "Enrolled", "Grant changed"] {
            assert!(output.contains(text), "{output}");
        }
    }

    mailbox_questions_lists_the_open_question: "mailbox --questions" [running] |example| {
        let output = example.run();
        assert!(output.contains("For the owner") && output.contains("Which build should ship?"), "{output}");
    }

    mailbox_orders_shows_the_order_in_force: "mailbox --orders" [running] |example| {
        let output = example.run();
        assert!(output.contains("in force") && output.contains("The lead may choose the test order."), "{output}");
    }

    mailbox_waiting_shows_alices_unacknowledged_task: "mailbox --waiting" [running] |example| {
        let output = example.run();
        assert!(output.lines().any(|line| line.starts_with("alice ") && line.ends_with("#1 task")), "{output}");
    }

    mailbox_trace_shows_the_task_on_its_known_trace: "mailbox --trace" [running] |example| {
        let (_child, mut screen) = example.terminal();
        screen.wait_for("Trace 0123456789abcdef0123456789abcdef");
        screen.wait_for("Run the checks.");
    }

    recipe_lists_all_shipped_names_without_a_board: "recipe" [stopped] |example| {
        let output = example.run();
        let names: Vec<_> = guide::snippets()
            .filter_map(|(command, _)| command.strip_prefix("recipe "))
            .collect();
        assert_eq!(output.lines().collect::<Vec<_>>(), names);
    }

    agents_installs_the_user_skill: "recipe agents" [stopped] |example| {
        fs::create_dir(example.board.home.path().join(".claude")).unwrap();
        example.run();
        assert!(example.board.home.path().join(".claude/skills/malak/SKILL.md").is_file());
    }

    agents_md_writes_the_project_instruction_files: "recipe agents-md" [stopped] |example| {
        example.run();
        for file in ["AGENTS.md", "CLAUDE.md"] {
            let text = fs::read_to_string(example.board.home.path().join(file)).unwrap();
            assert!(text.contains(MALAK_NUDGE), "{text}");
        }
    }

    answer_posts_a_reply_linked_to_the_question: "recipe answer" [running] |example| {
        let output = example.run();
        assert!(output.contains("Ship the passing build."), "{output}");
        let answer: Value = serde_json::from_str(&example.board.lead(&["get", "--json", "questions", "2"])).unwrap();
        assert_eq!(answer["headers"]["reply_to"], json!(["questions", example.first_on("questions")["creation"], 1]));
    }

    ask_records_the_question_options_and_recommendation: "recipe ask" [running] |example| {
        let output = example.run();
        for text in ["Which build should ship?", "- stable", "- preview", "Recommendation: stable"] {
            assert!(output.contains(text), "{output}");
        }
    }

    demo_coordinates_five_workers_and_collects_their_reports: "recipe demo" [running] |example| {
        let mut command = example.shell(example.code);
        configure_demo_fixture(&example.board, &mut command, "claude", "success");
        let output = run(command);
        assert!(output.contains("outcome.txt"), "{output}");
        assert!(demo_outcome(&example.board).contains("5 of 5 workers reported"));
        assert_eq!(example.read("lead").iter().filter(|record| record["kind"] == "report").count(), 5);
    }

    forward_carries_the_original_source_into_alices_inbox: "recipe forward" [running] |example| {
        let output = example.run();
        let forwarded = example.first_on(&example.inbox("alice"));
        assert!(output.contains("The build is ready."), "{output}");
        assert_eq!(forwarded["headers"]["reply_to"], json!(["lead", example.first_on("lead")["creation"], 1]));
    }

    handoff_passes_the_task_to_bob_on_the_same_trace: "recipe handoff" [running] |example| {
        example.run();
        let original = example.first_on(&example.inbox("alice"));
        let received = example.first_on(&example.inbox("bob"));
        assert_eq!(received["kind"], "task");
        assert!(received["text"].as_str().unwrap().contains("Review the build."));
        assert!(original["headers"]["trace"].is_string());
        assert_eq!(received["headers"]["trace"], original["headers"]["trace"]);
    }

    lead_up_loads_a_working_lead_handover: "recipe lead-up" [stopped] |example| {
        let output = example.run();
        let handover = example.board.state.join("lead.handover");
        assert_eq!(output.trim(), format!("Lead handover: {}", handover.display()));
        assert!(example.board.status()["run"].is_string());
    }

    observe_prints_a_session_start_reminder: "recipe observe" [stopped] |example| {
        let output: Value = serde_json::from_str(&example.run()).unwrap();
        let context = output["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(context.contains("If this chat already leads a board"), "{output}");
    }

    guest_prints_commands_to_send_and_watch: "recipe guest" [running] |example| {
        let output = example.run();
        let inbox = example.inbox("team-to-review");
        let command = |label: &str| output.lines().find_map(|line| {
            line.trim_start().strip_prefix(label).map(str::trim_start)
        }).unwrap_or_else(|| panic!("missing {label} in {output}"));
        run(example.shell(&format!("printf 'Build ready.' | {}", command("Send  "))));
        assert_eq!(example.first_on("lead")["text"], "Build ready.");
        example.board.lead(&["send", &inbox, "Reviewed."]);
        let reply = run(example.shell(command("Watch  ")));
        assert!(reply.contains("Reviewed."), "{reply}");
    }

    order_records_the_scope_and_end_time: "recipe order" [running] |example| {
        let output = example.run();
        for text in ["Until:", "The lead may choose the test order."] {
            assert!(output.contains(text), "{output}");
        }
    }

    queue_starts_the_reviewer_after_forwarding_the_verified_report: "recipe queue" [running] |example| {
        example.run();
        let inbox = example.read(&example.inbox("reviewer"));
        assert_eq!(inbox.len(), 2);
        assert!(inbox[0]["text"].as_str().unwrap().contains("Review the build."));
        assert!(inbox[1]["text"].as_str().unwrap().contains("Research finished."));
        let reports = example.read("lead");
        assert_eq!(reports.len(), 2);
        assert!(reports[1]["text"].as_str().unwrap().contains("Review finished."));
    }

    await_prints_the_builders_final_report: "recipe await" [running] |example| {
        let output = example.run();
        assert!(output.contains("Final report from builder"), "{output}");
        assert!(output.contains("Build finished."), "{output}");
    }

    worker_start_delivers_the_builders_report: "recipe worker-start" [running] |example| {
        let output = example.run();
        let report = example.first_on("lead");
        assert!(output.contains("Build finished."), "{output}");
        assert_eq!(report["kind"], "report");
        assert_eq!(report["sender"], participant(&example.board.status(), "builder")["id"]);
    }

    workers_shows_the_builders_exit: "recipe workers" [running] |example| {
        let output = example.run();
        assert!(output.contains("builder exited"), "{output}");
        assert!(output.contains(&example.inbox("builder")), "{output}");
    }

    wrapper_posts_the_commands_stdout_as_alices_report: "recipe wrapper" [running] |example| {
        let output = example.run();
        let report = example.first_on("lead");
        assert!(output.contains("Build finished."), "{output}");
        assert_eq!(report["kind"], "report");
        assert_eq!(report["sender"], participant(&example.board.status(), "alice")["id"]);
    }
}

#[test]
fn every_documented_example_runs_in_a_test_and_appears_in_help() {
    let board = Example::stopped("recipe");
    let mut names = BTreeSet::new();
    for (name, code) in guide::snippets() {
        assert!(names.insert(name), "duplicate example {name}");
        assert!(code.starts_with("# "), "missing comment in {name}");
        assert!(
            (2..=4).contains(&code.lines().count()),
            "example length: {name}"
        );
        let command = if name.starts_with("mailbox ") {
            "mailbox"
        } else if name == "board stop --all" {
            "board stop"
        } else {
            name
        };
        let help = run(board.shell(&format!("malak {command} --help")));
        assert!(
            help.contains("Example:\n"),
            "missing Example in {command} help"
        );
        assert!(help.contains(code), "snippet absent from {command} help");
    }
    assert_eq!(names, TESTED.iter().copied().collect());
    assert_eq!(names.len(), TESTED.len());

    for (command, snippets) in [
        ("malak --help", &["send"][..]),
        ("malak board --help", &["board list", "board start"][..]),
        (
            "malak channel --help",
            &["channel create", "channel remove"][..],
        ),
    ] {
        let output = run(board.shell(command));
        assert!(output.contains(&guide::help(snippets)), "{output}");
    }
}

#[test]
fn recipe_terminal_lists_every_description_in_one_aligned_column() {
    let example = Example::stopped("recipe");
    let (mut child, mut screen) = example.terminal();
    screen.wait_for("malak recipe NAME --help");
    assert!(child.0.wait().unwrap().success());
    let output = String::from_utf8_lossy(&screen.bytes);
    let names = guide::snippets().filter_map(|(command, _)| command.strip_prefix("recipe "));
    let mut columns = BTreeSet::new();
    for name in names {
        let (_, description) = output
            .lines()
            .find_map(|row| row.strip_prefix("  ")?.strip_prefix(name)?.split_once("  "))
            .unwrap_or_else(|| panic!("missing description for {name}: {output}"));
        let trimmed = description.trim_start();
        assert!(
            trimmed.ends_with('.'),
            "incomplete description for {name}: {description}"
        );
        columns.insert(name.len() + description.len() - trimmed.len());
    }
    assert_eq!(columns.len(), 1, "unaligned descriptions: {output}");
}

#[test]
fn terminal_help_preserves_the_shell_example_verbatim() {
    let example = Example::stopped("recipe");
    for command in ["send", "recipe ask", "recipe agents-md"] {
        let code = guide::snippet(command).replace('\n', "\r\n");
        let (mut child, mut screen) = terminal(example.shell(&format!("malak {command} --help")));
        screen.wait_for_output(&code, |bytes| {
            String::from_utf8_lossy(bytes).contains(&code)
        });
        assert!(child.0.wait().unwrap().success());
    }
}
