use super::*;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

const GATE: &str = r#"
    checks=0
    while [ ! -f release ]; do
        [ "$checks" -lt 1200 ] || exit 99
        checks=$((checks + 1))
        sleep 0.05
    done
"#;

struct Detached {
    text: String,
    group: Pid,
}

impl Detached {
    fn start(board: &Fixture, script: &str) -> Self {
        Self::launch(board.worker_start(&["--detach"], &["sh", "-ec", script]))
    }

    fn launch(command: Command) -> Self {
        let text = run(command);
        let group = field(&text, "Process group").parse().unwrap();
        Self {
            text,
            group: Pid::from_raw(group),
        }
    }
}

#[test]
fn a_detached_launcher_holds_its_claim_during_resume() {
    let board = Fixture::new();
    board.start();
    let resume = format!("touch resumed; {GATE}");
    let _worker = Detached::launch(board.worker_start(
        &["--detach", "--resume", &resume],
        &["sh", "-c", "echo first report"],
    ));
    assert!(run(wait_report(&board, "30s")).contains("first report"));
    let id = board.worker_id();
    board.lead(&["send", "--kind", "task", &format!("to/{id}"), "Resume work"]);
    wait_for_file(&board, "resumed");

    let lock = fs::File::open(board.state.join("launches").join(format!("{id}.lock"))).unwrap();

    assert!(matches!(
        lock.try_lock_shared(),
        Err(fs::TryLockError::WouldBlock)
    ));
    fs::write(board.home.path().join("release"), "").unwrap();
}

impl Drop for Detached {
    fn drop(&mut self) {
        let _ = killpg(self.group, Signal::SIGTERM);
    }
}

fn field<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.trim_start().strip_prefix(label)?.strip_prefix(' '))
        .map(str::trim)
        .unwrap_or_else(|| panic!("missing {label:?} in {text:?}"))
}

fn wait_report(board: &Fixture, timeout: &str) -> Command {
    let mut wait = board.recipe("await");
    wait.args(["alice", "--board", BOARD, "--timeout", timeout]);
    wait
}

fn worker_status(board: &Fixture) -> Vec<Value> {
    let mut command = board.recipe("workers");
    command.args(["alice", "--board", BOARD, "--json"]);
    let status: Value = serde_json::from_str(&run(command)).unwrap();
    status["workers"].as_array().unwrap().clone()
}

#[test]
fn worker_start_again_retires_an_enrolment_after_a_clean_exit() {
    let board = Fixture::new();
    board.start();
    let first = run(board.worker_start(&[], &["true"]));
    let old_inbox = field(&first, "Inbox");

    let second = run(board.worker_start(&["--again"], &["true"]));

    let new_inbox = field(&second, "Inbox");
    assert_ne!(old_inbox, new_inbox);
    let workers = worker_status(&board);
    assert_eq!(workers.len(), 2);
    assert!(
        workers
            .iter()
            .any(|row| row["inbox"] == old_inbox && row["enrolment"] == "revoked")
    );
    assert!(
        workers
            .iter()
            .any(|row| row["inbox"] == new_inbox && row["enrolment"] == "active")
    );
}

#[test]
fn worker_start_again_refuses_while_the_detached_launcher_is_live() {
    let board = Fixture::new();
    board.start();
    let worker = Detached::start(&board, GATE);

    let retry = output(board.worker_start(&["--again"], &["true"]));

    assert!(!retry.status.success());
    let reason = String::from_utf8_lossy(&retry.stderr);
    assert!(
        reason.contains(&format!("launcher PID {}", worker.group)),
        "{reason}"
    );
    assert_eq!(worker_status(&board)[0]["enrolment"], "active");
    fs::write(board.home.path().join("release"), "").unwrap();
    run(wait_report(&board, "30s"));
}

#[test]
fn worker_start_again_retires_a_detached_enrolment_after_exit() {
    let board = Fixture::new();
    board.start();
    let worker = Detached::start(&board, "true");
    let old_inbox = field(&worker.text, "Inbox");
    wait_for_event("the detached launcher to exit", || {
        worker_status(&board)
            .into_iter()
            .find(|row| row["state"] == "exited")
    });

    let relaunched = run(board.worker_start(&["--again"], &["true"]));

    assert_ne!(field(&relaunched, "Inbox"), old_inbox);
    let rows = worker_status(&board);
    assert!(
        rows.iter()
            .any(|row| row["inbox"] == old_inbox && row["enrolment"] == "revoked")
    );
}

#[test]
fn worker_start_again_refuses_while_foreground_launcher_waits_to_resume() {
    let board = Fixture::new();
    board.start();
    let mut worker = RunningWorker(
        board
            .worker_start(&["--resume", "true"], &["touch", "initial"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_for_file(&board, "initial");
    run(wait_report(&board, "30s"));
    let status = worker_status(&board);

    let retry = output(board.worker_start(&["--again"], &["true"]));

    run(board.owner(&["board", "stop"]));
    wait_for_event("the launcher to exit after the board stops", || {
        worker.0.try_wait().unwrap()
    });
    assert!(!retry.status.success());
    let reason = String::from_utf8_lossy(&retry.stderr);
    assert!(
        reason.contains(&format!("launcher PID {}", status[0]["launcher_pid"])),
        "{reason}"
    );
}

#[test]
fn workers_reports_the_running_foreground_launcher_and_its_child() {
    let board = Fixture::new();
    board.start();
    let script = format!("printf '%s %s' \"$PPID\" \"$$\" > pids; touch running; {GATE}");
    let child = board
        .worker_start(&[], &["sh", "-ec", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&board, "running");
    let pids = fs::read_to_string(board.home.path().join("pids")).unwrap();
    let (launcher, agent) = pids.split_once(' ').unwrap();
    let row = wait_for_event("the stock launcher to record the child PID", || {
        worker_status(&board)
            .into_iter()
            .find(|row| row["agent_pid"].is_number())
    });
    fs::write(board.home.path().join("release"), "").unwrap();
    stdout(child.wait_with_output().unwrap());

    assert_eq!(row["state"], "running");
    assert_eq!(row["launcher_pid"], launcher.parse::<u64>().unwrap());
    assert_eq!(row["agent_pid"], agent.parse::<u64>().unwrap());
    assert_eq!(row["inbox"], format!("to/{}", board.worker_id()));
    assert!(row["started_at_ms"].as_u64().unwrap() > 0);
    assert!(row["exit_code"].is_null());
}

#[test]
fn workers_reports_the_exit_code_collected_by_the_launcher() {
    let board = Fixture::new();
    board.start();
    let result = output(board.worker_start(&[], &["sh", "-c", "exit 7"]));
    assert_eq!(result.status.code(), Some(7));

    let rows = worker_status(&board);

    assert_eq!(rows[0]["state"], "exited");
    assert_eq!(rows[0]["exit_code"], 7);
    assert!(rows[0]["agent_pid"].is_number());
}

#[test]
fn workers_tracks_the_detached_launcher_after_dispatch_returns() {
    let board = Fixture::new();
    board.start();
    let worker = Detached::start(&board, GATE);

    let running = worker_status(&board);
    assert_eq!(running[0]["state"], "running");
    assert_eq!(running[0]["launcher_pid"], worker.group.as_raw());
    fs::write(board.home.path().join("release"), "").unwrap();
    let exited = wait_for_event("the detached launcher to exit", || {
        worker_status(&board)
            .into_iter()
            .find(|row| row["state"] == "exited")
    });

    assert_eq!(exited["launcher_pid"], running[0]["launcher_pid"]);
    assert_eq!(exited["exit_code"], 0);
}

#[test]
fn mailbox_roster_shows_the_worker_launcher_state() {
    let board = Fixture::new();
    board.start();
    run(board.worker_start(&[], &["true"]));
    let mut command = board.owner(&["mailbox", "--who"]);
    board.recipe_environment(&mut command);

    let roster = run(command);

    assert!(roster.contains("alice exited"), "{roster}");
}

#[test]
fn detached_launch_returns_while_the_worker_is_still_running() {
    let board = Fixture::new();
    board.start();

    let worker = Detached::start(
        &board,
        &format!(r#"touch running; {GATE} test -d "$MALAK_HOOK_STATE"; echo completed"#),
    );

    wait_for_file(&board, "running");
    let inbox = format!("to/{}", board.worker_id());
    assert_eq!(field(&worker.text, "Inbox"), inbox);
    assert_eq!(
        field(&worker.text, "Wait"),
        "malak --board malak recipe await alice"
    );
    assert!(field(&worker.text, "Follow").starts_with("tail -f "));
    assert!(messages(&board.lead(&["read", "lead", "--json"])).is_empty());
    fs::write(board.home.path().join("release"), "").unwrap();
    assert!(run(wait_report(&board, "30s")).contains("completed"));
}

#[test]
fn detached_worker_prints_working_stop_and_revoke_commands() {
    let board = Fixture::new();
    board.start();
    let worker = Detached::start(
        &board,
        &format!(r#"trap 'touch stopped; exit 0' TERM; touch running; {GATE}"#),
    );
    wait_for_file(&board, "running");

    for label in ["Revoke", "Stop"] {
        let mut command = Command::new("sh");
        board.recipe_environment(&mut command);
        command.args(["-ec", field(&worker.text, label)]);
        run(command);
    }

    wait_for_file(&board, "stopped");
    assert_eq!(participant(&board.status(), "alice")["revoked"], true);
}

#[test]
fn detached_launch_failure_revokes_the_unstarted_worker() {
    let board = Fixture::new();
    board.start();

    let failed = output(board.worker_start(&["--detach"], &["./missing-executable"]));

    assert_eq!(failed.status.code(), Some(3), "{failed:?}");
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("started in the background"));
    assert_eq!(participant(&board.status(), "alice")["revoked"], true);
}

#[test]
fn await_ignores_progress_from_the_same_worker() {
    let board = Fixture::new();
    board.start();
    let _worker = Detached::start(
        &board,
        &format!(
            "malak send --kind report lead 'Worker exit status: 0, progress'; \
             touch progressed; {GATE} echo 'exact final result'"
        ),
    );
    wait_for_file(&board, "progressed");

    let early = output(wait_report(&board, "1"));

    assert_eq!(early.status.code(), Some(3), "{early:?}");
    assert!(String::from_utf8_lossy(&early.stderr).contains("timed out"));
    assert!(early.stdout.is_empty());
    fs::write(board.home.path().join("release"), "").unwrap();
    assert!(run(wait_report(&board, "1m")).contains("exact final result"));
    assert_eq!(messages(&board.lead(&["read", "lead", "--json"])).len(), 2);
}

#[test]
fn await_prints_the_final_report_and_returns_the_worker_failure_status() {
    let board = Fixture::new();
    board.start();
    let initial = output(board.worker_start(&[], &["sh", "-c", "echo failed result; exit 7"]));
    assert_eq!(initial.status.code(), Some(7), "{initial:?}");
    board.lead(&["read", "--ack", "lead"]);

    let waited = output(wait_report(&board, "1h"));

    assert_eq!(waited.status.code(), Some(7), "{waited:?}");
    let text = String::from_utf8(waited.stdout).unwrap();
    assert_eq!(field(&text, "Worker exit"), "7");
    assert!(text.contains("failed result"), "{text}");
}
