//! OR: real-board grants, event writers and reader projections.

use super::*;

fn org(top: &Fixture, words: &[&str]) -> Command {
    let mut command = top.owner(&["recipe", "org"]);
    top.recipe_environment(&mut command);
    command.args(words);
    command
}

pub(super) fn organisation() -> (Fixture, Fixture, Fixture) {
    let a = Fixture::new();
    a.start();
    let b = a.sibling("second", "s2");
    b.configure("name = 'second'");
    b.start();
    let top = a.sibling("top", "s3");
    top.configure("name = 'top'");
    run(org(&top, &["setup", BOARD, "second"]));
    (top, a, b)
}

#[test]
fn org_setup_leaves_the_top_lead_unclaimed() {
    let (top, _, _) = organisation();
    assert!(top.status()["lead_session"].is_null());
}

#[test]
fn org_setup_refuses_a_bound_top_without_changing_channels_or_roster() {
    let board = Fixture::new();
    board.start();
    let before = board.status();
    let output = org(&board, &["setup"]).output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("bound lead"), "{error}");
    assert!(error.contains("unclaimed board"), "{error}");
    assert_eq!(board.status()["channels"], before["channels"]);
    assert_eq!(board.status()["lead_session"], before["lead_session"]);
    assert!(!board.state.join("org").exists());
}

#[test]
fn foreign_reads_use_only_the_callers_recorded_reader_seat() {
    let (_, a, b) = organisation();
    let body = "Foreign task stays information. ".repeat(2_000);
    let receipt: Value =
        serde_json::from_str(&b.lead(&["send", "--json", "--kind", "task", "lead", &body]))
            .unwrap();
    let position = receipt["position"].to_string();
    let creation = receipt["creation"].as_str().unwrap();
    let foreign = |words: &[&str]| {
        let mut command = b.owner(words);
        a.recipe_environment(&mut command);
        command
    };
    let value: Value = serde_json::from_str(&run(foreign(&[
        "get",
        "--agent",
        "--json",
        "--creation",
        creation,
        "lead",
        &position,
    ])))
    .unwrap();
    let message = &value["messages"][0];
    assert_eq!(message["presentation"], "information");
    assert_eq!(message["guest_board"], "second");
    assert_eq!(message["fetch"]["board"], "second");
    assert_eq!(message["fetch"]["creation"], creation);
    let offset = message["fetch"]["offset"].to_string();
    let page: Value = serde_json::from_str(&run(foreign(&[
        "get",
        "--agent",
        "--json",
        "--creation",
        creation,
        "--offset",
        &offset,
        "lead",
        &position,
    ])))
    .unwrap();
    assert!(
        page["messages"][0]["body"]
            .as_str()
            .unwrap()
            .starts_with(&body[offset.parse::<usize>().unwrap()..][..10])
    );
    let read = run(foreign(&["read", "--agent", "--json", "lead"]));
    assert!(read.contains("Foreign task stays information"));
    for words in [
        vec!["read", "--ack", "lead"],
        vec!["send", "lead", "no authority"],
        vec!["ack", "--creation", creation, "lead", &position],
    ] {
        assert!(!foreign(&words).output().unwrap().status.success());
    }
    let mut changed = creation.to_owned();
    changed.replace_range(..1, if creation.starts_with('0') { "1" } else { "0" });
    let wrong = foreign(&["get", "--agent", "--creation", &changed, "lead", &position])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(wrong.stdout.is_empty());
    let sub = a.enrol("sub", &["lead:get"]);
    let mut command = foreign(&["get", "lead", &position]);
    command.env("MALAK_HANDOVER", sub);
    assert!(!command.output().unwrap().status.success());
}

#[cfg(feature = "mcp")]
#[test]
fn mcp_get_pages_foreign_facts_with_a_checked_creation() {
    use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
    let (_, a, b) = organisation();
    let receipt: Value = serde_json::from_str(&b.lead(&[
        "send",
        "--json",
        "--kind",
        "task",
        "lead",
        "Foreign task is information",
    ]))
    .unwrap();
    let mut command = a.owner(&["mcp"]);
    a.recipe_environment(&mut command);
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let client = ().serve(TokioChildProcess::new(tokio::process::Command::from(command)).unwrap()).await.unwrap();
        let result = client.call_tool(CallToolRequestParams::new("get").with_arguments(json!({"board":"second","channel":"lead","position":receipt["position"],"creation":receipt["creation"]}).as_object().unwrap().clone())).await.unwrap();
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let value = result.structured_content.unwrap();
        assert_eq!(value["messages"][0]["presentation"], "information");
        assert_eq!(value["messages"][0]["guest_board"], "second");
        client.cancel().await.unwrap();
    });
}

#[test]
fn org_read_seats_are_read_get_only_on_every_required_channel() {
    let (top, _, b) = organisation();
    let roster: Value =
        serde_json::from_slice(&fs::read(top.state.join("org/roster.json")).unwrap()).unwrap();
    let seat = roster["seats"][&*format!("lead:{BOARD}")]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["board"] == "second")
        .unwrap();
    let status = b.status();
    let person = status["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == seat["id"])
        .unwrap();
    let grants = person["grant"].to_string();
    for name in [
        "rail",
        "briefs",
        "calls",
        "questions",
        "lead",
        "relay/*",
        "to/*",
    ] {
        assert!(grants.contains(name));
    }
    assert!(!grants.contains("send"));
    assert!(!grants.contains("ack"));
}

#[test]
fn org_read_seat_refuses_cross_board_send() {
    let (top, _, _) = organisation();
    let roster: Value =
        serde_json::from_slice(&fs::read(top.state.join("org/roster.json")).unwrap()).unwrap();
    let seat = roster["seats"][&*format!("lead:{BOARD}")]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["board"] == "second")
        .unwrap();
    let output = participant_command(
        Path::new(seat["handover"].as_str().unwrap()),
        &["send", "lead", "cannot order you"],
    )
    .output()
    .unwrap();
    assert!(!output.status.success());
}

#[test]
fn org_remove_lead_requires_an_interactive_typed_confirmation() {
    let (top, _, _) = organisation();
    let output = org(&top, &["remove-lead", "second"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("typed confirmation"));
}

#[test]
fn lead_up_reissues_org_seats_for_the_recorded_session() {
    let (top, a, _) = organisation();
    let old: Value =
        serde_json::from_slice(&fs::read(top.state.join("org/roster.json")).unwrap()).unwrap();
    let mut command = a.owner(&["recipe", "lead-up"]);
    a.recipe_environment(&mut command);
    run(command);
    let current: Value =
        serde_json::from_slice(&fs::read(top.state.join("org/roster.json")).unwrap()).unwrap();
    assert_ne!(
        old["seats"][&*format!("lead:{BOARD}")][0]["id"],
        current["seats"][&*format!("lead:{BOARD}")][0]["id"]
    );
}

#[test]
fn a_worker_cannot_send_to_rail() {
    let a = Fixture::new();
    a.start();
    let worker = a.enrol("sub", &["lead:send"]);
    let output = participant_command(&worker, &["send", "rail", "Type: rail\nEvent: started"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn rail_wait_is_a_proven_lead_event() {
    let a = Fixture::new();
    a.start();
    let mut command = a.owner(&["recipe", "rail", "wait", "--on", "other", "contract"]);
    a.recipe_environment(&mut command);
    run(command);
    let record: Value = serde_json::from_str(&a.lead(&["get", "--json", "rail", "1"])).unwrap();
    assert_eq!(record["sender"], "lead");
    assert_eq!(record["grade"], "proven");
    assert!(
        String::from_utf8(
            record["body"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_u64().unwrap() as u8)
                .collect()
        )
        .unwrap()
        .contains("Event: waits on")
    );
}

#[test]
fn pulse_refuses_an_ordinary_lead() {
    let a = Fixture::new();
    a.start();
    let mut command = a.owner(&["pulse", "--json"]);
    a.recipe_environment(&mut command);
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("chief of staff"));
}

#[test]
fn a_top_leads_request_reaches_the_receiving_leads_watcher() {
    let (top, _, b) = organisation();
    top.lead(&[
        "send",
        "top/leads",
        "For: second\nPlease check the contract.",
    ]);
    let mut command = b.owner(&[
        "mailbox",
        "--plain",
        "--for",
        "lead",
        "--next",
        "--timeout",
        "1",
    ]);
    b.recipe_environment(&mut command);
    let watched = run(command);
    assert!(watched.contains("top/leads"), "{watched}");
    assert!(watched.contains("Please check the contract"), "{watched}");
}

#[test]
fn a_top_leads_request_is_acknowledged_from_the_home_lead_inbox() {
    let (top, _, b) = organisation();
    top.lead(&[
        "send",
        "top/leads",
        "For: second\nPlease check the contract.",
    ]);
    let mut command = b.owner(&["read", "--json", "--ack", "lead"]);
    b.recipe_environment(&mut command);
    let read = run(command);
    assert!(read.contains("top/leads"), "{read}");
    let mut command = b.owner(&["read", "--json", "lead"]);
    b.recipe_environment(&mut command);
    assert!(!run(command).contains("top/leads"));
}

#[test]
fn rail_with_an_explicit_config_works_without_a_default_board() {
    let board = Fixture::in_home(Rc::new(BoardHome::new()), "named", "st", "s");
    board.configure("name = 'named'");
    board.start();
    assert!(!board.home.path().join(".malak/malak/board.toml").exists());
    let mut command = board.owner(&["rail", "--json"]);
    board.recipe_environment(&mut command);
    let result: Value = serde_json::from_str(&run(command)).unwrap();
    assert_eq!(result["items"], json!([]));
}

#[test]
fn a_chief_reads_its_top_board_by_the_board_name() {
    let (top, _, _) = organisation();
    top.lead(&[
        "send",
        "rail",
        "Type: rail\nEvent: started\nSubject: ramp\nLink: [\"top\",\"rail\",\"fact\",1]",
    ]);
    let path = top.state.join("org/roster.json");
    let mut roster: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    roster["chief_of_staff"] = json!({"name":"chief"});
    fs::write(path, serde_json::to_vec(&roster).unwrap()).unwrap();
    let mut command = top.owner(&["rail", "--json"]);
    top.recipe_environment(&mut command);
    let result: Value = serde_json::from_str(&run(command)).unwrap();
    assert_eq!(result["items"][0]["board"], "top");
}

#[test]
fn org_trim_applies_rail_age_and_preserves_briefs() {
    let (top, _, _) = organisation();
    top.lead(&["send", "rail", "old rail note"]);
    top.lead(&[
        "send",
        "briefs",
        "Type: brief\nTitle: keep\nKind: report\nLead: top\n\nRetained.",
    ]);
    let path = top.state.join("org/roster.json");
    let mut roster: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    roster["rail_age_days"] = json!(0);
    fs::write(path, serde_json::to_vec(&roster).unwrap()).unwrap();
    run(org(&top, &["trim"]));
    let status = top.status();
    let channels = status["channels"].as_array().unwrap();
    assert_eq!(
        channels.iter().find(|c| c["name"] == "rail").unwrap()["floor"],
        2
    );
    assert_eq!(
        channels.iter().find(|c| c["name"] == "briefs").unwrap()["floor"],
        1
    );
}

#[test]
fn wait_done_links_the_open_wait_and_does_not_use_unblocked() {
    let board = Fixture::new();
    board.start();
    let mut wait = board.owner(&["recipe", "rail", "wait", "--on", "other", "contract"]);
    board.recipe_environment(&mut wait);
    run(wait);
    let mut done = board.owner(&["recipe", "rail", "wait", "--done", "contract"]);
    board.recipe_environment(&mut done);
    run(done);
    let value: Value = serde_json::from_str(&board.lead(&["get", "--json", "rail", "2"])).unwrap();
    let text = String::from_utf8(
        value["body"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect(),
    )
    .unwrap();
    assert!(text.contains("Event: wait done"), "{text}");
    assert!(text.contains("\"rail\""), "{text}");
    assert!(!text.contains("unblocked"));
}

fn brief_view(board: &Fixture) -> Value {
    let mut command = board.owner(&["brief", "view", "--json"]);
    board.recipe_environment(&mut command);
    serde_json::from_str(&run(command)).unwrap()
}

fn wait_for_question(a: &Fixture, b: &Fixture, subject: &str) -> Value {
    let question: Value =
        serde_json::from_str(&b.lead(&["send", "--json", "questions", "Dependency source"]))
            .unwrap();
    let mut command = a.recipe("rail");
    command.args([
        "wait",
        "--on",
        "second",
        subject,
        "--link",
        "second",
        "questions",
        question["creation"].as_str().unwrap(),
        &question["position"].to_string(),
        "--board",
        BOARD,
    ]);
    run(command);
    question
}

#[test]
fn a_linked_wait_closes_from_a_proven_answer_on_the_peer_rail() {
    let (_top, a, b) = organisation();
    let question = wait_for_question(&a, &b, "consumer");
    assert!(
        brief_view(&a)["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["section"] == "stuck" && item["subject"] == "consumer")
    );
    let answer: Value = serde_json::from_str(&b.lead(&[
        "send",
        "--json",
        "--reply-to",
        "questions",
        question["creation"].as_str().unwrap(),
        &question["position"].to_string(),
        "questions",
        "Solved",
    ]))
    .unwrap();
    let mut command = b.recipe("rail");
    command.args([
        "emit",
        "unblocked",
        "different display subject",
        "--link",
        "second",
        "questions",
        answer["creation"].as_str().unwrap(),
        &answer["position"].to_string(),
        "--board",
        "second",
    ]);
    run(command);
    let view = brief_view(&a);
    assert!(
        !view["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["section"] == "stuck" && item["subject"] == "consumer"),
        "{view}"
    );
    let mut done = a.recipe("rail");
    done.args(["wait", "--done", "consumer", "--board", BOARD]);
    assert!(!done.output().unwrap().status.success());
}

#[test]
fn an_unreadable_peer_keeps_a_linked_wait_open_with_a_reason() {
    let (_top, a, b) = organisation();
    wait_for_question(&a, &b, "consumer");
    run(b.owner(&["board", "stop"]));
    let view = brief_view(&a);
    let row = view["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["section"] == "bottom line")
        .unwrap();
    assert_eq!(row["colour"], "yellow");
    assert!(
        row["reason"]
            .as_str()
            .unwrap()
            .contains("cannot check: board unreadable"),
        "{row}"
    );
    assert!(
        view["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["section"] == "stuck" && item["subject"] == "consumer")
    );
    let mut done = a.recipe("rail");
    done.args(["wait", "--done", "consumer", "--board", BOARD]);
    run(done);
}

#[test]
fn a_cos_ramp_request_reaches_and_is_acknowledged_from_the_home_inbox() {
    let (top, _, lead) = organisation();
    top.lead(&["send", "cos/ramp", "Ramp-up status proof"]);
    let mut command = lead.owner(&["read", "--json", "--ack", "lead"]);
    lead.recipe_environment(&mut command);
    let read = run(command);
    assert!(read.contains("cos/ramp"), "{read}");
    let mut command = lead.owner(&["read", "--json", "lead"]);
    lead.recipe_environment(&mut command);
    assert!(!run(command).contains("cos/ramp"));
}

fn bound_chief() -> Fixture {
    let board = Fixture::new();
    prepare_chief(&board);
    board
}

fn prepare_chief(board: &Fixture) {
    run(org(board, &["setup"]));
    let mut up = board.owner(&["recipe", "lead-up"]);
    board.recipe_environment(&mut up);
    run(up);
    let roster = board.state.join("org/roster.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&roster).unwrap()).unwrap();
    value["chief_of_staff"] = json!({"name":"test chief"});
    fs::write(roster, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn completed_worker(board: &Fixture) {
    let brief = board.home.path().join("brief");
    fs::write(&brief, "Check delayed lifecycle publication.").unwrap();
    let mut start = board.owner(&["recipe", "worker-start", "alice", "--no-watcher", "--brief"]);
    board.recipe_environment(&mut start);
    start.arg(brief).args([
        "--",
        "sh",
        "-ec",
        "malak read --ack \"$MALAK_TASK_INBOX\"; echo completed result",
    ]);
    run(start);
}

fn rail_snapshot(board: &Fixture) -> (Value, String) {
    let mut command = board.owner(&["rail", "--json"]);
    board.recipe_environment(&mut command);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    (
        serde_json::from_slice(&output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn rail_head(board: &Fixture) -> Value {
    board.status()["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "rail")
        .unwrap()["head"]
        .clone()
}

fn trim_now(board: &Fixture) {
    let roster = board.state.join("org/roster.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&roster).unwrap()).unwrap();
    value["rail_age_days"] = json!(0);
    fs::write(roster, serde_json::to_vec(&value).unwrap()).unwrap();
    run(org(board, &["trim"]));
}

#[test]
fn dependency_notes_do_not_poison_lifecycle_publication_history() {
    let board = bound_chief();
    let mut wait = board.recipe("rail");
    wait.args(["wait", "--on", "other", "contract", "--board", BOARD]);
    run(wait);
    completed_worker(&board);
    let (snapshot, warning) = rail_snapshot(&board);
    assert!(warning.is_empty(), "{warning}");
    let items = snapshot["items"].as_array().unwrap();
    assert!(items.iter().any(|event| event["event"] == "started"));
    assert!(items.iter().any(|event| event["event"] == "finished"));
    let links: Value =
        serde_json::from_slice(&fs::read(board.state.join("org/rail-published.json")).unwrap())
            .unwrap();
    assert!(
        links
            .as_array()
            .unwrap()
            .iter()
            .all(|link| matches!(link[0].as_str(), Some("started" | "finished")))
    );
    assert!(rail_snapshot(&board).1.is_empty());
}

#[test]
fn a_delayed_worker_lifecycle_publishes_once_when_the_lead_returns() {
    let board = bound_chief();
    completed_worker(&board);
    assert_eq!(rail_head(&board), 1);
    assert_eq!(
        rail_snapshot(&board).0["items"].as_array().unwrap().len(),
        2
    );
    let head = rail_head(&board);
    rail_snapshot(&board);
    assert_eq!(rail_head(&board), head);
}

#[test]
fn a_retention_lock_defers_publication_until_the_next_lead_tick() {
    let board = bound_chief();
    completed_worker(&board);
    let mut child = Command::new("python3")
        .args(["-u", "-c", "import fcntl,os,sys; f=os.fdopen(os.open(sys.argv[1],os.O_RDWR|os.O_CREAT,0o600),'r+'); fcntl.flock(f,fcntl.LOCK_EX); print('locked',flush=True); sys.stdin.read()"])
        .arg(board.state.join("org/rail-published.lock"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    struct Lock(std::process::Child);
    impl Drop for Lock {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    let lock = Lock(child);
    assert_eq!(line.trim(), "locked");
    assert_eq!(rail_snapshot(&board).0["items"], json!([]));
    drop(lock);
    assert_eq!(
        rail_snapshot(&board).0["items"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn trim_then_two_lead_ticks_publish_no_old_lifecycle_notes() {
    let board = bound_chief();
    completed_worker(&board);
    rail_snapshot(&board);
    trim_now(&board);
    let head = rail_head(&board);
    for _ in 0..2 {
        let mut tick = board.owner(&[
            "mailbox",
            "--plain",
            "--for",
            "lead",
            "--next",
            "--timeout",
            "1",
        ]);
        board.recipe_environment(&mut tick);
        run(tick);
        assert_eq!(rail_snapshot(&board).0["items"], json!([]));
        assert_eq!(rail_head(&board), head);
    }
}

#[test]
fn deleting_publication_history_cannot_resurrect_trimmed_events() {
    let board = bound_chief();
    completed_worker(&board);
    rail_snapshot(&board);
    trim_now(&board);
    fs::remove_file(board.state.join("org/rail-published.json")).unwrap();
    let head = rail_head(&board);
    let (snapshot, warning) = rail_snapshot(&board);
    assert_eq!(snapshot["items"], json!([]));
    assert_eq!(rail_head(&board), head);
    assert_eq!(
        warning.matches("Rail publication paused:").count(),
        1,
        "{warning}"
    );
    assert!(warning.contains("missing after a rail trim"), "{warning}");
    assert!(!board.state.join("org/rail-published.json").exists());
}

#[test]
fn unreadable_publication_history_cannot_resurrect_trimmed_events() {
    let board = bound_chief();
    completed_worker(&board);
    rail_snapshot(&board);
    trim_now(&board);
    fs::write(board.state.join("org/rail-published.json"), "invalid JSON").unwrap();
    let head = rail_head(&board);
    let (snapshot, warning) = rail_snapshot(&board);
    assert_eq!(snapshot["items"], json!([]));
    assert_eq!(rail_head(&board), head);
    assert_eq!(
        warning.matches("Rail publication paused:").count(),
        1,
        "{warning}"
    );
    assert!(warning.contains("unreadable"), "{warning}");
}

#[test]
fn late_old_exit_replay_keeps_an_again_worker_run_open() {
    let board = bound_chief();
    let brief = board.home.path().join("brief");
    fs::write(&brief, "Check lifecycle recovery.").unwrap();
    let start = |again: bool, script: &str| {
        let mut command = board.recipe("worker-start");
        command.args(["alice", "--board", BOARD, "--no-watcher", "--brief"]);
        command.arg(&brief);
        if again {
            command.args(["--again", "--detach"]);
        }
        command.args(["--", "sh", "-ec", script]);
        run(command)
    };
    start(
        false,
        "malak read --ack \"$MALAK_TASK_INBOX\"; echo first result",
    );
    let pulse = || {
        let mut command = board.owner(&["pulse", "--json"]);
        board.recipe_environment(&mut command);
        serde_json::from_str::<Value>(&run(command)).unwrap()["items"][0].clone()
    };
    assert_eq!(pulse()["counts"]["finished"], 1);
    let mut rail = board.owner(&["rail", "--json"]);
    board.recipe_environment(&mut rail);
    let events: Value = serde_json::from_str(&run(rail)).unwrap();
    let old = events["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "finished")
        .unwrap()["link"]
        .clone();
    let text = start(
        true,
        "malak read --ack \"$MALAK_TASK_INBOX\"; touch ready; while [ ! -f release ]; do sleep 0.05; done; echo second result",
    );
    let group = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Process group"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    struct Stop(Option<nix::unistd::Pid>);
    impl Drop for Stop {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGTERM);
            }
        }
    }
    let mut stop = Stop(Some(nix::unistd::Pid::from_raw(group)));
    wait_for_file(&board, "ready");
    wait_for_event("the accepted claim to appear on the rail", || {
        let row = pulse();
        (row["counts"]["started"] == 1).then_some(row)
    });
    let mut replay = board.recipe("rail");
    replay.args(["emit", "finished", "alice", "--board", BOARD, "--link"]);
    replay.args([
        old[0].as_str().unwrap(),
        old[1].as_str().unwrap(),
        old[2].as_str().unwrap(),
        &old[3].to_string(),
    ]);
    run(replay);
    let after = pulse();
    assert_eq!(after["counts"]["started"], 1);
    assert_eq!(after["counts"]["finished"], 1);
    fs::write(board.home.path().join("release"), "").unwrap();
    wait_for_event("the replacement launcher to exit", || {
        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD, "--json"]);
        let value: Value = serde_json::from_str(&run(workers)).unwrap();
        value["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["state"] == "exited" && row["enrolment"] == "active")
            .cloned()
    });
    stop.0 = None;
    assert_eq!(pulse()["counts"]["started"], 0);
    assert_eq!(pulse()["counts"]["finished"], 2);
}

#[test]
fn an_unresolvable_finished_note_does_not_close_a_dependency() {
    let board = Fixture::new();
    board.start();
    let mut wait = board.recipe("rail");
    wait.args(["wait", "--on", "other", "contract", "--board", BOARD]);
    run(wait);
    for link in [
        "[\"malak\",\"lead\",\"missing\",1]",
        "[\"malak\",\"\",\"missing\",1]",
        "[\"malak\",\"lead\",\"missing\",18446744073709551616]",
    ] {
        board.lead(&[
            "send",
            "rail",
            &format!("Type: rail\nEvent: finished\nSubject: contract\nLink: {link}"),
        ]);
    }
    let mut done = board.recipe("rail");
    done.args(["wait", "--done", "contract", "--board", BOARD]);
    run(done);
    let (snapshot, _) = rail_snapshot(&board);
    assert_eq!(snapshot["items"][0]["event"], "wait done");
    assert_eq!(snapshot["items"][1]["type"], "note");
}

#[test]
fn a_ramp_row_keeps_chief_calls_while_showing_handover_progress() {
    let (top, a, _) = organisation();
    let mut up = top.owner(&["recipe", "lead-up"]);
    top.recipe_environment(&mut up);
    run(up);
    let candidate = top.enrol("candidate", &["cos/ramp:send"]);
    let status = top.status();
    let candidate_id = participant(&status, "candidate")["id"].as_str().unwrap();
    let path = top.state.join("org/roster.json");
    let mut roster: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    roster["chief_of_staff"] = json!({"name":"coordinator","session":status["lead_session"]});
    roster["candidate"] =
        json!({"name":"candidate","id":candidate_id,"session":status["lead_session"]});
    roster["ramp_minutes"] = json!(0);
    fs::write(&path, serde_json::to_vec(&roster).unwrap()).unwrap();
    run(org(&top, &["reseat", "coordinator"]));
    run(participant_command(
        &candidate,
        &["send", "cos/ramp", "Type: ramp\nStep: seated"],
    ));
    a.lead(&[
        "send",
        "briefs",
        "Type: brief\nTitle: handover\nKind: handover\nLead: malak\n\nCurrent work.",
    ]);
    top.lead(&[
        "send",
        "questions",
        "Decision during ramp\nFor: owner\nLevel: red",
    ]);
    let mut command = top.owner(&["pulse", "--json"]);
    top.recipe_environment(&mut command);
    let value: Value = serde_json::from_str(&run(command)).unwrap();
    let rows = value["items"].as_array().unwrap();
    let chief = rows
        .iter()
        .find(|row| row["lead"] == "chief of staff")
        .unwrap();
    assert_eq!(chief["colour"], "red");
    assert_eq!(chief["counts"]["calls"], 1);
    assert!(
        chief["on"]
            .as_str()
            .unwrap()
            .contains("onboarding: 1 of 2 leads briefed")
    );
    assert!(chief.get("last_activity").is_some());
    assert!(!chief["blocker"].is_null());
    assert_eq!(
        rows.iter().find(|row| row["lead"] == "second").unwrap()["reason"],
        "handover brief overdue"
    );
}

#[test]
fn a_named_leads_command_publishes_without_a_default_board() {
    let board = Fixture::in_home(Rc::new(BoardHome::new()), "named", "st", "s");
    board.configure("name = 'named'");
    prepare_chief(&board);
    completed_worker(&board);
    assert!(!board.home.path().join(".malak/malak/board.toml").exists());
    assert_eq!(rail_head(&board), 1);
    let mut command = Command::new(MALAK);
    command.args(["read", "lead"]);
    board.recipe_environment(&mut command);
    run(command);
    assert_eq!(rail_head(&board), 3);
}
