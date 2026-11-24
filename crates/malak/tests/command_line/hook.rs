use super::*;
use serde_json::json;
use std::io::Write;
use std::process::Output;

const AGENT_TOOLS: [&str; 2] = ["claude", "codex"];

struct Hook {
    board: Fixture,
    inbox: String,
    handover: PathBuf,
    tool: &'static str,
}

impl Hook {
    fn new(tool: &'static str) -> Self {
        Self::with_board(tool, Fixture::new())
    }

    fn with_board(tool: &'static str, board: Fixture) -> Self {
        let (inbox, handover) = board.enrol("hook-worker");
        Self {
            board,
            inbox,
            handover,
            tool,
        }
    }

    fn for_each_tool(test: impl Fn(Self)) {
        for tool in AGENT_TOOLS.into_iter().chain(["generic"]) {
            test(Self::new(tool));
        }
    }

    fn command(&self, mut command: Command) -> Command {
        command.args(["hook", "--tool", self.tool]);
        self.environment(command)
    }

    fn environment(&self, mut command: Command) -> Command {
        command
            // A nonexistent directory leaves no executables on PATH.
            .env("PATH", self.board.socket.path().join("empty-path"))
            .env("MALAK_HANDOVER", &self.handover)
            .env("MALAK_HOOK_INBOX", &self.inbox)
            .env("MALAK_HOOK_STATE", self.state());
        command
    }

    fn state(&self) -> PathBuf {
        self.board.socket.path().join("hook")
    }

    fn probe_channel(&self) -> String {
        format!("selftest/{}", self.inbox.strip_prefix("to/").unwrap())
    }

    fn grant_probe(&self, actions: &str) {
        let id = self.inbox.strip_prefix("to/").unwrap();
        let channel = self.probe_channel();
        run(&mut self.board.owner_command(&["channel", "create", &channel]));
        run(&mut self.board.owner_command(&[
            "grant",
            id,
            "lead:send",
            &format!("{}:read,get,ack", self.inbox),
            &format!("{channel}:{actions}"),
        ]));
    }

    fn check(&self, tool: Option<&str>) -> Output {
        self.check_with(Command::new(MALAK), tool)
    }

    fn check_with(&self, command: Command, tool: Option<&str>) -> Output {
        self.check_command(command, tool).output().unwrap()
    }

    fn check_command(&self, command: Command, tool: Option<&str>) -> Command {
        let mut command = self.environment(command);
        command.args(["hook", "--check"]);
        if let Some(tool) = tool {
            command.args(["--tool", tool]);
        }
        command
    }

    fn send(&self, body: &str) {
        call(
            &self.board.lead(),
            &["send", &self.inbox, "--kind", "task", body],
        );
    }

    fn invoke(&self, event: &str) -> Output {
        self.invoke_with(Command::new(MALAK), event, false)
    }

    fn invoke_in(&self, session: &str, event: &str) -> Output {
        self.invoke_session(Command::new(MALAK), session, event, false)
    }

    fn invoke_with(&self, command: Command, event: &str, close_stdout: bool) -> Output {
        self.invoke_session(command, "session", event, close_stdout)
    }

    fn invoke_session(
        &self,
        command: Command,
        session: &str,
        event: &str,
        close_stdout: bool,
    ) -> Output {
        invoke(
            &mut self.command(command),
            &self.input(event, session),
            close_stdout,
        )
    }

    fn input(&self, event: &str, session: &str) -> Value {
        if self.tool == "generic" {
            let event = match event {
                "SessionStart" => "session_start",
                "UserPromptSubmit" => "prompt",
                "PostToolUse" => "step",
                "SessionEnd" => "session_end",
                _ => event,
            };
            json!({"event": event, "session": session})
        } else {
            agent_input(event, session)
        }
    }

    fn due(&self) -> Vec<Value> {
        messages(&call(&self.handover, &["read", &self.inbox, "--json"]))
    }

    fn context(&self, output: &Output, event: &str) -> String {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.output_context(output, event)
    }

    fn output_context(&self, output: &Output, event: &str) -> String {
        let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
        if self.tool == "generic" {
            let body = reply["context"].as_str().unwrap();
            assert_eq!(reply, json!({"context": body}));
            return body.to_owned();
        }
        let body = reply["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert_eq!(
            reply,
            json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": body}})
        );
        body.to_owned()
    }
}

fn agent_input(event: &str, session: &str) -> Value {
    json!({"hook_event_name": event, "session_id": session})
}

fn invoke(command: &mut Command, input: &Value, close_stdout: bool) -> Output {
    start_hook(command, input, close_stdout)
        .wait_with_output()
        .unwrap()
}

fn start_hook(command: &mut Command, input: &Value, close_stdout: bool) -> Child {
    let mut process = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if close_stdout {
        drop(process.stdout.take());
    }
    writeln!(process.stdin.take().unwrap(), "{input}").unwrap();
    process
}

#[test]
fn session_end_keeps_a_message_due_when_no_event_followed_it() {
    Hook::for_each_tool(|hook| {
        hook.send("first");
        let shown = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");
        assert!(shown.contains("\nfirst\nEND MALAK-"));

        let ended = hook.invoke("SessionEnd");

        assert!(ended.status.success(), "{:?}", ended.stderr);
        if hook.tool == "generic" {
            assert_eq!(hook.context(&ended, "SessionEnd"), "");
        } else {
            assert!(ended.stdout.is_empty());
        }
        let due = hook.due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0]["text"], "first");
    });
}

#[test]
fn a_message_kept_due_by_session_end_is_shown_in_the_next_session() {
    Hook::for_each_tool(|hook| {
        hook.send("first");
        let _ = hook.context(&hook.invoke("SessionStart"), "SessionStart");
        let _ = hook.invoke("SessionEnd");

        let shown = hook.context(&hook.invoke_in("next", "SessionStart"), "SessionStart");

        assert!(shown.contains("\nfirst\nEND MALAK-"));
    });
}

#[test]
fn a_later_step_acknowledges_the_previous_delivery() {
    Hook::for_each_tool(|hook| {
        hook.send("first");
        hook.send("second");
        let _ = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");
        let shown = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");

        assert!(shown.contains("\nsecond\nEND MALAK-"));
        let due = hook.due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0]["text"], "second");
    });
}

#[test]
fn the_hook_shows_one_message_at_a_time_in_inbox_order() {
    Hook::for_each_tool(|hook| {
        let events = [
            ("SessionStart", "first"),
            ("UserPromptSubmit", "second"),
            ("PostToolUse", "third"),
        ];
        for (_, body) in events {
            hook.send(body);
        }

        for (event, body) in events {
            let shown = hook.context(&hook.invoke(event), event);
            assert!(shown.contains(&format!("\n{body}\nEND MALAK-")));
            assert_eq!(shown.matches("\nBEGIN MALAK-").count(), 1);
        }
    });
}

#[test]
fn the_hook_context_is_bounded_by_the_agent_presentation_budget() {
    Hook::for_each_tool(|hook| {
        hook.send(&"🚀".repeat(10_000));

        let shown = hook.context(&hook.invoke("SessionStart"), "SessionStart");

        assert!(shown.len() <= 32 * 1024);
        assert!(shown.contains("Body bytes 40000 (utf8)"));
        assert!(shown.contains("Fetch more with malak get --agent --offset "));
    });
}

#[test]
fn the_hook_reoffers_a_message_when_acknowledgement_is_refused() {
    Hook::for_each_tool(|hook| {
        hook.send("still due");
        let _ = hook.context(&hook.invoke("SessionStart"), "SessionStart");
        let participant = hook.inbox.strip_prefix("to/").unwrap();
        let grant = format!("{}:read,get", hook.inbox);
        run(&mut hook.board.owner_command(&["grant", participant, &grant]));

        let next = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");

        assert!(next.contains("\nstill due\nEND MALAK-"));
    });
}

#[test]
fn the_hook_ignores_other_events_without_hook_environment() {
    for tool in AGENT_TOOLS {
        let result = invoke(
            Command::new(MALAK)
                .env_clear()
                .args(["hook", "--tool", tool]),
            &agent_input("PreToolUse", "session"),
            false,
        );

        assert!(result.status.success(), "{:?}", result.stderr);
        assert!(result.stdout.is_empty());
    }
}

#[test]
fn a_generic_hook_with_no_waiting_message_returns_empty_context() {
    let hook = Hook::new("generic");

    assert_eq!(
        hook.context(&hook.invoke("SessionStart"), "SessionStart"),
        ""
    );
}

#[test]
fn a_fake_tool_can_configure_its_hooks_from_the_description() {
    let description = run(Command::new(MALAK)
        .env_clear()
        .args(["hook", "--describe", "--json"]));
    let description: Value = serde_json::from_str(&description).unwrap();
    let events = description["events"].as_array().unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(description["context_byte_limit"], 32768);

    let hook = Hook::new("generic");
    let worker = hook.environment(Command::new(MALAK));
    let settings = description["environment"].as_array().unwrap();
    let environment = settings.iter().map(|setting| {
        let name = setting["name"].as_str().unwrap();
        worker.get_envs().find(|(key, _)| *key == name).unwrap()
    });
    for (index, event) in events.iter().enumerate() {
        let mut words = event["command"].as_str().unwrap().split_whitespace();
        assert_eq!(words.next(), Some("malak"));
        let mut command = Command::new(MALAK);
        command.env_clear().args(words);
        for (name, value) in environment.clone() {
            command.env(name, value.unwrap());
        }

        let mut expected = event["output"].clone();
        if expected["context"].as_str() != Some("") {
            hook.send(&format!("message {index}"));
        }
        let output = invoke(&mut command, &event["input"], false);
        let context = hook.context(&output, "");
        if expected["context"].as_str() != Some("") {
            assert!(context.contains(&format!("\nmessage {index}\nEND MALAK-")));
            expected["context"] = Value::String(context);
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            expected
        );
    }
    // Nothing followed the step's message, so session_end left it due.
    let due = hook.due();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0]["text"], "message 2");
}

#[test]
fn describe_prints_plain_text_by_default() {
    let output = run(Command::new(MALAK).env_clear().args(["hook", "--describe"]));

    assert!(output.starts_with("Malak hook contract for the generic tool\n"));
    assert_eq!(
        output.matches("Command  malak hook --tool generic").count(),
        4
    );
    assert!(output.contains("event came after it in the same session"));
}

#[test]
fn describe_includes_questions_in_the_lead_read_loop() {
    let command = "malak mailbox --plain --for lead --next";
    let fallback = "malak read --wait 60 lead questions";
    let plain = run(Command::new(MALAK).env_clear().args(["hook", "--describe"]));
    assert!(plain.contains(command));
    assert!(plain.contains(fallback));

    let output = run(Command::new(MALAK)
        .env_clear()
        .args(["hook", "--describe", "--json"]));
    let description: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        description["monitor"],
        json!({"command": command, "fallback": fallback})
    );
}

#[test]
fn describe_explains_the_lead_watchers_stuck_work() {
    let output = run(Command::new(MALAK).env_clear().args(["hook", "--describe"]));
    assert!(output.contains("malak mailbox --plain --for lead"));
    assert!(output.contains("follows new messages on lead, questions, and orders"));
    assert!(output.contains("unacknowledged after 15 minutes"));
}

#[test]
fn check_rejects_the_description_only_json_flag() {
    let output = Command::new(MALAK)
        .args(["hook", "--check", "--json"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}

#[test]
fn check_delivers_and_acknowledges_its_probe() {
    Hook::for_each_tool(|hook| {
        hook.grant_probe("send,read,ack");

        let result = hook.check((hook.tool != "generic").then_some(hook.tool));

        assert!(result.status.success(), "{:?}", result.stderr);
        let output = String::from_utf8(result.stdout).unwrap();
        assert!(output.starts_with("Hook probe received and acknowledged."));
        assert!(output.contains(&hook.probe_channel()));
        assert!(output.contains("verify its configuration"));
        assert!(
            messages(&call(
                &hook.handover,
                &["read", &hook.probe_channel(), "--json"]
            ))
            .is_empty()
        );
    });
}

#[test]
fn check_preserves_the_inbox_messages_and_pending_receipt() {
    let hook = Hook::new("generic");
    hook.grant_probe("send,read,ack");
    hook.send("first");
    hook.send("second");
    let first = hook.context(&hook.invoke("SessionStart"), "SessionStart");
    assert!(first.contains("\nfirst\nEND MALAK-"));

    let result = hook.check(None);

    assert!(result.status.success(), "{:?}", result.stderr);
    assert_eq!(hook.due().len(), 2);
    let second = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");
    assert!(second.contains("\nsecond\nEND MALAK-"));
}

#[test]
fn check_explains_how_the_owner_can_provision_a_missing_probe_channel() {
    let hook = Hook::new("generic");

    let result = hook.check(None);

    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    let channel = hook.probe_channel();
    assert!(
        error.contains(&format!("malak channel create {channel}")),
        "{error}"
    );
    assert!(
        error.contains(&format!("{channel}:send,read,ack")),
        "{error}"
    );
    assert!(result.stdout.is_empty());
}

#[test]
fn check_fails_when_the_probe_acknowledgement_is_refused() {
    let hook = Hook::new("generic");
    hook.grant_probe("send,read");

    let result = hook.check(None);

    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stdout.is_empty());
    assert_eq!(
        messages(&call(
            &hook.handover,
            &["read", &hook.probe_channel(), "--json"]
        ))
        .len(),
        1
    );
}

#[test]
fn check_preserves_unexpected_messages_on_its_probe_channel() {
    let hook = Hook::new("generic");
    hook.grant_probe("send,read,ack");
    let channel = hook.probe_channel();
    call(
        &hook.board.lead(),
        &["send", &channel, "an unexpected note"],
    );

    let result = hook.check(None);

    assert!(!result.status.success());
    let remaining = messages(&call(&hook.handover, &["read", &channel, "--json"]));
    assert_eq!(remaining[0]["text"], "an unexpected note");
}

#[test]
fn a_lead_hook_check_uses_a_channel_separate_from_lead_up() {
    let board = Fixture::new();
    run(&mut board.owner_command(&["channel", "create", "selftest/lead"]));
    run(&mut board.owner_command(&["channel", "create", "selftest/lead-hook"]));
    call(&board.lead(), &["send", "selftest/lead", "lead-up test"]);
    let mut command = Command::new(MALAK);
    command
        .args(["hook", "--check"])
        .env("MALAK_HANDOVER", board.lead())
        .env("MALAK_HOOK_INBOX", "lead")
        .env("MALAK_HOOK_STATE", board.socket.path().join("hook"));

    let output = run(&mut command);

    assert!(output.contains("selftest/lead-hook"));
    assert_eq!(
        messages(&call(&board.lead(), &["read", "selftest/lead", "--json"])).len(),
        1
    );
}

mod failure {
    use super::*;
    use malak_testkit::tasks;
    use std::fs;
    use std::net::Shutdown;
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Instant;

    fn block_receipt_writes() -> Command {
        let mut limited = Command::new("/bin/sh");
        // Pipes still carry output while receipt writes hit the file-size limit.
        limited.args([
            "-c",
            "ulimit -c 0 && ulimit -f 0 && exec \"$@\"",
            "hook-kill",
            MALAK,
        ]);
        limited
    }

    #[test]
    fn check_recovers_a_probe_left_waiting_after_an_interrupted_check() {
        let hook = Hook::new("generic");
        hook.grant_probe("send,read,ack");
        let interrupted = hook.check_with(block_receipt_writes(), None);
        assert_eq!(interrupted.status.signal(), Some(nix::libc::SIGXFSZ));

        let retry = hook.check(None);

        assert!(retry.status.success(), "{:?}", retry.stderr);
        assert!(
            messages(&call(
                &hook.handover,
                &["read", &hook.probe_channel(), "--json"]
            ))
            .is_empty()
        );
    }

    #[test]
    fn check_acknowledges_its_probe_after_a_session_end_timeout() {
        const WATCHDOG: Duration = Duration::from_secs(120);
        tasks::spawn(|| {
            let hook = Hook::new("codex");
            hook.grant_probe("send,read,ack");
            arm_session_end(&hook);

            let socket = hook.board.socket.path().join("d/p/s");
            let board_socket = socket.with_file_name("board");
            fs::rename(&socket, &board_socket).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();

            let mut command = hook.check_command(Command::new(MALAK), None);
            let check = tasks::spawn(move || command.output().unwrap());

            // The check sends and reads the probe before opening a new link to ACK it.
            for _ in 0..2 {
                let (client, _) = listener.accept().unwrap();
                relay(client, &board_socket);
            }
            let (ack, _) = listener.accept().unwrap();

            // Hold both ACKs until the native hook's shorter budget actually expires.
            let ended = hook.invoke("SessionEnd");
            assert!(board_call_timed_out(&ended), "{:?}", ended);

            drop(listener);
            fs::rename(board_socket, &socket).unwrap();
            relay(ack, &socket);

            let result = check.wait(WATCHDOG).expect("hook check did not exit");
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                messages(&call(
                    &hook.handover,
                    &["read", &hook.probe_channel(), "--json"]
                ))
                .is_empty()
            );
        })
        .wait(WATCHDOG)
        .expect("probe acknowledgement did not complete before the watchdog");
    }

    fn arm_session_end(hook: &Hook) {
        hook.send("already shown");
        let shown = hook.context(&hook.invoke("SessionStart"), "SessionStart");
        assert!(shown.contains("\nalready shown\nEND MALAK-"));

        // Model a step interrupted after marking the receipt and before its ACK.
        let receipt = fs::read_dir(hook.state())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .unwrap();
        let mut value: Value = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        value["ran"] = json!(true);
        fs::write(receipt, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn relay(mut client: UnixStream, socket: &Path) {
        let mut board = UnixStream::connect(socket).unwrap();
        let mut from_client = client.try_clone().unwrap();
        let mut to_board = board.try_clone().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = std::io::copy(&mut from_client, &mut to_board);
                let _ = to_board.shutdown(Shutdown::Write);
            });
            let _ = std::io::copy(&mut board, &mut client);
            let _ = client.shutdown(Shutdown::Write);
        });
    }

    #[test]
    fn the_hook_reoffers_a_message_after_output_is_interrupted() {
        Hook::for_each_tool(|hook| {
            hook.send("still due");

            let failed = hook.invoke_with(Command::new(MALAK), "PostToolUse", true);

            assert!(!failed.status.success());
            let next = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");
            assert!(next.contains("\nstill due\nEND MALAK-"));
        });
    }

    #[test]
    fn the_hook_reoffers_a_message_after_dying_while_recording_its_receipt() {
        Hook::for_each_tool(|hook| {
            hook.send("shown before the kill");
            let failed = hook.invoke_with(block_receipt_writes(), "PostToolUse", false);

            assert_eq!(failed.status.signal(), Some(nix::libc::SIGXFSZ));
            assert!(
                hook.output_context(&failed, "PostToolUse")
                    .contains("\nshown before the kill\n")
            );
            let next = hook.context(&hook.invoke("PostToolUse"), "PostToolUse");
            assert!(next.contains("\nshown before the kill\n"));
        });
    }

    #[test]
    fn session_end_retries_an_acknowledgement_after_a_failed_step() {
        let watchdog = Instant::now() + Duration::from_secs(180);
        tasks::spawn(move || {
            Hook::for_each_tool(|hook| {
                hook.send("already shown");
                let shown = hook.context(&hook.invoke("SessionStart"), "SessionStart");
                assert!(shown.contains("\nalready shown\nEND MALAK-"));
                let socket = hook.board.socket.path().join("d/p/s");
                let hidden_socket = socket.with_file_name("hidden");
                std::fs::rename(&socket, &hidden_socket).unwrap();
                let listener = UnixListener::bind(&socket).unwrap();

                let mut step = start_hook(
                    &mut hook.command(Command::new(MALAK)),
                    &hook.input("PostToolUse", "session"),
                    false,
                );
                // The step records that the agent ran before it connects to ACK.
                // Keep the real board out of reach until that step has exited.
                let (_attachment, _) = listener.accept().unwrap();
                step.kill().unwrap();
                assert_eq!(step.wait().unwrap().signal(), Some(nix::libc::SIGKILL));

                drop(listener);
                std::fs::rename(hidden_socket, socket).unwrap();

                loop {
                    let retry = hook.invoke("SessionEnd");
                    let error = String::from_utf8_lossy(&retry.stderr);
                    if retry.status.success() {
                        assert!(error.is_empty(), "{}: {error}", hook.tool);
                        break;
                    }
                    // SessionEnd has a short host budget. If the reply times out,
                    // acknowledging the same receipt again is safe.
                    assert!(
                        board_call_timed_out(&retry),
                        "{}: {}: {error}",
                        hook.tool,
                        retry.status
                    );
                    assert!(Instant::now() < watchdog, "{}: {error}", hook.tool);
                    eprintln!("{}: SessionEnd board call timed out, retrying", hook.tool);
                }
                assert!(hook.due().is_empty(), "{}: receipt is still due", hook.tool);
            });
        })
        .wait(watchdog.saturating_duration_since(Instant::now()))
        .expect("hook acknowledgement did not complete before the watchdog");
    }

    fn board_call_timed_out(output: &Output) -> bool {
        output.status.code() == Some(1)
            && String::from_utf8_lossy(&output.stderr)
                .lines()
                .any(|line| line.trim() == "Reason  board call timed out")
    }
}

#[test]
fn agent_hooks_refuse_hostile_inbox_aliases_for_each_disk_store() {
    for store in ["markdown", "sqlite"] {
        for tool in AGENT_TOOLS {
            let mut hook = Hook::with_board(tool, Fixture::with_store(store, launch));
            hook.send("only for the canonical inbox");
            let canonical = hook.inbox.clone();
            for invalid in [
                format!("{canonical}/../lead"),
                canonical.to_uppercase(),
                format!("{canonical}\u{1b}[2J"),
                format!("{canonical}/{}", "a".repeat(129)),
            ] {
                hook.inbox = invalid;
                let rejected = hook.invoke("SessionStart");
                assert!(!rejected.stderr.contains(&0x1b), "{store} {tool}");
                assert!(
                    !rejected
                        .stdout
                        .windows(9)
                        .any(|bytes| bytes == b"canonical"),
                    "{store} {tool}"
                );
            }
            hook.inbox = canonical;
            let delivered = hook.context(&hook.invoke("SessionStart"), "SessionStart");
            assert!(
                delivered.contains("only for the canonical inbox"),
                "{store} {tool}"
            );
        }
    }
}
