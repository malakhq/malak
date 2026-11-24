use super::*;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Instant;

const WAIT: Duration = Duration::from_secs(5);

#[test]
fn stop_reaches_a_live_owner_tail_before_it_closes() {
    let board = Fixture::new();
    let tail = Output::start(&mut board.owner_command(&["tail", "--json"]));
    ready_tails(&board, &[&tail]);

    let target = stop(&board);

    tail.assert_stop(&target);
}

#[test]
fn stop_reaches_several_live_tails_including_the_mailbox() {
    let board = Fixture::new();
    let tails = [(); 2].map(|()| Output::start(&mut board.owner_command(&["tail", "--json"])));
    let mailbox = Output::start(
        board
            .owner_command(&["mailbox", "--plain"])
            .env("MALAK_HANDOVER", board.lead()),
    );
    ready_tails(&board, &[&tails[0], &tails[1]]);
    mailbox.expect_line(|line| line.contains("sent a note to"));

    let target = stop(&board);

    for tail in tails {
        tail.assert_stop(&target);
    }
    mailbox.expect_line(|line| {
        line.contains(&format!("Owner STOP for {target}")) && line.contains("(done)")
    });
    assert!(mailbox.wait_for_exit().success());
}

fn ready_tails(board: &Fixture, tails: &[&Output]) {
    let deadline = Instant::now() + WAIT;
    loop {
        call(&board.lead(), &["send", "lead", "tail readiness"]);
        if tails.iter().all(
            |tail| match tail.lines.recv_timeout(Duration::from_millis(25)) {
                Ok(line) => serde_json::from_str::<Value>(&line).unwrap()["event"] == "stored",
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => panic!("tail closed during startup"),
            },
        ) {
            return;
        }
        assert!(Instant::now() < deadline, "tails did not start");
    }
}

fn stop(board: &Fixture) -> String {
    let target = board.status()["run"].as_str().unwrap().to_owned();
    run(&mut board.owner_command(&["board", "stop", "--run", &target]));
    target
}

struct Output {
    child: Child,
    lines: Receiver<String>,
}

impl Output {
    fn start(command: &mut Command) -> Self {
        let mut child = command.stdout(Stdio::piped()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self { child, lines }
    }

    fn expect_line(&self, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(remaining)
                .expect("expected output from live tail");
            if matches(&line) {
                return;
            }
        }
    }

    fn assert_stop(self, target: &str) {
        self.expect_line(|line| {
            let event: Value = serde_json::from_str(line).unwrap();
            event["operation"] == "STOP" && event["outcome"] == "done" && event["target"] == target
        });
        assert!(self.wait_for_exit().success());
    }

    fn wait_for_exit(mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(exit) = self.child.try_wait().unwrap() {
                return exit;
            }
            assert!(Instant::now() < deadline, "tail did not close after STOP");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
