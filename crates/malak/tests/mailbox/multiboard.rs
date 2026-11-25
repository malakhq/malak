use super::*;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

struct Home(SocketDir);

impl Home {
    fn new() -> Self {
        let home = Self(SocketDir::new().unwrap());
        fs::create_dir(home.0.path().join(".malak")).unwrap();
        home
    }

    fn register(&self, board: &Board) {
        let mut list = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.0.path().join(".malak/boards.jsonl"))
            .unwrap();
        let entry = serde_json::to_string(&board.root.path().join("board.toml")).unwrap();
        writeln!(list, "{entry}").unwrap();
    }

    fn copy_config(&self, board: &Board, relative: &str) {
        let destination = self.0.path().join(relative);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(board.root.path().join("board.toml"), destination).unwrap();
    }

    fn write_default_config(&self, state: &Path, sockets: &Path) {
        let path = self.0.path().join(".config/malak/board.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!("[board]\nstate = {state:?}\nsocket_folder = {sockets:?}\n"),
        )
        .unwrap();
    }

    fn mailbox(&self, args: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        command
            .env("HOME", self.0.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("MALAK_HANDOVER")
            .env("TERM", "xterm")
            .env("NO_COLOR", "1")
            .arg("mailbox")
            .args(args);
        command
    }

    fn terminal(&self, columns: u16) -> (Running, Screen) {
        self.terminal_with(columns, &[])
    }

    fn terminal_with(&self, columns: u16, args: &[&str]) -> (Running, Screen) {
        terminal(self.mailbox(args), columns)
    }
}

fn terminal(mut command: Command, columns: u16) -> (Running, Screen) {
    let (master, slave) = terminal_pair(30, columns);
    let process = command
        .stdin(slave.try_clone().unwrap())
        .stdout(slave)
        .spawn()
        .unwrap();
    (
        Running(process),
        Screen {
            terminal: TerminalOutput::new(master),
            columns,
            rows: 30,
        },
    )
}

struct Screen {
    terminal: TerminalOutput,
    columns: u16,
    rows: u16,
}

impl Screen {
    fn text(&self) -> String {
        render_screen(&self.terminal.bytes, self.columns, self.rows)
    }

    fn wait_for(&mut self, text: &str) {
        let (columns, rows) = (self.columns, self.rows);
        self.terminal.wait_for_output(text, |bytes| {
            render_screen(bytes, columns, rows).contains(text)
        });
    }

    fn press(&mut self, bytes: &[u8]) {
        self.terminal.press(bytes);
    }
}

fn board(name: &str) -> Board {
    Board::start_with(&format!("name = {name:?}"))
}

fn stop(board: &mut Board) {
    board.run(&["board", "stop"]);
    assert!(board.process.0.wait().unwrap().success());
}

#[test]
fn implicit_live_mailbox_discovers_the_list_home_folders_and_default() {
    let listed = board("listed");
    let home_board = Board::start();
    let default = Board::start();
    let home = Home::new();
    home.register(&listed);
    home.copy_config(&home_board, ".malak/folder/board.toml");
    home.copy_config(&default, ".config/malak/board.toml");

    let (_mailbox, mut terminal) = home.terminal(120);

    terminal.wait_for("Malak · 3 boards");
    for board in ["default ●", "folder ●", "listed ●"] {
        terminal.wait_for(board);
    }
}

#[test]
fn a_board_added_stopped_and_removed_updates_the_open_mailbox() {
    let home = Home::new();
    let active = board("active");
    home.register(&active);
    let (_mailbox, mut terminal) = home.terminal(120);
    terminal.wait_for("active ●");

    let mut arriving = board("arriving");
    home.register(&arriving);
    terminal.wait_for("arriving ●");

    stop(&mut arriving);
    terminal.wait_for("arriving ○ stopped");

    fs::remove_file(arriving.root.path().join("board.toml")).unwrap();
    terminal
        .terminal
        .wait_for_output("the removed board to disappear", |bytes| {
            let screen = render_screen(bytes, terminal.columns, terminal.rows);
            !screen.contains("arriving") && !screen.contains("All boards")
        });
}

#[test]
fn a_slow_owner_handshake_does_not_delay_another_boards_live_events() {
    let home = Home::new();
    let slow = board("slow");
    let active = board("active");
    home.register(&slow);
    home.register(&active);
    let owner_socket = slow.root.path().join("d/o/s");
    fs::rename(&owner_socket, owner_socket.with_file_name("held")).unwrap();
    let listener = UnixListener::bind(owner_socket).unwrap();
    let (accepted, connected) = mpsc::sync_channel(1);
    let _accept = std::thread::spawn(move || {
        let _ = accepted.send(listener.accept().unwrap().0);
    });

    let (_mailbox, mut terminal) = home.terminal(120);
    let _held_connection = connected.recv_timeout(WATCHDOG).unwrap();
    terminal.wait_for("active ●");
    active.send(
        "lead",
        "report",
        "active while the other owner handshake waits",
    );

    terminal.wait_for("active while the other owner handshake waits");
}

#[test]
fn a_config_that_cannot_be_read_has_its_reason_on_the_board_row() {
    let active = board("active");
    let bad = board("bad");
    let home = Home::new();
    home.register(&active);
    home.register(&bad);
    fs::set_permissions(
        bad.root.path().join("board.toml"),
        fs::Permissions::from_mode(0o666),
    )
    .unwrap();

    let (_mailbox, mut terminal) = home.terminal(80);

    terminal.wait_for("✕");
    terminal.wait_for("permissions");
    terminal.wait_for("active ●");
}

#[test]
fn a_running_board_on_another_build_shows_the_restart_note() {
    let active = board("active");
    let older = board("legacy");
    let home = Home::new();
    home.register(&active);
    home.register(&older);
    fs::write(
        older.root.path().join("d/o/version"),
        format!("{} 0.0.1\n", older.process.0.id()),
    )
    .unwrap();

    let (_mailbox, mut terminal) = home.terminal(80);

    terminal.wait_for("legacy ● ⚠ older build, restart");
    terminal.wait_for("active ●");
}

#[test]
fn a_one_shot_view_uses_only_the_default_board() {
    let default = Board::start();
    let other = board("other");
    let home = Home::new();
    home.copy_config(&default, ".config/malak/board.toml");
    home.register(&other);
    default.send("questions", "note", "Owner default question");
    other.send("questions", "note", "Owner other question");

    let output = run(&mut home.mailbox(&["--questions", "--json"]));

    assert!(output.contains("Owner default question"), "{output}");
    assert!(!output.contains("Owner other question"), "{output}");
}

#[test]
fn all_boards_names_the_upgrade_notice_sender_owner() {
    let home = Home::new();
    let first = board("first");
    let second = board("second");
    home.register(&first);
    home.register(&second);
    first.post_upgrade_notice();

    let (_mailbox, mut terminal) = home.terminal(120);

    terminal.wait_for("@owner → @lead · note #1 · [BOARD: FIRST]");
}

#[test]
fn the_combined_activity_feed_opens_a_trace_on_the_selected_board() {
    let home = Home::new();
    let first = board("first");
    let second = board("second");
    home.register(&first);
    home.register(&second);
    first.run(&[
        "send",
        "lead",
        "--trace",
        TRACE,
        "First board matching trace",
    ]);
    second.run(&[
        "send",
        "lead",
        "--trace",
        TRACE,
        "Second board matching trace",
    ]);

    let (_mailbox, mut terminal) = home.terminal(120);
    terminal.wait_for("[BOARD: FIRST]");
    terminal.wait_for("[BOARD: SECOND]");
    terminal.wait_for("Second board matching trace");
    let all = terminal.text().replace(' ', "");
    assert_in_order(&all, &["[BOARD:SECOND]", "[BOARD:FIRST]"]);
    terminal.press(b"t");

    terminal.wait_for("1 message");
    terminal.wait_for("Second board matching trace");
    assert!(!terminal.text().contains("First board matching trace"));
}

#[test]
fn an_open_board_has_participants_and_archive_at_80_columns() {
    let home = Home::new();
    let active = board("active");
    let other = board("other");
    home.register(&active);
    home.register(&other);
    let (retired, _) = active.enrol("retired");
    active.run(&["revoke", &retired]);
    active.enrol("active-worker");
    let (_mailbox, mut terminal) = home.terminal(80);
    terminal.wait_for("active ●");
    terminal.press(b"\t\x1b[B\x1b[C");

    terminal.wait_for("Archived (1)");
    terminal.wait_for("@active-worker");
    terminal.press(b"a");
    terminal.wait_for("@retired");
}

#[test]
fn an_inherited_handover_does_not_hide_other_boards_previews() {
    let home = Home::new();
    let active = board("active");
    let other = board("other");
    home.register(&active);
    home.register(&other);
    let (_, inherited) = active.enrol("worker");
    other.send("lead", "report", "Second board retained report");

    let mut command = home.mailbox(&[]);
    command.env("MALAK_HANDOVER", inherited);
    let (_mailbox, mut screen) = terminal(command, 80);

    screen.wait_for("Second board retained report");
}

#[test]
fn all_boards_previews_reopen_after_the_other_lead_is_rekeyed() {
    let home = Home::new();
    let first = board("first");
    let second = board("second");
    home.register(&first);
    home.register(&second);
    let (_, inherited) = first.enrol("worker");
    let initial = "Second board before rekey";
    let after = "Second board after rekey";
    let mut command = home.mailbox(&[]);
    command.env("MALAK_HANDOVER", inherited);
    let (_mailbox, mut screen) = terminal(command, 120);
    screen.wait_for("All boards");
    screen.wait_for("second ●");
    second.send("lead", "report", initial);
    screen.wait_for(initial);

    second.run(&[
        "rekey",
        "lead",
        "--out",
        second
            .root
            .path()
            .join("st/lead.handover")
            .to_str()
            .unwrap(),
    ]);
    second.send("lead", "report", after);

    screen.wait_for(after);
}

#[test]
fn all_boards_previews_do_not_report_the_lead_reading() {
    let home = Home::new();
    let first = board("first");
    let second = board("second");
    home.register(&first);
    home.register(&second);
    let (_watcher, lines) = second.plain();
    let (_mailbox, mut screen) = home.terminal(120);
    let message = second.send("lead", "report", "Preview from the second board");
    screen.wait_for("Preview from the second board");

    second.run(&["read", "lead"]);
    let mut read_positions = Vec::new();
    let marker = second.send("orders", "note", "End of observation");
    let deadline = Instant::now() + WATCHDOG;
    loop {
        let line = lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if let Some((_, position)) = line.split_once("lead read lead #") {
            read_positions.push(position.to_owned());
        }
        if line.contains(&format!("lead sent a note to orders #{}", marker.position)) {
            break;
        }
    }
    assert_eq!(
        read_positions
            .iter()
            .filter(|&position| position == &message.position)
            .count(),
        1,
        "{read_positions:?}"
    );
}

#[test]
fn plain_activity_uses_only_the_default_board() {
    let default = Board::start();
    let other = board("other");
    let home = Home::new();
    home.copy_config(&default, ".config/malak/board.toml");
    home.register(&other);
    other.send("lead", "note", "Other board retained message");
    default.send("lead", "note", "Default board retained marker");

    let (_mailbox, lines) = launch_plain(home.mailbox(&["--plain"]));

    loop {
        let line = lines.recv_timeout(WATCHDOG).unwrap();
        assert!(!line.contains("Other board retained message"), "{line}");
        if line.contains("Default board retained marker") {
            break;
        }
    }
}

#[test]
fn an_explicit_board_name_opens_the_single_board_view() {
    let home = Home::new();
    let active = board("active");
    let picked = Board::start();
    home.register(&active);
    home.copy_config(&picked, ".malak/picked/board.toml");
    active.send("lead", "report", "Other board activity");
    picked.send("lead", "report", "Selected board activity");

    let (_mailbox, mut screen) = home.terminal_with(120, &["--board", "picked"]);
    screen.wait_for("Malak · picked");
    screen.wait_for("Selected board activity");

    let text = screen.text();
    assert!(text.contains("PARTICIPANTS"), "{text}");
    assert!(!text.contains("Other board activity"), "{text}");
    assert!(!text.contains("[BOARD:"), "{text}");
}

#[test]
fn two_configs_for_one_board_show_one_activity_feed() {
    let home = Home::new();
    let board = Board::start();
    home.copy_config(&board, ".config/malak/board.toml");
    home.copy_config(&board, ".malak/lanes/board.toml");
    board.send("lead", "report", "One retained report");

    let (_mailbox, mut screen) = home.terminal(120);
    screen.wait_for("One retained report");

    let text = screen.text();
    assert!(text.contains("Malak · lanes"), "{text}");
    assert!(!text.contains("All boards"), "{text}");
    assert_eq!(text.matches("One retained report").count(), 1, "{text}");
}

#[test]
fn either_config_name_selects_the_same_board_with_a_canonical_state_alias() {
    let home = Home::new();
    let board = Board::start();
    home.copy_config(&board, ".malak/lanes/board.toml");
    let state = board.root.path().join("st");
    let alias = home.0.path().join("state-alias");
    std::os::unix::fs::symlink(&state, &alias).unwrap();
    home.write_default_config(&alias, &home.0.path().join("unused-sockets"));
    board.send("questions", "note", "One owner question");

    for name in ["default", "lanes"] {
        let output = run(&mut home.mailbox(&["--board", name, "--questions", "--json"]));
        assert!(output.contains("One owner question"), "{output}");
    }
}

#[test]
fn a_shared_socket_folder_also_resolves_both_config_names_to_one_board() {
    let home = Home::new();
    let board = Board::start();
    home.copy_config(&board, ".malak/lanes/board.toml");
    home.write_default_config(
        &home.0.path().join("unused-state"),
        &board.root.path().join("d"),
    );
    board.send("questions", "note", "Question on the shared socket");

    let output = run(&mut home.mailbox(&["--board", "default", "--questions", "--json"]));

    assert!(output.contains("Question on the shared socket"), "{output}");
}
