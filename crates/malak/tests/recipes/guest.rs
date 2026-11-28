use super::*;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

fn guest_recipe(board: &Fixture, name: &str, handover: &Path) -> Command {
    let mut command = board.recipe("guest");
    command.args([name, "--out"]).arg(handover);
    command.env("TMPDIR", board.home.path());
    command
}

fn enroll_guest(board: &Fixture, name: &str, handover: &Path) -> Value {
    let mut command = guest_recipe(board, name, handover);
    command.arg("--json");
    serde_json::from_str(&run(command)).unwrap()
}

fn call_guest(board: &Fixture, handover: &Path, args: &[&str]) -> Command {
    let mut command = board.owner(args);
    command.env("MALAK_HANDOVER", handover);
    command
}

fn assert_refused(command: Command, reason: &str) {
    let result = output(command);
    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains(reason),
        "{result:?}"
    );
}

struct GuestBoards {
    home: Fixture,
    receiving: Fixture,
    handover: PathBuf,
    inbox: String,
}

impl GuestBoards {
    fn start(auto_register: bool) -> Self {
        let home = Fixture::new();
        let receiving = home.sibling("reviews", "reviews");
        home.start();
        receiving.start();
        let handover = home.home.path().join("cross-board handover");
        let mut guest = guest_recipe(&receiving, "sender", &handover);
        guest.args(["--json", "--config"]).arg(&receiving.config);
        if auto_register {
            guest.env("MALAK_HANDOVER", home.state.join("lead.handover"));
        }
        let info: Value = serde_json::from_str(&run(guest)).unwrap();
        let inbox = info["inbox"].as_str().unwrap().to_owned();
        Self {
            home,
            receiving,
            handover,
            inbox,
        }
    }

    fn home_command(&self, args: &[&str]) -> Command {
        let mut command = self.home.owner(args);
        command.env("MALAK_HANDOVER", self.home.state.join("lead.handover"));
        command
    }

    fn reply(&self, text: &str) -> Value {
        serde_json::from_str(&self.receiving.lead(&["send", "--json", &self.inbox, text])).unwrap()
    }

    fn pending(&self) -> Vec<Value> {
        messages(&run(participant_command(
            &self.handover,
            &["read", "--json", &self.inbox],
        )))
    }

    fn identify_home_session(&self, session: &str) {
        let mut identify = self.home_command(&["send", "lead", "Identify this lead session."]);
        identify.env("CODEX_SESSION_ID", session);
        run(identify);
        run(self.home_command(&["read", "--ack", "lead"]));
    }
}

fn reminder(board: &Fixture, handover: &Path, session: &str, event: &str) -> Value {
    let mut command = board.recipe("observe");
    let payload = json!({"session_id": session, "hook_event_name": event});
    let mut child = command
        .arg("--reminder")
        .env("MALAK_HANDOVER", handover)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{payload}").unwrap();
    let text = stdout(child.wait_with_output().unwrap());
    if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    }
}

#[test]
fn a_guest_ack_reminder_appears_while_a_reply_is_due() {
    let boards = GuestBoards::start(false);
    let context = |session| {
        let output = reminder(
            &boards.receiving,
            &boards.handover,
            session,
            "UserPromptSubmit",
        );
        output["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    };
    assert!(!context("guest-empty").contains("ack --creation"));
    let reply = boards.reply("Handle this before acknowledging.");
    let creation = reply["creation"].as_str().unwrap();
    let position = reply["position"].to_string();
    let ack = format!(
        "MALAK_HANDOVER='{}' malak ack --creation {creation} {} {position}",
        boards.handover.display(),
        boards.inbox
    );
    assert_eq!(
        context("guest-waiting")
            .lines()
            .filter(|line| line.contains(&ack))
            .count(),
        1
    );
    assert_eq!(boards.pending().len(), 1);
    run(participant_command(
        &boards.handover,
        &["ack", "--creation", creation, &boards.inbox, &position],
    ));
    assert!(!context("guest-cleared").contains("ack --creation"));
}

#[test]
fn read_lead_flags_guest_replies_alongside_home_mail() {
    let boards = GuestBoards::start(true);
    boards
        .home
        .lead(&["send", "lead", "A message on the home board."]);
    boards.reply("A reply from the reviews board.");
    let read = run(boards.home_command(&["read", "lead"]));
    assert!(read.contains("A message on the home board."), "{read}");
    assert!(read.contains("guest · reviews #1"), "{read}");
    assert!(read.contains("A reply from the reviews board."), "{read}");
    let records = messages(&run(boards.home_command(&["read", "--json", "lead"])));
    assert_eq!(records[1]["guest_board"], "reviews");
}

#[test]
fn ack_lead_routes_a_guest_reply_to_its_board() {
    let boards = GuestBoards::start(true);
    boards
        .home
        .lead(&["send", "lead", "Keep this local message due."]);
    let receipt = boards.reply("Acknowledge only this reply.");
    run(boards.home_command(&["read", "lead"]));
    run(boards.home_command(&[
        "ack",
        "--creation",
        receipt["creation"].as_str().unwrap(),
        "lead",
        "1",
    ]));

    assert!(boards.pending().is_empty());
    let local = messages(&boards.home.lead(&["read", "--json", "lead"]));
    assert_eq!(local.len(), 1);
    assert_eq!(local[0]["text"], "Keep this local message due.");
}

#[test]
fn inbox_names_read_to_lead_includes_registered_guest_replies() {
    let boards = GuestBoards::start(true);
    boards.home.lead(&["send", "lead", "Local mail."]);
    boards.reply("Guest mail.");

    let records = messages(&run(boards.home_command(&["read", "--json", "to/lead"])));
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["channel"], "lead");
    assert_eq!(records[0]["text"], "Local mail.");
    assert_eq!(records[1]["guest_board"], "reviews");
    assert_eq!(records[1]["channel"], boards.inbox);
    assert_eq!(records[1]["text"], "Guest mail.");
}

#[test]
fn inbox_names_ack_to_lead_uses_the_guest_creation_without_acking_local_mail() {
    let boards = GuestBoards::start(true);
    boards.home.lead(&["send", "lead", "Keep local mail due."]);
    let receipt = boards.reply("Acknowledge guest mail.");
    run(boards.home_command(&["read", "to/lead"]));
    run(boards.home_command(&[
        "ack",
        "--creation",
        receipt["creation"].as_str().unwrap(),
        "to/lead",
        "1",
    ]));

    assert!(boards.pending().is_empty());
    let local = messages(&boards.home.lead(&["read", "--json", "lead"]));
    assert_eq!(local.len(), 1);
    assert_eq!(local[0]["text"], "Keep local mail due.");
}

#[test]
fn read_ack_honors_its_limit_across_home_and_guest_inboxes() {
    let boards = GuestBoards::start(true);
    boards
        .home
        .lead(&["send", "lead", "Read this home message first."]);
    boards.reply("Then read this guest reply.");
    let command = ["read", "--ack", "--json", "--limit", "1", "lead"];
    let first = messages(&run(boards.home_command(&command)));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["text"], "Read this home message first.");
    let second = messages(&run(boards.home_command(&command)));
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["text"], "Then read this guest reply.");
    assert!(boards.pending().is_empty());
}

#[test]
fn an_existing_guest_handover_can_be_registered_on_its_home_board() {
    let boards = GuestBoards::start(false);
    boards.reply("A reply for an existing handover.");
    assert!(!run(boards.home_command(&["read", "lead"])).contains("guest · reviews #1"));
    let mut register = boards.home_command(&["recipe", "guest", "--register-home"]);
    register.arg(&boards.handover);
    run(register);
    assert!(run(boards.home_command(&["read", "lead"])).contains("guest · reviews #1"));
}

#[test]
fn one_shot_lead_watcher_wakes_once_for_a_new_guest_reply() {
    let boards = GuestBoards::start(true);
    let next = ["mailbox", "--plain", "--for", "lead", "--next", "--timeout"];
    let mut watcher = boards.home_command(&[next.as_slice(), &["15"]].concat());
    let mut watcher = RunningWorker(watcher.stdout(Stdio::piped()).spawn().unwrap());
    let tick = boards.home.home.path().join("d/o/lead-next");
    wait_for_event("the home watcher to start", || tick.is_file().then_some(()));
    boards.reply("Wake the home board watcher.");
    let mut read = String::new();
    use std::io::Read;
    watcher
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut read)
        .unwrap();
    assert!(watcher.0.wait().unwrap().success());
    assert!(read.contains("guest · reviews #1"), "{read}");
    assert!(read.contains("Wake the home board watcher."), "{read}");
    let repeated = run(boards.home_command(&[next.as_slice(), &["2"]].concat()));
    assert_eq!(repeated, "No events.\n");
}

#[test]
fn the_plain_lead_stream_prints_a_new_guest_reply() {
    let boards = GuestBoards::start(true);
    let mut command = boards.home_command(&["mailbox", "--plain", "--for", "lead"]);
    let mut watcher = RunningWorker(command.stdout(Stdio::piped()).spawn().unwrap());
    let pipe = BufReader::new(watcher.0.stdout.take().unwrap());
    let (send, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in pipe.lines().map_while(Result::ok) {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    assert!(
        lines
            .recv_timeout(TEST_WATCHDOG)
            .unwrap()
            .contains("Mailbox activity")
    );
    boards.reply("Write this into the plain stream.");
    while !lines
        .recv_timeout(TEST_WATCHDOG)
        .unwrap()
        .contains("guest · reviews #1")
    {}
}

#[test]
fn the_home_lead_reminder_includes_the_guest_ack_command() {
    let boards = GuestBoards::start(true);
    let session = "home-guest-reminder";
    boards.identify_home_session(session);
    let reply = boards.reply("A reply waiting for the home lead.");
    let output = reminder(
        &boards.home,
        &boards.home.state.join("lead.handover"),
        session,
        "UserPromptSubmit",
    );
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    let creation = reply["creation"].as_str().unwrap();
    assert!(context.contains("guest · reviews #1"), "{context}");
    assert!(
        context.contains(&format!("ack --creation {creation} lead 1")),
        "{context}"
    );
}

#[test]
fn turn_end_hands_the_home_lead_the_guest_reply_and_ack_command() {
    let boards = GuestBoards::start(true);
    let session = "home-guest-turn-end";
    boards.identify_home_session(session);
    let reply = boards.reply("Handle this guest reply before stopping.");
    let output = reminder(
        &boards.home,
        &boards.home.state.join("lead.handover"),
        session,
        "Stop",
    );
    let context = output["reason"].as_str().unwrap();
    let creation = reply["creation"].as_str().unwrap();
    assert_eq!(output["decision"], "block");
    assert!(context.contains("guest · reviews #1"), "{context}");
    assert!(
        context.contains("Handle this guest reply before stopping."),
        "{context}"
    );
    assert!(
        context.contains(&format!("ack --creation {creation} lead 1")),
        "{context}"
    );
}

#[test]
fn guest_recipe_prints_the_ack_command_in_text_and_json() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest handover");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    let ack = format!(
        "MALAK_HANDOVER='{}' malak ack --creation CREATION {inbox} POSITION",
        handover.display()
    );
    assert_eq!(guest["ack"], ack);
    let output = run(guest_recipe(&board, "sender", &handover));
    assert!(output.lines().any(|line| {
        line.trim_start()
            .strip_prefix("Ack ")
            .is_some_and(|command| command.trim_start() == ack)
    }));
}

#[test]
fn following_the_printed_guest_commands_acknowledges_each_reply() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest's handover");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    let run_printed = |line: &str| {
        let mut command = Command::new("sh");
        board.recipe_environment(&mut command);
        command
            .env_remove("MALAK_HANDOVER")
            .env("XDG_CONFIG_HOME", board.home.path().join("no-config"))
            .args(["-c", line]);
        run(command)
    };
    for reply in ["First reply.", "Second reply."] {
        let receipt = board.lead(&["send", "--json", inbox, reply]);
        let receipt: Value = serde_json::from_str(&receipt).unwrap();
        let watch = guest["watch"].as_str().unwrap();
        assert!(run_printed(watch).contains(reply));
        let ack = guest["ack"]
            .as_str()
            .unwrap()
            .replace("CREATION", receipt["creation"].as_str().unwrap())
            .replace("POSITION", &receipt["position"].to_string());
        run_printed(&ack);
    }
    let remaining = run(participant_command(&handover, &["read", "--json", inbox]));
    assert!(messages(&remaining).is_empty());
}

#[test]
fn a_guest_gets_and_acknowledges_a_reply_linked_to_its_report() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest handover");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    assert_eq!(guest["handover"], json!(handover));
    assert_eq!(
        fs::metadata(&handover).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Let the guest's claim acceptance occupy the first lead position.
    run(call_guest(&board, &handover, &["read", inbox]));
    let notice = board.lead(&["read", "--json", "--wait", "15", "lead"]);
    assert!(
        messages(&notice)
            .iter()
            .any(|note| note["sender"] == "malakowner"),
        "{notice}"
    );

    run(call_guest(
        &board,
        &handover,
        &["send", "--kind", "report", "lead", "The build is ready."],
    ));
    let notes = messages(&board.lead(&["read", "--json", "lead"]));
    let report = notes
        .iter()
        .find(|note| note["sender"] == guest["id"])
        .expect("the guest sent a report");
    assert_eq!(report["grade"], "proven");
    assert!(
        board
            .lead(&["read", "lead"])
            .lines()
            .any(|line| { line.contains("Reply inbox") && line.ends_with(inbox) })
    );

    let origin = report["creation"].as_str().unwrap();
    let position = report["position"].as_u64().unwrap();
    board.lead(&[
        "send",
        "--reply-to",
        "lead",
        origin,
        &position.to_string(),
        inbox,
        "Reviewed.",
    ]);
    let reply: Value = serde_json::from_str(&run(call_guest(
        &board,
        &handover,
        &["get", "--json", inbox, "1"],
    )))
    .unwrap();
    assert_eq!(
        reply["headers"]["reply_to"],
        json!(["lead", origin, position])
    );
    let read = run(call_guest(
        &board,
        &handover,
        &["read", "--ack", "--json", inbox],
    ));
    assert_eq!(messages(&read)[0]["text"], "Reviewed.");
    let remaining = run(call_guest(&board, &handover, &["read", "--json", inbox]));
    assert!(messages(&remaining).is_empty());
}

#[test]
fn upgrading_a_legacy_guest_preserves_its_handover_and_unread_replies() {
    let board = Fixture::new();
    board.start();
    let handover = board.enrol("sender", &["lead:send"]);
    let original_handover = fs::read(&handover).unwrap();
    let original_id = participant(&board.status(), "sender")["id"].clone();
    let inbox = format!("to/{}", original_id.as_str().unwrap());
    board.lead(&["send", &inbox, "The earlier reply."]);
    assert_refused(
        call_guest(&board, &handover, &["read", &inbox]),
        "grant does not allow the call",
    );

    let guest = enroll_guest(&board, "sender", &handover);
    assert_eq!(guest["id"], original_id);
    assert_eq!(guest["handover"], json!(handover));
    assert_eq!(fs::read(&handover).unwrap(), original_handover);
    assert_eq!(enroll_guest(&board, "sender", &handover), guest);
    let read = run(call_guest(
        &board,
        &handover,
        &["read", "--ack", "--json", &inbox],
    ));
    assert_eq!(messages(&read)[0]["text"], "The earlier reply.");
}

#[test]
fn upgrading_a_guest_refuses_a_different_participants_handover() {
    let board = Fixture::new();
    board.start();
    board.enrol("sender", &["lead:send"]);
    let wrong_handover = board.enrol("other", &["lead:send"]);
    let before = participant(&board.status(), "sender")["grant"].clone();

    assert_refused(
        guest_recipe(&board, "sender", &wrong_handover),
        "use the existing handover for guest",
    );
    assert_eq!(participant(&board.status(), "sender")["grant"], before);
}

#[test]
fn reusing_a_guest_name_restores_its_missing_inbox() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    run(board.owner(&["channel", "remove", inbox]));

    assert_eq!(enroll_guest(&board, "sender", &handover), guest);
    board.lead(&["send", inbox, "A reply in the restored inbox."]);
    let received = run(call_guest(
        &board,
        &handover,
        &["get", "--json", inbox, "1"],
    ));
    let record: Value = serde_json::from_str(&received).unwrap();
    assert_eq!(record["text"], "A reply in the restored inbox.");
}

#[test]
fn the_guest_recipe_refuses_a_worker_name() {
    let board = Fixture::new();
    board.start();
    run(board.worker_start(&[], &["sh", "-c", "true"]));
    let before = participant(&board.status(), "alice")["grant"].clone();
    let handover = board.home.path().join("guest");
    assert_refused(
        guest_recipe(&board, "alice", &handover),
        "already belongs to a worker",
    );
    assert_eq!(participant(&board.status(), "alice")["grant"], before);
    assert!(!handover.exists());
}

#[test]
fn the_guest_recipe_refuses_the_lead_name() {
    let board = Fixture::new();
    board.start();
    assert_refused(
        guest_recipe(&board, "lead", &board.home.path().join("guest")),
        "reserved for the lead",
    );
}

#[test]
fn a_revoked_guest_cannot_read_its_inbox() {
    let board = Fixture::new();
    board.start();
    let handover = board.home.path().join("guest");
    let guest = enroll_guest(&board, "sender", &handover);
    let inbox = guest["inbox"].as_str().unwrap();
    board.lead(&["send", inbox, "Private reply."]);
    run(board.owner(&["revoke", "sender"]));

    let result = output(call_guest(&board, &handover, &["read", inbox]));
    assert!(!result.status.success(), "{result:?}");
    assert!(result.stdout.is_empty(), "{result:?}");
    assert_eq!(participant(&board.status(), "sender")["revoked"], true);
}
