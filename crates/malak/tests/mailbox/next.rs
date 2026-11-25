use super::*;

const NEXT: &[&str] = &["mailbox", "--plain", "--for", "lead", "--next"];

fn tick(board: &Board) -> String {
    board.run(&[NEXT, &["--timeout", "1"]].concat())
}

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

fn send_report(board: &Board, worker: &Path, body: &str) -> Receipt {
    let mut command = command(
        board.root.path(),
        &["send", "--json", "lead", "--kind", "report", body],
    );
    Receipt::parse(&run(command.env("MALAK_HANDOVER", worker)))
}

fn reporter(board: &Board) -> PathBuf {
    let (id, worker) = board.enrol("reviewer");
    board.run(&["grant", &id, "lead:send"]);
    worker
}

fn make_mail_reminder_due(board: &Board) {
    let file = board.root.path().join("st/mailbox-next-lead.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let shown = state["reminder"]["at_ms"].as_u64().unwrap();
    state["reminder"]["at_ms"] = (shown - 60 * 60 * 1_000).into();
    std::fs::write(file, serde_json::to_vec(&state).unwrap()).unwrap();
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
fn concurrent_next_starts_share_the_delivery_cursor() {
    // Each new board makes all watchers contend to create the delivery lock.
    for _ in 0..12 {
        let board = Board::start();
        let worker = reporter(&board);
        send_report(&board, &worker, "One report for concurrent watchers");

        let start = std::sync::Barrier::new(2);
        let watch = || {
            start.wait();
            tick(&board)
        };
        let output = std::thread::scope(|scope| {
            let peer = scope.spawn(watch);
            [watch(), peer.join().unwrap()]
        });

        assert_eq!(
            output.iter().filter(|text| *text == "No events.\n").count(),
            1,
            "{output:?}"
        );
        assert!(
            output
                .iter()
                .any(|text| text.contains("One report for concurrent watchers")),
            "{output:?}"
        );
    }
}

#[test]
fn next_leaves_the_leads_open_question_in_the_questions_list() {
    let board = Board::start();
    board.send("questions", "note", "Open for the owner");

    assert_eq!(tick(&board), "No events.\n");
    assert!(
        board
            .run(&["mailbox", "--questions"])
            .contains("Open for the owner")
    );
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
fn next_waits_for_a_live_order_from_another_participant() {
    let board = Board::start();
    let (id, operator) = board.enrol("operator");
    board.run(&["grant", &id, "orders:send"]);
    let (mut watcher, lines) = start(&board);

    run(command(
        board.root.path(),
        &["send", "orders", "Choose the next check"],
    )
    .env("MALAK_HANDOVER", operator));

    let message = block_with(&lines, "operator sent a note to orders #1");
    assert!(message.contains("Choose the next check"), "{message}");
    assert!(watcher.0.wait().unwrap().success());
}

#[test]
fn next_ignores_the_leads_live_send() {
    let board = Board::start();
    let worker = reporter(&board);
    let (mut watcher, lines) = start(&board);

    board.send("orders", "note", "My own order");
    send_report(&board, &worker, "New report");

    let first = lines.recv_timeout(WATCHDOG).unwrap();
    assert!(
        first.contains("reviewer sent a report to lead #1"),
        "{first}"
    );
    assert!(watcher.0.wait().unwrap().success());
}

#[test]
fn next_ignores_the_leads_live_recipe_ask() {
    let board = Board::start();
    let worker = reporter(&board);
    let (mut watcher, lines) = start(&board);

    board.recipe_ask(&board.root.path().join("st/lead.handover"), "Ask the owner");
    send_report(&board, &worker, "New report");

    let first = lines.recv_timeout(WATCHDOG).unwrap();
    assert!(
        first.contains("reviewer sent a report to lead #1"),
        "{first}"
    );
    assert!(watcher.0.wait().unwrap().success());
}

#[test]
fn next_reports_a_workers_question_once() {
    let board = Board::start();
    let (_, worker) = board.asker("reviewer");
    let (mut watcher, lines) = start(&board);

    board.ask(&worker, "Ask the lead");

    let message = block_with(&lines, "reviewer sent a note to questions #1");
    assert!(message.contains("Ask the lead"), "{message}");
    assert!(watcher.0.wait().unwrap().success());
    assert_eq!(tick(&board), "No events.\n");
}

#[test]
fn next_catches_up_with_a_workers_question_posted_between_runs() {
    let board = Board::start();
    let (_, worker) = board.asker("reviewer");
    assert_eq!(tick(&board), "No events.\n");
    board.ask(&worker, "Posted between ticks");

    let output = tick(&board);

    assert!(output.contains("Posted between ticks"), "{output}");
}

#[test]
fn next_skips_shown_unacknowledged_mail_after_a_board_restart() {
    let mut board = Board::start();
    let worker = reporter(&board);
    send_report(&board, &worker, "First report");
    assert!(board.run(NEXT).contains("First report"));
    board.restart();

    assert_eq!(tick(&board), "No events.\n");
}

#[test]
fn next_prints_new_waiting_mail_while_a_shown_report_stays_unacknowledged() {
    let board = Board::start();
    let worker = reporter(&board);
    send_report(&board, &worker, "First report");
    assert!(board.run(NEXT).contains("First report"));
    send_report(&board, &worker, "New report");

    let output = board.run(NEXT);

    assert!(output.contains("New report"), "{output}");
    assert!(!output.contains("First report"), "{output}");
}

#[test]
fn next_repeats_unacknowledged_mail_when_the_hourly_reminder_is_due() {
    let board = Board::start();
    let worker = reporter(&board);
    send_report(&board, &worker, "Still waiting");
    assert!(board.run(NEXT).contains("Still waiting"));
    make_mail_reminder_due(&board);

    assert!(tick(&board).contains("Still waiting"));
    assert_eq!(tick(&board), "No events.\n");
}

#[test]
fn next_skips_the_hourly_reminder_for_acknowledged_mail() {
    let board = Board::start();
    let worker = reporter(&board);
    let receipt = send_report(&board, &worker, "Handled");
    assert!(board.run(NEXT).contains("Handled"));
    board.ack_as(
        &board.root.path().join("st/lead.handover"),
        "lead",
        &receipt,
    );
    make_mail_reminder_due(&board);

    assert_eq!(tick(&board), "No events.\n");
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
