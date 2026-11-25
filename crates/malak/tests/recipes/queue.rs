use super::*;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::time::{SystemTime, UNIX_EPOCH};

const GATED: &[&str] = &[
    "sh",
    "-ec",
    "while test ! -f release; do sleep 0.05; done; printf 'A finished'",
];
const CLOCK_WAIT: Duration = Duration::from_secs(3600);
const EXPIRED: &str = "deadline expired without an on-time final report";

struct RecipeProcess {
    child: Child,
    log: PathBuf,
}

impl RecipeProcess {
    fn start(board: &Fixture, mut command: Command, name: &str) -> Self {
        let log = board.home.path().join(format!("{name}.log"));
        let output = fs::File::create(&log).unwrap();
        let child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        Self { child, log }
    }

    fn wait_for(&mut self, text: &str) -> String {
        let until = Instant::now() + TEST_WATCHDOG;
        loop {
            let log = self.read_log();
            if log.contains(text) {
                return log;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                let final_log = self.read_log();
                assert!(
                    final_log.contains(text),
                    "{status} while waiting for {text}: {final_log}"
                );
                return final_log;
            }
            assert!(Instant::now() < until, "waiting for {text}: {log}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn group_signal(&self, signal: Signal) {
        killpg(Pid::from_raw(self.child.id() as i32), signal).unwrap();
    }

    fn signal(&self, signal: Signal) {
        kill(Pid::from_raw(self.child.id() as i32), signal).unwrap();
    }

    fn finish(&mut self, success: bool) {
        let until = Instant::now() + TEST_WATCHDOG;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success, "{status}: {}", self.read_log());
                return;
            }
            assert!(
                Instant::now() < until,
                "recipe did not finish: {}",
                self.read_log()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn read_log(&self) -> String {
        String::from_utf8_lossy(&fs::read(&self.log).unwrap()).into_owned()
    }
}

impl Drop for RecipeProcess {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let _ = killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

fn worker(board: &Fixture, name: &str, options: &[&str], command: &[&str]) -> Command {
    let mut recipe = board.recipe("worker-start");
    recipe.arg("--unbound");
    recipe
        .args([name, "--board", BOARD, "--no-watcher"])
        .args(options)
        .arg("--")
        .args(command);
    recipe
}

fn queue(board: &Fixture, name: &str, after: &str, options: &[&str], command: &[&str]) -> Command {
    let mut recipe = board.recipe("queue");
    recipe
        .args([name, "--board", BOARD, "--no-watcher", "--after", after])
        .args(options)
        .arg("--")
        .args(command);
    recipe
}

fn scratch() -> Fixture {
    let board = Fixture::new();
    fs::write(board.home.path().join("brief"), b"Use the queued brief.\n").unwrap();
    board.start();
    board
}

#[test]
fn queue_refuses_to_enrol_without_a_lead_watcher() {
    let board = scratch();
    run(worker(&board, "a", &[], &["true"]));
    let before = board.status();

    let mut recipe = board.recipe("queue");
    recipe.args([
        "b",
        "--board",
        BOARD,
        "--after",
        "a",
        "--brief",
        "brief",
        "--",
        "touch",
        "b.started",
    ]);
    let result = output(recipe);

    let diagnostics = String::from_utf8_lossy(&result.stderr);
    assert!(!result.status.success(), "{diagnostics}");
    assert!(
        diagnostics.contains("no lead watcher is running on board malak"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("then run queue again (or pass --no-watcher)"),
        "{diagnostics}"
    );
    assert!(!marker(&board, "b.started"));
    let after = board.status();
    assert_eq!(after["participants"], before["participants"]);
    assert_eq!(after["channels"], before["channels"]);
}

fn gated(board: &Fixture) -> RecipeProcess {
    let mut worker =
        RecipeProcess::start(board, worker(board, "a", &["--brief", "brief"], GATED), "a");
    worker.wait_for("Worker a ready.");
    worker
}

fn waiting(
    board: &Fixture,
    name: &str,
    after: &str,
    extra: &[&str],
    command: &[&str],
) -> RecipeProcess {
    let mut options = vec!["--brief", "brief"];
    options.extend(extra);
    let mut waiter =
        RecipeProcess::start(board, queue(board, name, after, &options, command), name);
    waiter.wait_for(&format!("Worker {name} waiting for {after}."));
    waiter
}

struct QueueClock {
    path: PathBuf,
    start: u128,
}

impl QueueClock {
    fn new(board: &Fixture, start: SystemTime) -> Self {
        let clock = Self {
            path: board.home.path().join("queue-clock"),
            start: start.duration_since(UNIX_EPOCH).unwrap().as_nanos(),
        };
        clock.set(clock.start);
        clock
    }

    fn waiting(&self, board: &Fixture, command: &[&str]) -> RecipeProcess {
        let recipe = queue(
            board,
            "b",
            "a",
            &[
                "--brief",
                "brief",
                "--wait",
                &CLOCK_WAIT.as_secs().to_string(),
            ],
            command,
        );
        self.start(board, recipe)
    }

    fn start(&self, board: &Fixture, mut recipe: Command) -> RecipeProcess {
        recipe_python_startup(
            board,
            &mut recipe,
            include_bytes!("../fixtures/queue_clock.py"),
        );
        recipe
            .env("TEST_QUEUE_CLOCK", &self.path)
            .env("TEST_QUEUE_START_NS", self.start.to_string());
        let mut waiter = RecipeProcess::start(board, recipe, "b");
        waiter.wait_for("Worker b waiting for a.");
        waiter
    }

    fn pause(&self) {
        self.set("paused");
    }

    fn expire(&self) {
        self.set(self.start + CLOCK_WAIT.as_nanos() + 1);
    }

    fn set(&self, value: impl ToString) {
        let next = self.path.with_extension("next");
        fs::write(&next, value.to_string()).unwrap();
        fs::rename(next, &self.path).unwrap();
    }
}

fn id(board: &Fixture, name: &str) -> String {
    participant(&board.status(), name)["id"]
        .as_str()
        .unwrap()
        .into()
}

fn inbox(board: &Fixture, name: &str) -> String {
    format!("to/{}", id(board, name))
}

fn reports(board: &Fixture) -> Vec<Value> {
    messages(&board.lead(&["read", "lead", "--json"]))
}

fn release(board: &Fixture) {
    fs::write(board.home.path().join("release"), []).unwrap();
}

fn marker(board: &Fixture, name: &str) -> bool {
    board.home.path().join(name).is_file()
}

#[test]
fn queue_runs_a_real_chain_on_the_original_trace() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(
        &board,
        "b",
        "a",
        &[],
        &[
            "sh",
            "-ec",
            r#"malak read --ack "$MALAK_HOOK_INBOX" --json > b.messages
malak send --kind report lead "B working" >/dev/null
printf '%s' "$MALAK_PARTICIPANT_ID" > b.started
while test ! -f release-b; do sleep 0.05; done
printf 'B finished'"#,
        ],
    );
    let mut c = waiting(
        &board,
        "c",
        "b",
        &[],
        &[
            "sh",
            "-ec",
            r#"malak read --ack "$MALAK_HOOK_INBOX" --json > c.messages
printf '%s' "$MALAK_PARTICIPANT_ID" > c.started"#,
        ],
    );
    let a_inbox = inbox(&board, "a");
    let b_inbox = inbox(&board, "b");
    let c_inbox = inbox(&board, "c");
    let before = messages(&board.lead(&["read", &b_inbox, "--json"]));
    assert_eq!(before.len(), 1);
    assert_eq!(before[0]["sender"], "lead");
    assert_eq!(before[0]["kind"], "task");
    assert!(!marker(&board, "b.started"));
    assert!(!marker(&board, "c.started"));
    let a_task = messages(&board.lead(&["read", &a_inbox, "--json"]));
    let cursor = || {
        let status = board.status();
        status["channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|channel| channel["name"] == "lead")
            .unwrap()["readers"]["lead"]
            .clone()
    };
    let lead_before = cursor();

    release(&board);
    a.finish(true);
    b.wait_for("Forwarded lead #");
    wait_for_file(&board, "b.started");
    assert!(!marker(&board, "c.started"));
    fs::write(board.home.path().join("release-b"), []).unwrap();
    b.finish(true);
    c.finish(true);
    assert_eq!(cursor(), lead_before);

    let read = |name| messages(&fs::read_to_string(board.home.path().join(name)).unwrap());
    let b_messages = read("b.messages");
    let c_messages = read("c.messages");
    assert_eq!(b_messages.len(), 2);
    assert_eq!(c_messages.len(), 2);
    assert_eq!(b_messages[0]["id"], before[0]["id"]);
    let reports = reports(&board);
    let a_report = reports
        .iter()
        .find(|row| row["sender_name"] == "a")
        .unwrap();
    let b_final = reports
        .iter()
        .find(|row| row["text"].as_str().unwrap().contains("B finished"))
        .unwrap();
    let b_progress = reports
        .iter()
        .find(|row| row["text"] == "B working")
        .unwrap();
    let trace = &a_task[0]["headers"]["trace"];
    assert!(trace.is_string());
    for entry in [
        &b_messages[0],
        &b_messages[1],
        &c_messages[0],
        &c_messages[1],
        b_progress,
    ] {
        assert_eq!(&entry["headers"]["trace"], trace);
    }
    for (received, report) in [(&b_messages[1], a_report), (&c_messages[1], b_final)] {
        assert_eq!(received["sender"], "lead");
        assert_eq!(received["kind"], "note");
        assert_eq!(
            received["headers"]["reply_to"],
            json!(["lead", report["creation"], report["position"]])
        );
        let prefix = format!(
            "Forwarded from {} ({}):\n\n",
            report["sender_name"].as_str().unwrap(),
            report["sender"].as_str().unwrap()
        );
        assert_eq!(body(received), [prefix.into_bytes(), body(report)].concat());
    }
    assert_eq!(
        fs::read_to_string(board.home.path().join("b.started")).unwrap(),
        b_inbox.trim_start_matches("to/")
    );
    assert_eq!(
        fs::read_to_string(board.home.path().join("c.started")).unwrap(),
        c_inbox.trim_start_matches("to/")
    );
}

#[test]
fn a_progress_report_cannot_release_a_queue() {
    let board = scratch();
    let mut a = gated(&board);
    let clock = QueueClock::new(&board, SystemTime::now());
    let mut b = clock.waiting(&board, &["touch", "b.started"]);
    let log = a.read_log();
    let handover = announced_handover(&log);
    run(participant_command(
        handover,
        &[
            "send",
            "--kind",
            "report",
            "--id",
            "launcher-looking",
            "lead",
            "Worker exit status: 0\n\nDone.",
        ],
    ));
    clock.expire();
    b.finish(false);
    assert!(b.read_log().contains(EXPIRED));
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], false);
    assert!(
        messages(&board.lead(&["read", &inbox(&board, "a"), "--json"]))
            .iter()
            .all(|row| row["kind"] != "note")
    );
    let notes: Vec<_> = reports(&board)
        .into_iter()
        .filter(|row| row["sender"] == "lead")
        .collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["kind"], "note");
    assert!(notes[0]["text"].as_str().unwrap().contains("behind a"));
    release(&board);
    a.finish(true);
}

#[test]
fn expired_lane_can_be_started_manually_with_the_same_brief() {
    let board = scratch();
    let _a = gated(&board);
    let clock = QueueClock::new(&board, SystemTime::now());
    let mut b = clock.waiting(&board, &["touch", "automatic"]);
    let before = id(&board, "b");
    let inbox = format!("to/{before}");
    let brief = messages(&board.lead(&["read", &inbox, "--json"]));
    assert_eq!(brief.len(), 1);
    assert_eq!(brief[0]["kind"], "task");
    let competing = output(worker(
        &board,
        "b",
        &["--prepared"],
        &["touch", "competing"],
    ));
    assert!(!competing.status.success());
    let error = String::from_utf8_lossy(&competing.stderr);
    assert!(error.contains("already has a launcher"), "{error}");
    clock.expire();
    b.finish(false);
    assert!(b.read_log().contains(EXPIRED));
    run(worker(
        &board,
        "b",
        &["--prepared"],
        &[
            "sh",
            "-ec",
            r#"malak read --ack "$MALAK_HOOK_INBOX" --json > manual"#,
        ],
    ));
    assert_eq!(id(&board, "b"), before);
    let received = fs::read_to_string(board.home.path().join("manual")).unwrap();
    let received = messages(&received);
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["id"], brief[0]["id"]);
    assert_eq!(body(&received[0]), body(&brief[0]));
    assert!(!marker(&board, "automatic"));
    assert!(!marker(&board, "competing"));
    assert!(
        !output(worker(&board, "b", &["--prepared"], &["true"]))
            .status
            .success()
    );
}

#[test]
fn requeue_reuses_an_expired_lane_after_the_predecessor_reports_and_is_revoked() {
    let board = scratch();
    let mut a = gated(&board);
    let clock = QueueClock::new(&board, SystemTime::now());
    let mut b = clock.waiting(&board, &["false"]);
    let before = id(&board, "b");
    clock.expire();
    b.finish(false);
    assert!(b.read_log().contains(EXPIRED));
    release(&board);
    a.finish(true);
    run(board.owner(&["revoke", &id(&board, "a")]));
    run(queue(
        &board,
        "b",
        "a",
        &["--prepared"],
        &["touch", "b.started"],
    ));
    assert!(marker(&board, "b.started"));
    assert_eq!(id(&board, "b"), before);
    let rows = messages(&board.lead(&["read", &inbox(&board, "b"), "--json"]));
    assert_eq!(
        rows.iter()
            .map(|row| row["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["task", "note"]
    );
}

#[test]
fn cancel_revokes_a_waiting_lane_before_reporting() {
    let board = scratch();
    let _a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    b.signal(Signal::SIGTERM);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], true);
    assert!(
        !output(worker(&board, "b", &["--prepared"], &["true"]))
            .status
            .success()
    );
    assert!(
        reports(&board).iter().any(|row| row["kind"] == "report"
            && row["text"].as_str().unwrap().contains("queue cancelled"))
    );
}

#[test]
fn a_stopped_launcher_without_a_report_fails_a_queue() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    a.group_signal(Signal::SIGKILL);
    a.finish(false);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(reports(&board).iter().any(|row| {
        row["text"]
            .as_str()
            .unwrap()
            .contains("launcher stopped without a final report")
    }));
}

#[test]
fn failing_predecessor_blocks_queued_launch() {
    let board = scratch();
    let mut a = RecipeProcess::start(
        &board,
        worker(
            &board,
            "a",
            &[],
            &[
                "sh",
                "-ec",
                "while test ! -f release; do sleep 0.05; done; exit 7",
            ],
        ),
        "a",
    );
    a.wait_for("Worker a ready.");
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    release(&board);
    a.finish(false);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(reports(&board).iter().any(|row| {
        row["sender"] == "lead"
            && row["text"]
                .as_str()
                .unwrap()
                .contains("exited with status 7")
    }));
}

#[test]
fn revocation_before_the_report_stops_the_entire_waiting_chain() {
    let board = scratch();
    let _a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    let mut c = waiting(&board, "c", "b", &[], &["touch", "c.started"]);
    run(board.owner(&["revoke", &id(&board, "a")]));
    b.finish(false);
    c.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(!marker(&board, "c.started"));
    assert_eq!(
        reports(&board)
            .iter()
            .filter(|row| row["sender"] == "lead")
            .count(),
        2
    );
}

#[test]
fn one_predecessor_can_release_two_waiters() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    let mut c = waiting(&board, "c", "a", &[], &["touch", "c.started"]);
    release(&board);
    a.finish(true);
    b.finish(true);
    c.finish(true);
    for name in ["b", "c"] {
        let rows = messages(&board.lead(&["read", &inbox(&board, name), "--json"]));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["kind"], "note");
        assert!(marker(&board, &format!("{name}.started")));
    }
}

#[test]
fn the_same_queue_can_release_after_board_restart() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    run(board.owner(&["board", "stop"]));
    board.start();
    release(&board);
    a.finish(true);
    b.finish(true);
    assert!(marker(&board, "b.started"));
}

#[test]
fn killing_a_waiter_does_not_restart_it_with_the_board() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "automatic"]);
    b.signal(Signal::SIGKILL);
    b.finish(false);
    run(board.owner(&["board", "stop"]));
    board.start();
    release(&board);
    a.finish(true);
    assert!(!marker(&board, "automatic"));
    run(worker(&board, "b", &["--prepared"], &["touch", "manual"]));
    assert!(marker(&board, "manual"));
}

#[test]
fn a_report_stamped_after_the_deadline_cannot_release() {
    let board = scratch();
    let mut a = gated(&board);
    let clock = QueueClock::new(&board, UNIX_EPOCH);
    clock.pause();
    let mut b = clock.waiting(&board, &["touch", "b.started"]);
    release(&board);
    a.finish(true);
    clock.expire();
    b.finish(false);
    assert!(b.read_log().contains(EXPIRED));
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], false);
}

#[test]
fn an_on_time_report_allows_later_verification() {
    let board = scratch();
    let mut a = gated(&board);
    let clock = QueueClock::new(&board, SystemTime::now());
    clock.pause();
    let mut b = clock.waiting(&board, &["touch", "b.started"]);
    release(&board);
    a.finish(true);
    clock.expire();
    b.finish(true);
    assert!(marker(&board, "b.started"));
}

#[test]
fn invalid_queue_requests_leave_no_new_enrolment() {
    let board = scratch();
    let mut a = gated(&board);
    let before = board.status()["participants"].as_array().unwrap().len();
    for (name, after, options) in [
        ("b", "absent", vec!["--brief", "brief"]),
        ("b", "a", vec!["--brief", "absent"]),
        ("b", "a", vec!["--brief", "brief", "--wait", "0"]),
        ("a", "a", vec!["--brief", "brief"]),
    ] {
        assert!(
            !output(queue(&board, name, after, &options, &["true"]))
                .status
                .success()
        );
    }
    let log = a.read_log();
    let mut unauthorized = queue(&board, "b", "a", &["--brief", "brief"], &["true"]);
    unauthorized.env("MALAK_HANDOVER", announced_handover(&log));
    assert!(!output(unauthorized).status.success());
    assert_eq!(
        board.status()["participants"].as_array().unwrap().len(),
        before
    );
    release(&board);
    a.finish(true);
}

fn fault_recipe(board: &Fixture, recipe: &str, fault: &str) -> Command {
    let recipes = Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes");
    let shim = board.home.path().join("queue-faults");
    fs::write(&shim, include_bytes!("../fixtures/queue_faults.py")).unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let hooks = board.home.path().join("test-hooks");
    fs::create_dir_all(&hooks).unwrap();
    let mut recipe = Command::new(recipes.join(recipe));
    board.recipe_environment(&mut recipe);
    recipe
        .current_dir(board.home.path())
        .env("TEST_MALAK", MALAK)
        .env("TEST_QUEUE_FAULT", fault)
        .env("MALAK_RECIPE_EXE", shim)
        .env("MALAK_RECIPE_HOOK_STATE", hooks);
    recipe
}

fn fault_queue(board: &Fixture, fault: &str, command: &[&str], extra: &[&str]) -> Command {
    let mut recipe = fault_recipe(board, "queue", fault);
    recipe
        .args([
            "b",
            "--board",
            BOARD,
            "--no-watcher",
            "--after",
            "a",
            "--brief",
            "brief",
        ])
        .args(extra)
        .arg("--")
        .args(command);
    recipe
}

#[test]
fn lost_brief_receipt_reuses_the_original_task() {
    let board = scratch();
    let mut a = gated(&board);
    let command = fault_queue(&board, "lose-brief", &["touch", "b.started"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    b.wait_for("Worker b waiting");
    let rows = messages(&board.lead(&["read", &inbox(&board, "b"), "--json"]));
    assert!(marker(&board, "fault-lose-brief"));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "task");
    release(&board);
    a.finish(true);
    b.finish(true);
}

#[test]
fn lost_forward_receipt_reuses_the_original_note() {
    let board = scratch();
    let mut a = gated(&board);
    let command = fault_queue(&board, "lose-forward", &["touch", "b.started"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    b.wait_for("Worker b waiting");
    release(&board);
    a.finish(true);
    b.finish(true);
    let rows = messages(&board.lead(&["read", &inbox(&board, "b"), "--json"]));
    assert_eq!(rows.iter().filter(|row| row["kind"] == "note").count(), 1);
    assert!(marker(&board, "fault-lose-forward"));
    assert!(marker(&board, "b.started"));
}

#[test]
fn lost_expiry_receipt_reuses_the_original_outcome() {
    let board = scratch();
    let _a = gated(&board);
    let clock = QueueClock::new(&board, SystemTime::now());
    let command = fault_queue(
        &board,
        "lose-outcome",
        &["touch", "b.started"],
        &["--wait", &CLOCK_WAIT.as_secs().to_string()],
    );
    let mut b = clock.start(&board, command);
    clock.expire();
    b.finish(false);
    assert!(b.read_log().contains(EXPIRED));
    assert!(marker(&board, "fault-lose-outcome"));
    assert_eq!(
        reports(&board)
            .iter()
            .filter(|row| row["kind"] == "note")
            .count(),
        1
    );
}

#[test]
fn failed_brief_post_revokes_the_new_enrolment_before_reporting() {
    let board = scratch();
    let _a = gated(&board);
    let result = output(fault_queue(
        &board,
        "fail-brief",
        &["touch", "b.started"],
        &[],
    ));
    assert!(!result.status.success());
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], true);
    assert!(reports(&board).iter().any(|row| {
        row["text"]
            .as_str()
            .unwrap()
            .contains("fixture brief refused")
    }));
}

#[test]
fn cancellation_after_forward_commit_prevents_handoff() {
    let board = scratch();
    let mut a = gated(&board);
    let command = fault_queue(&board, "hold", &["touch", "b.started"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    b.wait_for("Worker b waiting");
    release(&board);
    a.finish(true);
    wait_for_file(&board, "fault-hold");
    b.signal(Signal::SIGTERM);
    fs::write(board.home.path().join("finish-forward"), []).unwrap();
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], true);
    assert!(
        reports(&board)
            .iter()
            .any(|row| row["text"].as_str().unwrap().contains("queue cancelled"))
    );
}

#[test]
fn queued_revocation_after_forward_commit_prevents_handoff() {
    let board = scratch();
    let mut a = gated(&board);
    let command = fault_queue(&board, "hold", &["touch", "b.started"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    b.wait_for("Worker b waiting");
    release(&board);
    a.finish(true);
    wait_for_file(&board, "fault-hold");
    run(board.owner(&["revoke", &id(&board, "b")]));
    fs::write(board.home.path().join("finish-forward"), []).unwrap();
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(
        reports(&board)
            .iter()
            .any(|row| row["sender"] == "lead" && row["kind"] == "report")
    );
}

#[test]
fn a_brief_trimmed_after_forward_commit_prevents_handoff() {
    let board = scratch();
    let mut a = gated(&board);
    let command = fault_queue(&board, "trim", &["touch", "b.started"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    b.wait_for("Worker b waiting");
    release(&board);
    a.finish(true);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    let rows = messages(&board.lead(&["read", &inbox(&board, "b"), "--json"]));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "note");
    assert!(reports(&board).iter().any(|row| {
        row["text"]
            .as_str()
            .unwrap()
            .contains("is missing or does not match")
    }));
}

#[test]
fn failed_queued_hook_check_uses_worker_start_cleanup() {
    let board = scratch();
    let mut a = gated(&board);
    let agent = board.home.path().join("claude");
    fs::write(&agent, "#!/bin/sh\ntouch b.started\n").unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    let command = fault_queue(&board, "hook", &["./claude"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    let before = b.wait_for("Worker b waiting");
    assert!(!before.contains("fixture hook check refused"));
    release(&board);
    a.finish(true);
    b.finish(false);
    assert!(b.read_log().contains("fixture hook check refused"));
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], true);
}

#[test]
fn revoked_predecessor_cannot_be_replaced_by_name() {
    let board = scratch();
    let _a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    b.group_signal(Signal::SIGSTOP);
    run(board.owner(&["revoke", &id(&board, "a")]));
    run(worker(&board, "a", &[], &["true"]));
    b.group_signal(Signal::SIGCONT);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
}

#[test]
fn a_trimmed_final_report_cannot_release_a_queue() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    b.group_signal(Signal::SIGSTOP);
    release(&board);
    a.finish(true);
    let report = reports(&board).remove(0);
    let cutoff = report["position"].as_u64().unwrap() + 1;
    run(board.owner(&["trim", "lead", &cutoff.to_string()]));
    b.group_signal(Signal::SIGCONT);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(
        reports(&board)
            .iter()
            .any(|row| row["sender"] == "lead" && row["kind"] == "report")
    );
}

#[test]
fn queue_help_publishes_the_default_wait() {
    let board = Fixture::new();
    let mut recipe = board.recipe("queue");
    recipe.arg("--help");
    let help = run(recipe);
    assert!(help.contains("The default wait is 30 minutes."), "{help}");
}

#[test]
fn unavailable_evidence_at_expiry_keeps_the_lane_and_reports_after_restart() {
    let board = scratch();
    let _a = gated(&board);
    let clock = QueueClock::new(&board, SystemTime::now());
    let mut b = clock.waiting(&board, &["touch", "b.started"]);
    run(board.owner(&["board", "stop"]));
    clock.expire();
    b.wait_for("final-report evidence was unavailable");
    board.start();
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert_eq!(participant(&board.status(), "b")["revoked"], false);
    assert!(reports(&board).iter().any(|row| {
        row["kind"] == "note"
            && row["text"]
                .as_str()
                .unwrap()
                .contains("evidence was unavailable")
    }));
}

#[test]
fn a_replacement_board_cannot_release_a_waiter() {
    let board = scratch();
    let other = board.sibling("other", "other-socket");
    other.start();
    let _a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    let handover = board.state.join("lead.handover");
    let original = fs::read(&handover).unwrap();
    fs::copy(other.state.join("lead.handover"), &handover).unwrap();
    b.finish(false);
    fs::write(handover, original).unwrap();
    assert!(!marker(&board, "b.started"));
    assert!(b.read_log().contains("board identity changed"));
}

#[test]
fn a_different_retained_record_cannot_replace_the_final_report() {
    let board = scratch();
    let mut a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    b.group_signal(Signal::SIGSTOP);
    let log = a.read_log();
    run(participant_command(
        announced_handover(&log),
        &["send", "lead", "--kind", "report", "Worker exit status: 0"],
    ));
    release(&board);
    a.finish(true);
    let store = board
        .state
        .join("launches")
        .join(format!("{}.json", id(&board, "a")));
    let mut evidence: Value = serde_json::from_slice(&fs::read(&store).unwrap()).unwrap();
    evidence["report"]["position"] = json!(1);
    fs::write(&store, serde_json::to_vec(&evidence).unwrap()).unwrap();
    b.group_signal(Signal::SIGCONT);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(b.read_log().contains("is missing or does not match"));
}

#[test]
fn queued_revocation_ends_the_wait_before_the_predecessor_reports() {
    let board = scratch();
    let _a = gated(&board);
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    run(board.owner(&["revoke", &id(&board, "b")]));
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(reports(&board).iter().any(|row| {
        row["text"]
            .as_str()
            .unwrap()
            .contains("queued enrolment was revoked")
    }));
}

#[test]
fn refused_forward_keeps_the_queued_command_unstarted() {
    let board = scratch();
    let mut a = gated(&board);
    let command = fault_queue(&board, "fail-forward", &["touch", "b.started"], &[]);
    let mut b = RecipeProcess::start(&board, command, "b");
    b.wait_for("Worker b waiting");
    release(&board);
    a.finish(true);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(reports(&board).iter().any(|row| {
        row["text"]
            .as_str()
            .unwrap()
            .contains("fixture forward refused")
    }));
}

#[test]
fn predecessor_launch_failure_leaves_its_queue_unstarted() {
    let board = scratch();
    let mut command = fault_recipe(&board, "worker-start", "hold-hook");
    command.args([
        "a",
        "--unbound",
        "--board",
        BOARD,
        "--no-watcher",
        "--",
        "./claude",
    ]);
    let mut a = RecipeProcess::start(&board, command, "a");
    a.wait_for("Worker a ready.");
    wait_for_file(&board, "hook-check-waiting");
    let mut b = waiting(&board, "b", "a", &[], &["touch", "b.started"]);
    fs::write(board.home.path().join("finish-hook-check"), []).unwrap();
    a.finish(false);
    b.finish(false);
    assert!(!marker(&board, "b.started"));
    assert!(
        reports(&board).iter().any(|row| row["sender"] == "lead"
            && row["text"].as_str().unwrap().contains("the predecessor"))
    );
}
