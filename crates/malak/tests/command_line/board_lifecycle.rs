use super::*;
use std::fs::{self, Permissions};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use terminal::{TerminalOutput, terminal_pair};

#[path = "upgrade.rs"]
mod upgrade;

struct Home(SocketDir);

impl Home {
    fn new() -> Self {
        Self(SocketDir::new().unwrap())
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn command(&self, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        self.environment(&mut command);
        command.args(words);
        command
    }

    fn environment(&self, command: &mut Command) {
        command
            .env("HOME", self.path())
            .env("CARGO_HOME", self.path().join("cargo-home"))
            .env_remove("CARGO_INSTALL_ROOT")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("MALAK_HANDOVER");
    }

    fn board(&self) -> Fixture {
        Fixture::start(|path| spawn_board(self.command(&[]), path))
    }

    fn default_alias(&self, board: &Fixture) -> PathBuf {
        let path = self.path().join(".config/malak/board.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::copy(&board.config, &path).unwrap();
        path
    }

    fn list(&self) -> PathBuf {
        self.path().join(".malak/boards.jsonl")
    }
}

#[test]
fn board_start_records_an_external_config_once_in_a_private_list() {
    let home = Home::new();
    let mut board = home.board();
    run(&mut board.owner_command(&["board", "stop"]));
    assert!(board.board.wait().unwrap().success());
    board.board = spawn_board(home.command(&[]), &board.config);
    board.wait_until_ready();

    assert_eq!(fs::metadata(home.list()).unwrap().mode() & 0o777, 0o600);
    let listed: Vec<PathBuf> = fs::read_to_string(home.list())
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(listed, [board.config.clone()]);
}

#[test]
fn stop_all_stops_running_boards_and_lists_already_stopped_boards() {
    let home = Home::new();
    let mut left = home.board();
    let mut right = home.board();
    let mut stopped = home.board();
    run(&mut stopped.owner_command(&["board", "stop"]));
    assert!(stopped.board.wait().unwrap().success());
    let runs = [left.status()["run"].clone(), right.status()["run"].clone()];

    let output = run(&mut home.command(&["board", "stop", "--all"]));

    assert_eq!(output.matches("Board stopped.").count(), 2, "{output}");
    assert_eq!(
        output.matches("Board already stopped.").count(),
        1,
        "{output}"
    );
    for (board, run) in [(&mut left, &runs[0]), (&mut right, &runs[1])] {
        assert!(output.contains(board.config.to_str().unwrap()), "{output}");
        assert!(output.contains(run.as_str().unwrap()), "{output}");
        assert!(board.board.wait().unwrap().success());
    }
    assert!(
        output.contains(stopped.config.to_str().unwrap()),
        "{output}"
    );
}

#[test]
fn stop_all_stops_a_board_named_by_two_configs_once() {
    let home = Home::new();
    let mut board = home.board();
    home.default_alias(&board);

    let output = run(&mut home.command(&["board", "stop", "--all"]));

    assert_eq!(output.matches("Board stopped.").count(), 1, "{output}");
    assert!(!output.contains("Board already stopped."), "{output}");
    assert!(board.board.wait().unwrap().success());
}

#[test]
fn uninstall_plans_one_board_tree_and_both_config_files() {
    let home = Home::new();
    let board = home.board();
    let alias = home.default_alias(&board);

    let output = run(&mut home.command(&["uninstall", "--dry-run", "--delete-data"]));

    assert_eq!(output.matches("Would stop board.").count(), 1, "{output}");
    assert_eq!(output.matches("Board data").count(), 1, "{output}");
    let (_, plan) = output.split_once("Will delete\n").unwrap();
    let targets = [
        &board.socket.path().join("d"),
        &board.socket.path().join("st"),
        &board.config,
        &alias,
        &home.list(),
    ];
    let mut actual: Vec<_> = plan
        .lines()
        .take_while(|line| line.starts_with("  "))
        .map(str::trim)
        .collect();
    actual.sort_unstable();
    let mut expected = targets.map(|path| path.to_str().unwrap());
    expected.sort_unstable();
    assert_eq!(actual, expected, "{output}");
}

#[test]
fn bulk_stop_recognizes_a_board_finishing_an_earlier_stop() {
    use malak_core::{LocalClient, OwnerCredential};
    use malak_proto::encode_frame;
    use malak_transport_unix::UnixTransport;
    use std::sync::Arc;

    let home = Home::new();
    let mut board = home.board();
    let status = board.status();
    let run_id = status["run"].as_str().unwrap();
    let credential = OwnerCredential::read(board.socket.path().join("st")).unwrap();
    let mut owner = LocalClient::owner(Arc::new(UnixTransport::default()), credential).unwrap();
    let stop = encode_frame(
        1,
        "STOP",
        1,
        |fields| {
            fields.str("run").unwrap().str(run_id).unwrap();
        },
        &[],
    )
    .unwrap();
    owner.owner_call(stop, 1).unwrap();

    let output = run(&mut home.command(&["board", "stop", "--all"]));

    assert!(output.contains("Board already stopped."), "{output}");
    assert!(board.board.wait().unwrap().success());
}

#[test]
fn stop_all_finds_named_and_default_configs_without_a_good_list() {
    let home = Home::new();
    run(&mut home.command(&["board", "start", "--detach"]));
    let named = home.path().join(".malak/example");
    fs::create_dir(&named).unwrap();
    let config = write_config(&named, "markdown");
    run(home
        .command(&["board", "start", "--detach", "--config"])
        .arg(&config));
    fs::write(home.list(), b"damaged\n\"unterminated").unwrap();

    let output = run(&mut home.command(&["board", "stop", "--all"]));

    assert_eq!(output.matches("Board stopped.").count(), 2, "{output}");
    assert!(output.contains(config.to_str().unwrap()), "{output}");
    assert!(output.contains(".config/malak/board.toml"), "{output}");
}

#[test]
fn stop_all_reports_an_unstoppable_board_and_still_stops_the_others() {
    let home = Home::new();
    let stuck = home.board();
    let mut board = home.board();
    fs::remove_file(stuck.socket.path().join("st/owner.credential")).unwrap();

    let output = home.command(&["board", "stop", "--all"]).output().unwrap();

    assert_eq!(output.status.code(), Some(3));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Could not stop running board."), "{text}");
    assert!(text.contains("no owner credential"), "{text}");
    assert!(board.board.wait().unwrap().success());
}

#[test]
fn dry_run_does_not_stop_a_board_or_delete_any_data() {
    let home = Home::new();
    let board = home.board();
    let before = fs::read(home.list()).unwrap();

    let output = run(&mut home.command(&["uninstall", "--dry-run", "--delete-data"]));

    assert!(output.contains("Would stop board."), "{output}");
    assert!(output.contains("Will delete"), "{output}");
    assert!(board.status()["run"].is_string());
    assert!(board.lead().is_file());
    assert_eq!(fs::read(home.list()).unwrap(), before);
}

#[test]
fn uninstall_keeps_board_data_by_default() {
    let home = Home::new();
    let mut board = home.board();

    let output = run(&mut home.command(&["uninstall", "--yes"]));

    assert!(output.contains("Data kept."), "{output}");
    assert!(
        output.contains("malak uninstall --yes --delete-data"),
        "{output}"
    );
    assert!(
        output.contains(board.lead().parent().unwrap().to_str().unwrap()),
        "{output}"
    );
    assert!(board.board.wait().unwrap().success());
    assert!(board.lead().is_file());
    assert!(board.config.is_file());
    assert!(home.list().is_file());
}

#[test]
fn the_uninstall_inventory_counts_workers_and_demo_workspaces() {
    let home = Home::new();
    let board = home.board();
    let state = board.lead().parent().unwrap().to_path_buf();
    for index in 0..6 {
        let folder = state.join(format!("worker.{index:08}"));
        fs::create_dir(&folder).unwrap();
        fs::set_permissions(&folder, Permissions::from_mode(0o700)).unwrap();
        run(board
            .owner_command(&["enrol", &format!("worker-{index}"), "--out"])
            .arg(folder.join("handover")));
    }
    fs::create_dir(state.join("demo.abcd1234")).unwrap();

    let output = run(&mut home.command(&["uninstall", "--dry-run"]));

    assert!(
        output.contains("6 worker handovers, 1 demo workspace"),
        "{output}"
    );
    assert!(!output.contains("worker.00000000"), "{output}");
    assert!(!output.contains("demo.abcd1234"), "{output}");
}

#[test]
fn verbose_uninstall_lists_nested_handovers_with_custom_names() {
    let home = Home::new();
    let board = home.board();
    let folder = board.socket.path().join("st/handovers");
    fs::create_dir(&folder).unwrap();
    let handover = folder.join("worker.key");
    run(board
        .owner_command(&["enrol", "worker", "--out"])
        .arg(&handover));

    let output = run(&mut home.command(&["uninstall", "--dry-run", "--verbose"]));

    assert!(output.contains(handover.to_str().unwrap()), "{output}");
}

fn answer(home: &Home, uninstall: &[u8], data: Option<&[u8]>) -> String {
    let (master, slave) = terminal_pair(25, 160);
    let mut child = home
        .command(&["uninstall"])
        .stdin(slave.try_clone().unwrap())
        .stdout(slave)
        .spawn()
        .unwrap();
    let mut terminal = TerminalOutput::new(master);
    terminal.wait_for("Uninstall Malak? [y/N]");
    terminal.press(uninstall);
    if let Some(data) = data {
        terminal.wait_for("Also delete board data? [y/N]");
        terminal.press(data);
        terminal.wait_for("Remove the Malak binary by hand.");
    } else {
        terminal.wait_for("Uninstall cancelled.");
    }
    assert!(child.wait().unwrap().success());
    String::from_utf8_lossy(&terminal.bytes).into_owned()
}

#[test]
fn uninstall_deletes_data_after_a_terminal_yes() {
    let home = Home::new();
    let board = home.board();

    let output = answer(&home, b"y\n", Some(b"y\n"));

    assert!(output.contains("Deleted."), "{output}");
    assert!(!board.config.exists());
    assert!(!board.socket.path().join("st").exists());
    assert!(!board.socket.path().join("d").exists());
    assert!(!home.list().exists());
}

#[test]
fn uninstall_cancels_when_a_terminal_user_declines() {
    let home = Home::new();
    let board = home.board();

    let output = answer(&home, b"N\n", None);

    assert!(output.contains("Uninstall cancelled."), "{output}");
    assert!(!output.contains("Also delete board data?"), "{output}");
    assert!(board.config.is_file());
    assert!(board.status()["run"].is_string());
}

#[test]
fn declining_data_deletion_still_uninstalls_and_keeps_data() {
    let home = Home::new();
    let mut board = home.board();

    let output = answer(&home, b"y\n", Some(b"N\n"));

    assert!(output.contains("Data kept."), "{output}");
    assert!(board.config.is_file());
    assert!(board.lead().is_file());
    assert!(board.board.wait().unwrap().success());
}

#[test]
fn unattended_uninstall_requires_yes_before_stopping_the_board() {
    let home = Home::new();
    let board = home.board();

    let output = home.command(&["uninstall"]).output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --yes"));
    assert!(board.status()["run"].is_string());
}

#[test]
fn unattended_confirmed_deletion_keeps_external_handovers() {
    let home = Home::new();
    let board = home.board();
    let (_, handover) = board.enrol("worker");

    let output = run(home
        .command(&["uninstall", "--delete-data", "--yes"])
        .env("MALAK_HANDOVER", &handover));

    let (_, kept) = output.split_once("Will keep\n").expect(&output);
    assert!(
        kept.lines().any(|line| line.trim()
            == format!("{} (handover outside board folders)", handover.display())),
        "{output}"
    );
    assert!(handover.is_file());
    assert!(!board.config.exists());
}

#[test]
fn uninstall_keeps_unmanaged_entries_in_the_socket_tree() {
    let home = Home::new();
    let board = home.board();
    let sockets = board.socket.path().join("d");
    let foreign = sockets.join("workspace.txt");
    let door_foreign = sockets.join("o/notes.txt");
    fs::write(&foreign, "workspace").unwrap();
    fs::write(&door_foreign, "notes").unwrap();

    let output = run(&mut home.command(&["uninstall", "--delete-data", "--yes"]));

    assert_kept(&output, &foreign);
    assert_kept(&output, &door_foreign);
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "workspace");
    assert_eq!(fs::read_to_string(&door_foreign).unwrap(), "notes");
    assert!(!sockets.join("o/version").exists());
    assert!(!sockets.join("l").exists());
    assert!(!sockets.join("p").exists());
}

#[test]
fn uninstall_keeps_unmanaged_entries_in_the_state_tree() {
    let home = Home::new();
    let board = home.board();
    call(&board.lead(), &["send", "lead", "A record to remove"]);
    let state = board.socket.path().join("st");
    let foreign = state.join("workspace.txt");
    let store_foreign = state.join("messages/notes.md");
    for path in [&foreign, &store_foreign] {
        fs::write(path, "keep").unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o600)).unwrap();
    }

    let output = run(&mut home.command(&["uninstall", "--delete-data", "--yes"]));

    for path in [&foreign, &store_foreign] {
        assert_kept(&output, path);
        assert_eq!(fs::read_to_string(path).unwrap(), "keep");
    }
    assert!(!state.join("owner.credential").exists());
    assert!(!board.lead().exists());
    assert_eq!(fs::read_dir(state.join("messages")).unwrap().count(), 1);
}

#[test]
fn the_deletion_preview_lists_unmanaged_entries_as_kept() {
    let home = Home::new();
    let board = home.board();
    let state_file = board.socket.path().join("st/notes");
    let socket_file = board.socket.path().join("d/notes");
    for path in [&state_file, &socket_file] {
        fs::write(path, "keep").unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o600)).unwrap();
    }

    let output = run(&mut home.command(&["uninstall", "--delete-data", "--dry-run"]));

    let (deleted, kept) = output.split_once("Will keep\n").expect(&output);
    let actual: Vec<_> = kept
        .lines()
        .take_while(|line| line.starts_with("  "))
        .map(str::trim)
        .collect();
    assert_eq!(
        actual,
        [
            format!(
                "{} (directory contains other entries)",
                socket_file.parent().unwrap().display()
            ),
            format!("{} (outside Malak's managed data)", socket_file.display()),
            format!(
                "{} (directory contains other entries)",
                state_file.parent().unwrap().display()
            ),
            format!("{} (outside Malak's managed data)", state_file.display()),
        ],
        "{output}"
    );
    for path in [state_file, socket_file] {
        assert!(!deleted.contains(path.to_str().unwrap()), "{deleted}");
        assert!(path.is_file());
    }
    assert!(board.lead().is_file());
    assert!(board.status()["run"].is_string());
}

fn assert_kept(output: &str, path: &Path) {
    let (_, kept) = output.split_once("Left these paths.").expect(output);
    assert!(kept.contains(path.to_str().unwrap()), "{kept}");
    assert!(kept.contains("outside Malak's managed data"), "{kept}");
}

#[test]
fn uninstall_refuses_to_delete_a_symlink_target() {
    let home = Home::new();
    let board = home.board();
    let outside = home.path().join("keep");
    fs::write(&outside, "outside").unwrap();
    symlink(&outside, board.socket.path().join("st/snapshot.pending")).unwrap();

    let output = home
        .command(&["uninstall", "--delete-data", "--yes"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("symbolic link"));
    assert!(board.config.is_file());
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
}

#[test]
fn an_unstarted_config_does_not_authorize_removing_an_unrelated_folder() {
    let home = Home::new();
    let folder = home.path().join(".malak/unstarted");
    fs::create_dir_all(&folder).unwrap();
    write_config(&folder, "markdown");
    let unrelated = folder.join("st");
    fs::create_dir(&unrelated).unwrap();
    fs::set_permissions(&unrelated, Permissions::from_mode(0o700)).unwrap();
    let file = unrelated.join("important");
    fs::write(&file, "keep").unwrap();

    let output = home
        .command(&["uninstall", "--delete-data", "--yes"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot verify board data"));
    assert_eq!(fs::read_to_string(file).unwrap(), "keep");
}

struct CargoInstall {
    home: Home,
    tools: PathBuf,
    exe: PathBuf,
    called: PathBuf,
}

impl CargoInstall {
    fn new() -> Self {
        let home = Home::new();
        let bin = home.path().join("cargo-home/bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("malak");
        // Keep the writable descriptor out of the test process so a concurrent
        // fork cannot inherit it and briefly make this executable busy.
        run(Command::new("/bin/cp").arg(MALAK).arg(&exe));
        let tools = home.path().join("tools");
        fs::create_dir(&tools).unwrap();
        let called = home.path().join("called");
        let cargo = tools.join("cargo");
        fs::write(
            &cargo,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/called\"\n/bin/rm -- \"$CARGO_INSTALL_ROOT/bin/malak\"\n",
        )
        .unwrap();
        fs::set_permissions(&cargo, Permissions::from_mode(0o700)).unwrap();
        Self {
            home,
            tools,
            exe,
            called,
        }
    }

    fn command(&self, exe: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(exe);
        self.home.environment(&mut command);
        command.env("PATH", &self.tools).args(args);
        command
    }

    fn move_cargo_to_install_root(&self) -> PathBuf {
        let proxy = self.tools.join("rustup");
        fs::rename(self.tools.join("cargo"), &proxy).unwrap();
        symlink(&proxy, self.exe.with_file_name("cargo")).unwrap();
        proxy
    }
}

#[test]
fn a_cargo_install_calls_cargo_uninstall_malak() {
    let fixture = CargoInstall::new();

    run(&mut fixture.command(&fixture.exe, &["uninstall", "--yes"]));

    assert_eq!(
        fs::read_to_string(fixture.called).unwrap(),
        "uninstall\nmalak\n"
    );
}

#[test]
fn uninstall_uses_the_installed_cargo_proxy_when_path_has_no_cargo() {
    let fixture = CargoInstall::new();
    fixture.move_cargo_to_install_root();

    run(&mut fixture.command(&fixture.exe, &["uninstall", "--yes"]));

    assert_eq!(
        fs::read_to_string(fixture.called).unwrap(),
        "uninstall\nmalak\n"
    );
    assert!(!fixture.exe.exists());
}

#[test]
fn uninstall_removes_path_symlinks_to_the_removed_binary() {
    let fixture = CargoInstall::new();
    let proxy = fixture.move_cargo_to_install_root();
    let alias = fixture.tools.join("malak");
    symlink("../cargo-home/bin/malak", &alias).unwrap();
    let unrelated = fixture.tools.join("another-tool");
    symlink(&proxy, &unrelated).unwrap();

    run(&mut fixture.command(&alias, &["uninstall", "--yes"]));

    assert!(!fixture.exe.exists());
    assert!(!alias.is_symlink());
    assert!(unrelated.is_symlink());
}

#[test]
fn uninstall_prints_a_removal_command_for_a_broken_path_symlink_it_cannot_unlink() {
    let fixture = CargoInstall::new();
    let alias = fixture.tools.join("malak");
    symlink(&fixture.exe, &alias).unwrap();
    fs::set_permissions(&fixture.tools, Permissions::from_mode(0o777)).unwrap();

    let output = run(&mut fixture.command(&fixture.exe, &["uninstall", "--yes"]));

    assert!(alias.is_symlink());
    assert!(!alias.exists());
    assert!(output.contains("broken Malak PATH symlinks"), "{output}");
    assert_eq!(
        output.lines().last().unwrap(),
        format!("rm -- '{}'", alias.display())
    );
}

#[test]
fn uninstall_manual_removal_command_is_last_and_shell_quoted() {
    let fixture = CargoInstall::new();
    let manual = fixture.home.path().join("owner's malak");
    fs::copy(MALAK, &manual).unwrap();
    let alias = fixture.tools.join("malak");
    symlink(&manual, &alias).unwrap();

    let output = run(&mut fixture.command(&manual, &["uninstall", "--yes"]));

    assert!(output.contains("will be broken"), "{output}");
    let command = output.lines().last().unwrap();
    assert!(command.starts_with("rm -- "), "{output}");
    assert!(
        Command::new("/bin/sh")
            .args(["-c", command])
            .status()
            .unwrap()
            .success()
    );
    assert!(!manual.exists());
    assert!(!alias.is_symlink());
}

#[test]
fn a_standalone_binary_is_listed_for_manual_removal() {
    let fixture = CargoInstall::new();

    let output = run(&mut fixture.command(Path::new(MALAK), &["uninstall", "--yes"]));

    assert!(
        output.contains("Remove the Malak binary by hand."),
        "{output}"
    );
    assert!(!fixture.called.exists());
}

#[test]
fn uninstall_deletes_legacy_worker_results() {
    let home = Home::new();
    let board = home.board();
    let state = board.lead().parent().unwrap().to_path_buf();
    let worker = state.join("worker.A1b2c3D4");
    let result = worker.join("result.E5f6g7H8");
    for folder in [&worker, &result] {
        fs::create_dir(folder).unwrap();
        fs::set_permissions(folder, Permissions::from_mode(0o700)).unwrap();
    }
    for file in [
        worker.join("brief"),
        result.join("out"),
        result.join("err"),
        result.join("exec-attempt"),
    ] {
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
    }
    let unrelated = state.join("worker.A1b2cD-4");
    fs::create_dir(&unrelated).unwrap();

    let output = run(&mut home.command(&["uninstall", "--delete-data", "--yes"]));

    assert!(!worker.exists(), "{output}");
    assert!(unrelated.is_dir(), "{output}");
}

#[test]
fn uninstall_deletes_empty_worker_folders() {
    let home = Home::new();
    let board = home.board();
    let empty = board.lead().parent().unwrap().join("worker.K1m2R3n4");
    fs::create_dir(&empty).unwrap();
    fs::set_permissions(&empty, Permissions::from_mode(0o700)).unwrap();

    let output = run(&mut home.command(&["uninstall", "--delete-data", "--yes"]));

    assert!(!empty.exists(), "{output}");
}

#[test]
fn a_dry_run_does_not_call_cargo_uninstall() {
    let fixture = CargoInstall::new();

    let output = run(&mut fixture.command(&fixture.exe, &["uninstall", "--dry-run"]));

    assert!(
        output.contains("Would run cargo uninstall malak."),
        "{output}"
    );
    assert!(!fixture.called.exists());
}
