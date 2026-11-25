//! `board start --detach` runs in the background and exits once the board is ready.

use super::*;
use std::os::unix::fs::PermissionsExt;

struct Detached {
    config: PathBuf,
    socket: BoardHome,
}

impl Detached {
    fn new() -> Self {
        let socket = BoardHome::new();
        let config = write_config(socket.path(), "markdown");
        Self { config, socket }
    }

    fn start_with(&self, extra: &[&str]) -> std::process::Output {
        Command::new(MALAK)
            .env("HOME", self.socket.path())
            .args(["board", "start", "--detach", "--config"])
            .arg(&self.config)
            .args(extra)
            .output()
            .unwrap()
    }

    fn start(&self) -> std::process::Output {
        self.start_with(&[])
    }

    fn log(&self) -> PathBuf {
        self.socket.path().join("st/board.log")
    }

    fn status(&self) -> std::process::Output {
        Command::new(MALAK)
            .args(["board", "status", "--json", "--config"])
            .arg(&self.config)
            .output()
            .unwrap()
    }

    fn stop(&self) -> std::process::Output {
        Command::new(MALAK)
            .args(["board", "stop", "--config"])
            .arg(&self.config)
            .output()
            .unwrap()
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn detach_returns_once_the_board_accepts_connections() {
    let board = Detached::new();

    let started = board.start();

    assert!(started.status.success(), "{}", stderr(&started));
    assert!(board.status().status.success());
}

#[test]
fn detach_immediately_after_stop_serves_a_new_run() {
    let board = Detached::new();
    assert!(board.start().status.success());
    let previous: Value = serde_json::from_str(&stdout(&board.status())).unwrap();

    let stopped = board.stop();
    let started = board.start();
    let status = board.status();

    assert!(stopped.status.success(), "{}", stderr(&stopped));
    assert!(started.status.success(), "{}", stderr(&started));
    assert!(
        stdout(&started).starts_with("Board ready."),
        "start: {}status: {}{}",
        stdout(&started),
        stdout(&status),
        stderr(&status)
    );
    assert!(status.status.success(), "{}", stderr(&status));
    let current: Value = serde_json::from_str(&stdout(&status)).unwrap();
    assert_ne!(current["run"], previous["run"]);
}

#[test]
fn detach_prints_the_log_path_and_does_not_hold_the_callers_stderr() {
    let board = Detached::new();

    // Collecting both pipes to the end only returns if the board let go of them.
    let started = board.start();

    assert_eq!(
        stdout(&started),
        format!("Board ready.\n  Log  {}\n", board.log().display())
    );
    assert!(stderr(&started).is_empty());
    assert!(
        std::fs::read_to_string(board.log())
            .unwrap()
            .contains("ready")
    );
}

#[test]
fn detach_reports_the_existing_board_without_starting_another() {
    let board = Detached::new();
    assert!(board.start().status.success());
    let log = std::fs::read(board.log()).unwrap();

    let again = board.start();

    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(
        stdout(&again),
        format!("Board already running.\n  Log  {}\n", board.log().display())
    );
    assert_eq!(std::fs::read(board.log()).unwrap(), log);
}

#[test]
fn a_board_still_starting_when_the_wait_ends_is_left_running() {
    let board = Detached::new();

    let slow = board.start_with(&["--wait", "0"]);

    assert_eq!(slow.status.code(), Some(3));
    let message = stderr(&slow);
    assert!(message.contains("still starting"), "{message}");
    assert!(message.contains(board.log().to_str().unwrap()));
    assert!(message.contains("malak board status"));
    let until = std::time::Instant::now() + Duration::from_secs(15);
    while !board.status().status.success() {
        assert!(std::time::Instant::now() < until, "board never came up");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn verbose_detach_shows_the_startup_steps() {
    let board = Detached::new();

    let started = board.start_with(&["--verbose"]);

    let shown = stderr(&started);
    let steps: Vec<_> = shown
        .lines()
        .filter_map(|line| line.strip_prefix("Startup: "))
        .filter(|line| !line.starts_with("Warning: ") && !line.starts_with("  Connections  "))
        .collect();
    assert_eq!(
        steps,
        [
            "state folder checked",
            "socket folder locked",
            "entrance sockets prepared",
            "store opened",
            "entrances listening",
            "ready",
        ]
    );
}

#[test]
fn detach_reports_a_failed_start_with_its_exit_code_and_leaves_no_board() {
    let board = Detached::new();
    let door = board.socket.path().join("d/o");
    std::fs::create_dir_all(&door).unwrap();
    for folder in [door.parent().unwrap(), &door] {
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(door.join("s"), "").unwrap();

    let failed = board.start();

    assert_eq!(failed.status.code(), Some(2));
    assert!(stderr(&failed).contains("occupied"), "{}", stderr(&failed));
    assert!(!board.status().status.success());
    std::fs::remove_file(door.join("s")).unwrap();
    assert!(board.start().status.success());
}

#[test]
fn detach_refuses_a_link_at_the_log_path() {
    let board = Detached::new();
    let target = board.socket.path().join("outside.log");
    std::fs::write(&target, "unchanged\n").unwrap();
    std::fs::create_dir(board.log().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, board.log()).unwrap();

    let started = board.start();

    assert_eq!(std::fs::read_to_string(target).unwrap(), "unchanged\n");
    assert_eq!(started.status.code(), Some(2), "{}", stdout(&started));
    assert!(stderr(&started).contains(board.log().to_str().unwrap()));
}

#[test]
fn detach_refuses_a_log_writable_by_other_accounts() {
    let board = Detached::new();
    let log = board.log();
    std::fs::create_dir(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "unchanged\n").unwrap();
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o622)).unwrap();

    let started = board.start();

    assert_eq!(started.status.code(), Some(2), "{}", stdout(&started));
    assert!(stderr(&started).contains(log.to_str().unwrap()));
    assert_eq!(std::fs::read_to_string(log).unwrap(), "unchanged\n");
}
