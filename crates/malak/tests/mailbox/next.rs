use super::*;

const NEXT: &[&str] = &["mailbox", "--plain", "--for", "lead", "--next"];

fn start(board: &Board) -> (Running, Receiver<String>) {
    let mut cmd = command(board.root.path(), NEXT);
    cmd.stderr(Stdio::piped());
    let (mut watcher, lines) = launch_plain(cmd);
    let door = std::fs::File::open(board.root.path().join("d/o")).unwrap();
    let deadline = Instant::now() + WATCHDOG;
    loop {
        match door.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => return (watcher, lines),
            Ok(()) => door.unlock().unwrap(),
            Err(error) => panic!("could not check watcher lock: {error}"),
        }
        assert!(
            watcher.0.try_wait().unwrap().is_none(),
            "watcher exited before waiting"
        );
        assert!(Instant::now() < deadline, "watcher did not register");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn send_report(board: &Board, worker: &Path, body: &str) {
    run(command(
        board.root.path(),
        &["send", "lead", "--kind", "report", body],
    )
    .env("MALAK_HANDOVER", worker));
}

fn reporter(board: &Board) -> PathBuf {
    let (id, worker) = board.enrol("reviewer");
    board.run(&["grant", &id, "lead:send"]);
    worker
}

#[test]
fn next_prints_waiting_lead_messages_and_exits() {
    let board = Board::start();
    let worker = reporter(&board);
    send_report(&board, &worker, "Waiting report");
    send_report(&board, &worker, "Another waiting report");

    let output = board.run(NEXT);

    assert_in_order(
        &output,
        &[
            "reviewer sent a report to lead #1",
            "Waiting report",
            "reviewer sent a report to lead #2",
            "Another waiting report",
        ],
    );
    assert!(!output.contains("Mailbox activity"), "{output}");
}

#[test]
fn next_prints_open_owner_questions_on_startup() {
    let board = Board::start();
    let answered = board.send("questions", "note", "Already answered");
    board.reply("questions", "questions", &answered, "My answer");
    let (_, worker) = board.asker("reviewer");
    board.ask(&worker, "Agent question");
    board.send("questions", "note", "Open for the owner");

    let output = board.run(NEXT);

    assert!(output.contains("Open for the owner"), "{output}");
    for old in ["Already answered", "My answer", "Agent question"] {
        assert!(!output.contains(old), "{output}");
    }
}

#[test]
fn next_waits_silently_then_prints_one_live_message() {
    let board = Board::start();
    let worker = reporter(&board);
    let (mut watcher, lines) = start(&board);
    assert_eq!(
        lines.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );

    send_report(&board, &worker, "New report");

    let message = block_with(&lines, "reviewer sent a report to lead #1");
    assert!(message.contains("New report"), "{message}");
    assert!(watcher.0.wait().unwrap().success());
    assert!(lines.try_iter().next().is_none());
}

#[test]
fn next_waits_for_a_live_order() {
    let board = Board::start();
    let (mut watcher, lines) = start(&board);

    board.post_order(None, "Choose the next check");

    let message = block_with(&lines, "lead sent a note to orders #1");
    assert!(message.contains("Choose the next check"), "{message}");
    assert!(watcher.0.wait().unwrap().success());
}

#[test]
fn next_waits_for_a_live_owner_question() {
    let board = Board::start();
    let (mut watcher, lines) = start(&board);

    board.send("questions", "note", "Ask the owner");

    let message = block_with(&lines, "lead sent a note to questions #1");
    assert!(message.contains("Ask the owner"), "{message}");
    assert!(watcher.0.wait().unwrap().success());
}

#[test]
fn next_survives_a_graceful_board_restart() {
    let mut board = Board::start();
    let worker = reporter(&board);
    let (mut watcher, lines) = start(&board);

    board.restart();
    send_report(&board, &worker, "After restart");

    let message = block_with(&lines, "reviewer sent a report to lead #1");
    assert!(message.contains("After restart"), "{message}");
    assert!(watcher.0.wait().unwrap().success());
}

#[test]
fn next_times_out_without_printing_acknowledged_mail_or_historical_orders() {
    let board = Board::start();
    let old = board.send("lead", "note", "Acknowledged mail");
    board.ack_as(&board.root.path().join("st/lead.handover"), "lead", &old);
    board.post_order(None, "Historical order");

    let output = board.run(&[NEXT, &["--timeout", "1"]].concat());

    assert_eq!(output, "No events.\n");
}

#[test]
fn next_returns_a_reason_after_the_board_stays_down() {
    let mut board = Board::start();
    let (mut watcher, lines) = start(&board);
    board.run(&["board", "stop"]);
    board.process.0.wait().unwrap();

    assert_eq!(
        lines.recv_timeout(WATCHDOG),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
    assert!(!watcher.0.wait().unwrap().success());
    let mut diagnostics = String::new();
    watcher
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut diagnostics)
        .unwrap();
    assert!(
        diagnostics.contains("Reason  the link dropped and could not reconnect"),
        "{diagnostics}"
    );
}
