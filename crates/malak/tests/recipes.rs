//! Exercise the shell recipes against the Cargo-built command and real boards.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

#[path = "recipes/adopt.rs"]
mod adopt;
#[path = "recipes/briefs.rs"]
mod briefs;
#[path = "recipes/org.rs"]
mod org;
#[path = "fixtures/retained.rs"]
mod retained;
#[path = "recipes/worker_await.rs"]
mod worker_await;

#[path = "fixtures/board_home.rs"]
mod board_home;
use board_home::BoardHome;

const MALAK: &str = env!("CARGO_BIN_EXE_malak");
const BOARD: &str = "malak";
const TEST_WATCHDOG: Duration = Duration::from_secs(180);
const MALAK_NUDGE: &str = "Malak is installed for visibility. Route work through the board.";
const MALAK_BLOCK_START: &str =
    "<!-- malak:begin (managed by malak recipe agents-md; remove with --remove) -->";
const MALAK_BLOCK_END: &str = "<!-- malak:end -->";

struct Fixture {
    home: Rc<BoardHome>,
    config: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let board = Self::in_home(Rc::new(BoardHome::new()), BOARD, "st ' quoted", "d");
        // This config also occupies the fixture's default XDG path.
        board.configure(&format!("name = {BOARD:?}"));
        board
    }

    fn sibling(&self, name: &str, sockets: &str) -> Self {
        Self::in_home(
            Rc::clone(&self.home),
            name,
            &format!("state-{name}"),
            sockets,
        )
    }

    fn in_home(home: Rc<BoardHome>, name: &str, state: &str, sockets: &str) -> Self {
        let state = home.path().join(state);
        let config = Self::write_config(home.path(), name, &state, sockets);
        Self {
            home,
            config,
            state,
        }
    }

    fn write_config(root: &Path, board: &str, state: &Path, sockets: &str) -> PathBuf {
        let directory = root.join(".malak").join(board);
        fs::create_dir_all(&directory).unwrap();
        let config = directory.join("board.toml");
        fs::write(
            &config,
            format!(
                "[board]\nstate = {state:?}\nsocket_folder = {:?}\n",
                root.join(sockets)
            ),
        )
        .unwrap();
        config
    }

    fn recipe(&self, name: &str) -> Command {
        let mut command = Command::new(MALAK);
        command.args(["recipe", name]).current_dir(self.home.path());
        self.recipe_environment(&mut command);
        command
    }

    fn recipe_environment(&self, command: &mut Command) {
        let malak = Path::new(MALAK).parent().unwrap().to_path_buf();
        let path = env::var_os("PATH").unwrap();
        let path = env::join_paths(std::iter::once(malak).chain(env::split_paths(&path)));
        command
            .env("PATH", path.unwrap())
            .env("HOME", self.home.path())
            .env("TMPDIR", self.home.path())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("CODEX_SESSION_ID")
            .env_remove("CODEX_THREAD_ID")
            .env("XDG_CONFIG_HOME", self.home.path().join(".malak"))
            .env("MALAK_HANDOVER", self.state.join("lead.handover"));
    }

    fn owner(&self, words: &[&str]) -> Command {
        let mut command = Command::new(MALAK);
        command
            .arg("--config")
            .arg(&self.config)
            .env("HOME", self.home.path())
            .args(words);
        command
    }

    fn status(&self) -> Value {
        serde_json::from_str(&run(self.owner(&["board", "status", "--json"]))).unwrap()
    }

    fn lead(&self, words: &[&str]) -> String {
        run(participant_command(
            &self.state.join("lead.handover"),
            words,
        ))
    }

    fn enrol(&self, name: &str, grants: &[&str]) -> PathBuf {
        let handover = self.home.path().join(name);
        let mut command = self.owner(&["enrol", name, "--out"]);
        command.arg(&handover);
        for grant in grants {
            command.args(["--grant", grant]);
        }
        run(command);
        handover
    }

    fn configure(&self, settings: &str) {
        let mut config = fs::OpenOptions::new()
            .append(true)
            .open(&self.config)
            .unwrap();
        writeln!(config, "{settings}").unwrap();
    }

    fn start(&self) {
        // Most recipe fixtures deliberately reuse credentials across processes.
        // Tests that exercise lead-up run it explicitly.
        run(self.owner(&["board", "start", "--detach"]));
        run(self.owner(&["channel", "create", "selftest/lead"]));
        self.lead(&[
            "send",
            "--id",
            "lead-up-test-fixture",
            "selftest/lead",
            "Malak lead-up self-test",
        ]);
        self.lead(&["read", "--ack", "selftest/lead"]);
    }

    fn worker_start(&self, options: &[&str], worker_command: &[&str]) -> Command {
        let options = [&["--no-watcher"][..], options].concat();
        self.worker_start_command(&options, worker_command)
    }

    fn worker_start_command(&self, options: &[&str], worker_command: &[&str]) -> Command {
        let mut command = self.recipe("worker-start");
        command.arg("--unbound");
        command
            .args(["alice", "--board", BOARD])
            .args(options)
            .arg("--")
            .args(worker_command);
        command
    }

    fn worker_id(&self) -> String {
        participant(&self.status(), "alice")["id"]
            .as_str()
            .unwrap()
            .into()
    }
}

fn output(mut command: Command) -> Output {
    command.output().unwrap()
}

fn recipe_python_startup(board: &Fixture, recipe: &mut Command, startup: &[u8]) {
    fs::write(board.home.path().join("sitecustomize.py"), startup).unwrap();
    recipe.env("PYTHONPATH", board.home.path());
}

fn terminate_worker_during_startup(during_detach: bool) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let board = Fixture::new();
    let mut recipe = board.recipe("worker-start");
    if during_detach {
        recipe.args(["worker", "--detach", "--", "true"]);
    } else {
        recipe.arg("--help");
    }
    recipe_python_startup(
        &board,
        &mut recipe,
        b"import os, pathlib, time\n\
          temporary = pathlib.Path(os.environ['TMPDIR'])\n\
          (temporary / 'pid').write_text(str(os.getpid()))\n\
          (temporary / 'ready').touch()\n\
          time.sleep(60)\n",
    );
    let mut parent = RunningWorker(recipe.stdout(Stdio::null()).spawn().unwrap());
    let directory = board.home.path();
    wait_for_event("the recipe interpreter to start", || {
        directory.join("ready").is_file().then_some(())
    });
    let child_pid = fs::read_to_string(directory.join("pid")).unwrap();
    let child = Pid::from_raw(child_pid.parse().unwrap());
    let pid = Pid::from_raw(parent.0.id() as i32);
    kill(pid, Signal::SIGTERM).unwrap();
    let status = wait_for_event("the terminated launcher to exit", || {
        parent.0.try_wait().unwrap()
    });
    let _ = kill(child, Signal::SIGKILL);
    assert!(!status.success());
    let leftovers: Vec<_> = directory
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("malak-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn terminated_foreground_recipe_removes_temporary_directories() {
    terminate_worker_during_startup(false);
}

#[test]
fn terminated_worker_detach_removes_temporary_directories_before_handoff() {
    terminate_worker_during_startup(true);
}

fn wait_for_event<T>(description: &str, mut observe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TEST_WATCHDOG;
    loop {
        if let Some(event) = observe() {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_file(board: &Fixture, name: &str) {
    let path = board.home.path().join(name);
    wait_for_event(name, || path.is_file().then_some(()));
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn run(command: Command) -> String {
    stdout(output(command))
}

fn participant_command(handover: &Path, words: &[&str]) -> Command {
    let mut command = Command::new(MALAK);
    command
        .args(words)
        .env("MALAK_HANDOVER", handover)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_SESSION_ID")
        .env_remove("CODEX_THREAD_ID");
    command
}

fn participant<'a>(status: &'a Value, name: &str) -> &'a Value {
    status["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|person| person["name"] == name)
        .unwrap()
}

fn messages(output: &str) -> Vec<Value> {
    output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|record| record.get("channels").is_none())
        .collect()
}

fn body(message: &Value) -> Vec<u8> {
    serde_json::from_value(message["body"].clone()).unwrap()
}

#[test]
fn recipe_lists_one_shipped_name_per_line() {
    let names = run({
        let mut command = Command::new(MALAK);
        command.arg("recipe");
        command
    });
    assert!(names.lines().all(|name| {
        name.bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
    }));
    assert!(names.lines().any(|name| name == "worker-start"));
}

#[test]
fn recipe_refuses_a_path_in_place_of_a_name() {
    let board = Fixture::new();
    let result = output(board.recipe("../../bin/sh"));
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("unknown shipped recipe"));
}

#[test]
fn recipe_refuses_when_no_trusted_temporary_base_is_available() {
    let board = Fixture::new();
    let shared = board.home.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let mut recipe = board.recipe("ask");
    recipe
        .arg("--help")
        .env("TMPDIR", &shared)
        .env("HOME", &shared);

    let result = output(recipe);

    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("no trusted writable temporary base"),
        "{result:?}"
    );
    assert_eq!(fs::read_dir(shared).unwrap().count(), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn recipe_checks_permissions_on_all_resolved_temporary_ancestors() {
    let board = Fixture::new();
    let ancestor = board.home.path().join("ancestor");
    let base = ancestor.join("base");
    fs::create_dir_all(&base).unwrap();
    let mut acl = Command::new("/bin/chmod");
    acl.args(["+a", "everyone allow writesecurity"])
        .arg(&ancestor);
    run(acl);
    let mut recipe = board.recipe("ask");
    recipe.arg("--help").env("TMPDIR", &base).env("HOME", &base);

    let result = output(recipe);

    assert_eq!(result.status.code(), Some(2));
    assert_eq!(fs::read_dir(base).unwrap().count(), 0);
}

#[cfg(target_os = "macos")]
#[test]
fn recipe_refuses_effective_access_inherited_by_the_fresh_parent() {
    let board = Fixture::new();
    let base = board.home.path().join("base");
    fs::create_dir(&base).unwrap();
    let mut acl = Command::new("/bin/chmod");
    acl.args(["+a", "everyone allow list,directory_inherit,only_inherit"])
        .arg(&base);
    run(acl);
    let mut recipe = board.recipe("ask");
    recipe.arg("--help").env("TMPDIR", &base).env("HOME", &base);

    let result = output(recipe);

    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("extended ACL grants access"));
    assert!(result.stdout.is_empty());
    assert_eq!(fs::read_dir(base).unwrap().count(), 0);
}

#[test]
fn every_shipped_recipe_has_help_before_board_lookup() {
    let board = Fixture::new();
    let mut list = Command::new(MALAK);
    list.arg("recipe");
    let names = run(list);
    let mut missing = Vec::new();
    for name in names.lines() {
        let mut help = board.recipe(name);
        help.arg("--help");
        let result = output(help);
        let usage = String::from_utf8_lossy(&result.stdout);
        if !result.status.success() || !usage.to_ascii_lowercase().contains("usage:") {
            missing.push(name);
        }
    }
    assert!(missing.is_empty(), "missing help for {missing:?}");
}

#[test]
fn agents_md_creates_matching_instruction_files_in_the_current_project() {
    let board = Fixture::new();
    let mut recipe = board.recipe("agents-md");
    recipe.env_remove("MALAK_HANDOVER");
    run(recipe);

    let agents = fs::read_to_string(board.home.path().join("AGENTS.md")).unwrap();
    let claude = fs::read_to_string(board.home.path().join("CLAUDE.md")).unwrap();
    assert_eq!(agents, claude);
    for text in [
        MALAK_BLOCK_START,
        MALAK_BLOCK_END,
        MALAK_NUDGE,
        "Board `\"malak\"`",
        "`lead`",
        "`questions`",
        "`orders`",
        "`relay/<name>`",
        "`to/<id>`",
    ] {
        assert!(agents.contains(text), "missing {text} in {agents}");
    }
}

#[test]
fn agents_md_names_the_lead_monitor() {
    let board = Fixture::new();
    let mut recipe = board.recipe("agents-md");
    recipe.env_remove("MALAK_HANDOVER");
    run(recipe);

    let agents = fs::read_to_string(board.home.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("`malak mailbox --plain --for lead --next`"));
}

#[test]
fn agents_md_does_not_rewrite_an_identical_block() {
    use std::os::unix::fs::MetadataExt;

    let board = Fixture::new();
    let recipe = || {
        let mut command = board.recipe("agents-md");
        command.env_remove("MALAK_HANDOVER");
        command
    };
    run(recipe());
    let files = ["AGENTS.md", "CLAUDE.md"].map(|name| board.home.path().join(name));
    let before = files
        .each_ref()
        .map(|path| (fs::read(path).unwrap(), fs::metadata(path).unwrap().ino()));

    run(recipe());

    for (path, (contents, inode)) in files.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), contents);
        assert_eq!(fs::metadata(path).unwrap().ino(), inode);
    }
}

#[test]
fn agents_md_updates_a_managed_block_and_preserves_existing_bytes() {
    let board = Fixture::in_home(Rc::new(BoardHome::new()), BOARD, "state", "d");
    board.configure("name = \"research\"");
    let project = board.home.path().join("separate project");
    fs::create_dir(&project).unwrap();
    let agents = project.join("AGENTS.md");
    let prefix = "# Existing agent instructions\r\n\r\nLeave these.  \r\n";
    let suffix = "\r\n# More project instructions\r\nLeave these too.";
    fs::write(
        &agents,
        format!("{prefix}{MALAK_BLOCK_START}\r\nold block\r\n{MALAK_BLOCK_END}\r\n{suffix}"),
    )
    .unwrap();
    let claude = project.join("CLAUDE.md");
    let existing_claude = b"# Existing Claude instructions\nNo final newline";
    fs::write(&claude, existing_claude).unwrap();
    let mut recipe = board.recipe("agents-md");
    recipe
        .env_remove("MALAK_HANDOVER")
        .arg("--dir")
        .arg(&project)
        .arg("--config")
        .arg(&board.config);

    run(recipe);

    let agents = fs::read_to_string(agents).unwrap();
    assert!(agents.starts_with(prefix));
    assert!(agents.ends_with(suffix));
    assert!(agents.contains("Board `\"research\"`.\r\n"));
    assert!(!agents.contains("old block"));
    let claude = fs::read_to_string(claude).unwrap();
    assert!(claude.starts_with(MALAK_BLOCK_START));
    assert!(claude.as_bytes().ends_with(existing_claude));
    assert!(claude.contains("Board `\"research\"`.\n"));
}

#[test]
fn agents_md_restores_original_bytes_after_prepend_and_remove() {
    let board = Fixture::new();
    let agents = board.home.path().join("AGENTS.md");
    let claude = board.home.path().join("CLAUDE.md");
    let front_matter = b"---\r\ntitle: Existing project\r\n---\r\n\r\nKeep this too.  ";
    let existing_claude = b"# Use the existing instructions\n\n";
    fs::write(&agents, front_matter).unwrap();
    fs::write(&claude, existing_claude).unwrap();
    let mut recipe = board.recipe("agents-md");
    recipe.env_remove("MALAK_HANDOVER");

    run(recipe);

    for (path, original) in [
        (&agents, front_matter.as_slice()),
        (&claude, existing_claude),
    ] {
        let contents = fs::read(path).unwrap();
        assert!(contents.starts_with(MALAK_BLOCK_START.as_bytes()));
        assert!(contents.ends_with(original));
    }

    let mut remove = board.recipe("agents-md");
    remove.arg("--remove");
    run(remove);

    assert_eq!(fs::read(agents).unwrap(), front_matter);
    assert_eq!(fs::read(claude).unwrap(), existing_claude);
}

#[test]
fn agents_md_remove_deletes_a_created_file_and_preserves_an_original_empty_file() {
    let board = Fixture::new();
    let agents = board.home.path().join("AGENTS.md");
    let claude = board.home.path().join("CLAUDE.md");
    fs::write(&claude, b"").unwrap();
    let mut install = board.recipe("agents-md");
    install.env_remove("MALAK_HANDOVER");
    run(install);

    let mut remove = board.recipe("agents-md");
    remove.arg("--remove");
    run(remove);

    assert!(!agents.exists());
    assert!(fs::read(claude).unwrap().is_empty());
}

#[test]
fn agents_md_names_the_inbox_from_the_selected_boards_handover() {
    let board = Fixture::new();
    board.start();
    let handover = board.enrol("alice", &["lead:send"]);
    let id = board.worker_id();
    let mut recipe = board.recipe("agents-md");
    recipe
        .args(["--board", BOARD])
        .env("MALAK_HANDOVER", &handover);

    run(recipe);

    for file in ["AGENTS.md", "CLAUDE.md"] {
        let contents = fs::read_to_string(board.home.path().join(file)).unwrap();
        assert!(contents.contains(&format!("Your inbox from `MALAK_HANDOVER` is `to/{id}`.")));
        assert!(!contents.contains(handover.to_str().unwrap()));
    }
}

#[test]
fn agents_md_uses_a_handover_without_a_board_config() {
    let board = Fixture::new();
    board.start();
    let saved = board.config.with_extension("saved");
    fs::rename(&board.config, &saved).unwrap();

    let result = output(board.recipe("agents-md"));

    fs::rename(saved, &board.config).unwrap();
    stdout(result);
    let contents = fs::read_to_string(board.home.path().join("AGENTS.md")).unwrap();
    assert!(contents.contains("Board `\"~/d\"`."));
    assert!(contents.contains("Your inbox from `MALAK_HANDOVER` is `lead`."));
}

#[test]
fn agents_md_rejects_a_handover_for_another_board_before_writing() {
    let board = Fixture::new();
    let other = board.sibling("other", "e");
    board.start();
    let mut recipe = board.recipe("agents-md");
    recipe.arg("--config").arg(&other.config);

    let result = output(recipe);

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not match the selected board"));
    assert!(!board.home.path().join("AGENTS.md").exists());
    assert!(!board.home.path().join("CLAUDE.md").exists());
}

#[test]
fn agents_md_refuses_incomplete_markers_before_replacing_either_file() {
    let board = Fixture::new();
    let claude = board.home.path().join("CLAUDE.md");
    fs::write(&claude, MALAK_BLOCK_START).unwrap();
    let mut recipe = board.recipe("agents-md");
    recipe.env_remove("MALAK_HANDOVER");

    let result = output(recipe);

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("incomplete or repeated"));
    assert!(!board.home.path().join("AGENTS.md").exists());
    assert_eq!(fs::read_to_string(claude).unwrap(), MALAK_BLOCK_START);
}

#[test]
fn unknown_recipe_lists_the_shipped_names() {
    let board = Fixture::new();
    let mut list = Command::new(MALAK);
    list.arg("recipe");
    let names = run(list);
    let rejected = output(board.recipe("not-shipped"));
    let diagnostic = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        names.lines().all(|name| diagnostic.contains(name)),
        "{diagnostic}"
    );
}

#[test]
fn a_renamed_cli_runs_its_recipe_with_the_same_executable() {
    let board = Fixture::new();
    board.start();
    let running = board.home.path().join("running ' malak");
    fs::copy(MALAK, &running).unwrap();
    let decoy = board.home.path().join("malak");
    fs::write(&decoy, "#!/bin/sh\nexit 77\n").unwrap();
    fs::set_permissions(decoy, fs::Permissions::from_mode(0o700)).unwrap();

    let mut command = Command::new(running);
    board.recipe_environment(&mut command);
    command
        .args(["recipe", "lead-up", "--config"])
        .arg(&board.config);

    assert!(run(command).starts_with("export MALAK_HANDOVER="));
}

#[test]
fn demo_refuses_to_start_when_codex_hook_trust_is_declined() {
    let board = Fixture::new();
    board.start();
    let codex = board.home.path().join("codex");
    fs::write(&codex, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(codex, fs::Permissions::from_mode(0o700)).unwrap();
    let path = env::var_os("PATH").unwrap();
    let path = env::join_paths(
        std::iter::once(board.home.path().to_owned()).chain(env::split_paths(&path)),
    )
    .unwrap();

    let mut command = board.recipe("demo");
    command
        .args(["codex", "Name five plants"])
        .env("PATH", path)
        .stdin(Stdio::null());
    let result = output(command);

    assert_eq!(result.status.code(), Some(2));
    let error = String::from_utf8(result.stderr).unwrap();
    assert_eq!(error.matches("malak hook --tool codex").count(), 4);
    assert!(error.contains("trust these for this session? y/N"));
    assert!(result.stdout.is_empty());
}

#[test]
fn demo_returns_an_immediate_lead_failure() {
    let board = Fixture::new();
    let mut demo = demo_with_claude(&board, "exit 37");
    demo.arg("Fixture goal")
        .env("MALAK_TEST_DEMO_WAIT_FOR_LEAD_EXIT", "1");

    let result = output(demo);

    assert_eq!(result.status.code(), Some(37), "{result:?}");
    assert!(
        !String::from_utf8_lossy(&result.stdout).contains("workers started"),
        "{result:?}"
    );
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("Demo lead exited during startup."),
        "{error}"
    );
    assert!(error.contains("Lead exit  37"), "{error}");
    assert_demo_removed(&board, &demo_name(&String::from_utf8_lossy(&result.stdout)));
}

#[test]
fn demo_shows_terminal_progress_while_the_lead_is_writing_briefs() {
    let board = Fixture::new();
    board.start();
    let (events, address) = demo_events(&board);
    let (master, slave) = terminal::terminal_pair(32, 160);
    let mut screen = terminal::TerminalOutput::new(master);
    let mut demo = RunningWorker(
        demo_fixture(&board, "claude", "progress")
            .env("MALAK_TEST_DEMO_EVENTS", &address)
            .env("TERM", "xterm")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(slave)
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_eq!(next_demo_event(&events), "lead");
    let board_folder = demo_folder(&board);
    let name = board_folder.file_name().unwrap().to_string_lossy();
    let mut expected = format!(
        "The demo runs on its own board, {name}.\n\
         Open env -u MALAK_HANDOVER malak --config {} mailbox in another terminal to watch the demo.\n\
         Starting claude lead.\n\
         Waiting for 5 briefs.\n",
        board_folder.join("board.toml").display()
    );
    screen.wait_for_output(&expected, |bytes| {
        String::from_utf8_lossy(bytes).replace('\r', "") == expected
    });

    let gate = address.with_file_name("demo-gate");
    for number in 1..=5 {
        events.send_to(b"1", &gate).unwrap();
        expected.push_str(&format!("Found task-{number}.md.\n"));
        screen.wait_for_output(&expected, |bytes| {
            String::from_utf8_lossy(bytes).replace('\r', "") == expected
        });
    }
    events.send_to(b"1", &gate).unwrap();
    screen.wait_for("Demo lead and five workers started with claude.");
    assert!(demo.0.wait().unwrap().success());

    let stdout = String::from_utf8_lossy(&screen.bytes).replace('\r', "");
    let mut lines = stdout.strip_prefix(&expected).unwrap().lines();
    for number in 1..=5 {
        let line = lines.next().unwrap();
        assert!(line.starts_with("Starting worker demo-"), "{line}");
        assert!(line.ends_with(&format!("-{number}.")), "{line}");
    }
    assert_eq!(
        lines.next(),
        Some("Demo lead and five workers started with claude.")
    );
}

#[test]
fn demo_refuses_a_board_selector_before_resolving_it() {
    let board = Fixture::new();
    let mut leading = Command::new(MALAK);
    leading.args(["--board", "missing", "recipe", "demo", "claude", "Goal"]);
    board.recipe_environment(&mut leading);
    let mut recipe_option = board.recipe("demo");
    recipe_option.args(["--board", "missing", "claude", "Goal"]);
    let mut trailing = board.recipe("demo");
    trailing.args(["claude", "Goal", "--board=missing"]);
    for command in [leading, recipe_option, trailing] {
        let result = output(command);
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        assert!(result.stdout.is_empty(), "{result:?}");
        assert_eq!(
            String::from_utf8_lossy(&result.stderr),
            "The demo runs on its own board. Omit --board and --config.\n"
        );
    }
}

#[test]
fn demo_finishes_and_removes_the_board_and_its_registration() {
    let board = Fixture::new();
    let stdout = run(demo_fixture(&board, "claude", "success"));
    assert!(stdout.contains("Fixture demo summary"), "{stdout}");
    assert!(stdout.contains("5 of 5 workers reported"), "{stdout}");
    assert_demo_removed(&board, &demo_name(&stdout));
}

#[test]
fn demo_fails_when_all_workers_fail_and_the_lead_exits_zero() {
    assert_demo_worker_outcome("9 9 9 9 9", 0, 0, 3);
}

#[test]
fn demo_counts_only_successful_workers_and_fails_if_any_failed() {
    assert_demo_worker_outcome("0 0 9 0 9", 3, 0, 3);
}

#[test]
fn demo_succeeds_when_all_workers_and_the_lead_succeed() {
    assert_demo_worker_outcome("0 0 0 0 0", 5, 0, 0);
}

#[test]
fn demo_revokes_its_workers_and_retains_their_reports() {
    let board = Fixture::new();
    run(kept_demo_fixture(&board, "claude", "success"));

    assert_demo_workers_revoked(&board, 5);
    let status = demo_status(&board);
    let reports = messages(&demo_lead(&board, &["read", "lead", "--json"]));
    let workers = demo_workers(&status);
    assert_eq!(reports.len(), 5);
    for worker in workers {
        assert!(
            reports
                .iter()
                .any(|report| report["sender"] == worker["id"])
        );
    }
}

#[test]
fn demo_does_not_touch_the_existing_bound_board() {
    let board = Fixture::new();
    board.start();
    let mut bind = board.owner(&["recipe", "lead-up"]);
    board.recipe_environment(&mut bind);
    run(bind);
    let notices = messages(&board.lead(&["read", "--json", "--wait", "5", "lead"]));
    assert!(notices.iter().any(|message| {
        message["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("claim-lead-") && id.ends_with("-accepted"))
    }));
    let handover = board.state.join("lead.handover");
    let before = fs::read(&handover).unwrap();
    let status = board.status();
    let claim = &participant(&status, "lead")["claim"];
    assert_eq!(claim["state"], "bound");
    let registry = board_registry(&board);

    run(demo_fixture(&board, "claude", "success"));

    let current = board.status();
    assert_eq!(&participant(&current, "lead")["claim"], claim);
    assert_eq!(current["participants"], status["participants"]);
    assert_eq!(current["channels"], status["channels"]);
    assert_eq!(fs::read(handover).unwrap(), before);
    assert_eq!(board_registry(&board), registry);
}

#[test]
fn consecutive_demos_use_different_boards() {
    let board = Fixture::new();
    let mut command = demo_fixture(&board, "claude", "success");
    let first = demo_name(&stdout(command.output().unwrap()));
    let second = demo_name(&stdout(command.output().unwrap()));
    assert_ne!(first, second);
}

#[test]
fn demo_keep_preserves_the_board_and_outcome() {
    let board = Fixture::new();
    let stdout = run(kept_demo_fixture(&board, "claude", "success"));
    let name = demo_name(&stdout);
    let folder = demo_folder(&board);
    assert!(folder.join("state/lead.handover").is_file());
    assert!(board_registry(&board).contains(&name));
    assert!(demo_outcome(&board).contains("5 of 5 workers reported"));
    assert!(stdout.contains("board stop"), "{stdout}");
    assert!(
        stdout.contains(&format!("malak recipe demo --remove {name}")),
        "{stdout}"
    );
    assert_eq!(
        messages(&demo_lead(&board, &["read", "lead", "--json"])).len(),
        5
    );
}

#[test]
fn the_printed_demo_mailbox_command_ignores_the_callers_handover() {
    let board = Fixture::new();
    let stdout = run(kept_demo_fixture(&board, "claude", "success"));
    let mailbox = stdout
        .lines()
        .find_map(|line| {
            line.strip_prefix("Open ")?
                .strip_suffix(" in another terminal to watch the demo.")
        })
        .unwrap();
    let mut command = Command::new("sh");
    board.recipe_environment(&mut command);
    command.arg("-c").arg(format!("{mailbox} --who"));

    assert!(run(command).contains("lead"));
}

#[test]
fn a_kept_board_survives_later_demos_after_it_stops() {
    let board = Fixture::new();
    run(kept_demo_fixture(&board, "claude", "success"));
    let folder = demo_folder(&board);
    run(demo_owner(&board, &folder, &["board", "stop"]));

    run(demo_fixture(&board, "claude", "success"));

    assert_eq!(demo_folders(&board), [folder]);
}

#[test]
fn demo_remove_refuses_a_running_board() {
    let board = Fixture::new();
    let stdout = run(kept_demo_fixture(&board, "claude", "success"));
    let name = demo_name(&stdout);
    let mut remove = board.recipe("demo");
    remove.args(["--remove", &name]);

    let result = output(remove);

    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("first with"));
    assert!(demo_folder(&board).join("state/lead.handover").is_file());
}

#[test]
fn demo_remove_clears_a_stopped_kept_board() {
    let board = Fixture::new();
    let stdout = run(kept_demo_fixture(&board, "claude", "success"));
    let name = demo_name(&stdout);
    run(demo_owner(&board, &demo_folder(&board), &["board", "stop"]));
    let mut remove = board.recipe("demo");
    remove.args(["--remove", &name]);

    run(remove);

    assert_demo_removed(&board, &name);
}

#[test]
fn demo_reaps_the_stopped_board_from_a_crashed_run() {
    let board = Fixture::new();
    let (events, address) = demo_events(&board);
    let mut crashed = RunningWorker(
        demo_fixture(&board, "claude", "start-signal")
            .env("MALAK_TEST_DEMO_EVENTS", address)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_eq!(next_demo_event(&events), "lead");
    let folder = demo_folder(&board);
    let name = folder.file_name().unwrap().to_str().unwrap();
    let workspace = demo_workspace(&board);
    let host = fs::read_to_string(workspace.join("host.running")).unwrap();
    let lead = fs::read_to_string(workspace.join("lead.running")).unwrap();
    signal_demo_process("-KILL", &host);
    assert_eq!(crashed.0.wait().unwrap().code(), Some(137));
    signal_demo_process("-KILL", &lead);
    run(demo_owner(&board, &folder, &["board", "stop"]));
    assert!(board_registry(&board).contains(name));

    run(demo_fixture(&board, "claude", "success"));

    assert_demo_removed(&board, name);
}

#[test]
fn a_second_demo_does_not_reap_a_running_demo() {
    let board = Fixture::new();
    let (events, address) = demo_events(&board);
    let mut first = RunningWorker(
        demo_fixture(&board, "claude", "start-signal")
            .env("MALAK_TEST_DEMO_EVENTS", address)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_eq!(next_demo_event(&events), "lead");
    let folder = demo_folder(&board);

    run(demo_fixture(&board, "claude", "success"));

    assert_eq!(demo_folders(&board), [folder]);
    signal_demo_process("-INT", &first.0.id().to_string());
    assert_eq!(first.0.wait().unwrap().code(), Some(130));
}

#[test]
fn demo_never_reaps_an_unmarked_board_with_a_demo_name() {
    let board = Fixture::new();
    let other = board.sibling("demo-c0ffee123456", "e");
    other.start();
    run(other.owner(&["board", "stop"]));
    let handover = other.state.join("lead.handover");
    let before = fs::read(&handover).unwrap();
    let registry = board_registry(&board);

    run(demo_fixture(&board, "claude", "success"));

    assert!(other.config.is_file());
    assert_eq!(fs::read(handover).unwrap(), before);
    assert_eq!(board_registry(&board), registry);
}

#[test]
fn demo_timeout_stops_a_hung_worker_and_removes_the_board() {
    let board = Fixture::new();
    let (events, address) = demo_events(&board);
    let clock_trigger = board.home.path().join("advance-demo-clock");
    let demo = demo_fixture(&board, "claude", "signal")
        .args(["--timeout", "120"])
        .env("MALAK_TEST_DEMO_EVENTS", address)
        .env("MALAK_TEST_DEMO_CLOCK_TRIGGER", &clock_trigger)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(next_demo_event(&events), "lead");
    let worker = next_demo_event(&events);
    assert!(worker.starts_with("worker-"), "{worker}");
    let pid = fs::read_to_string(demo_workspace(&board).join(format!("{worker}.running"))).unwrap();
    fs::write(clock_trigger, "").unwrap();

    let result = demo.wait_with_output().unwrap();

    assert_eq!(result.status.code(), Some(3), "{result:?}");
    let stdout = String::from_utf8(result.stdout).unwrap();
    assert!(stdout.contains("Demo timed out. Rerun with --timeout SECONDS"));
    assert_demo_process_stopped(&pid);
    assert_demo_removed(&board, &demo_name(&stdout));
}

#[test]
fn demo_records_one_failed_revocation_and_continues_revoking_workers() {
    let board = Fixture::new();
    board.start();
    let mut demo = kept_demo_fixture(&board, "claude", "success");
    demo.env("MALAK_TEST_DEMO_REFUSE_REVOKE", "1");
    run(demo);

    let outcome = demo_outcome(&board);
    assert!(outcome.contains("Revoked    4 of 5 workers"), "{outcome}");
    let status = demo_status(&board);
    let workers = demo_workers(&status);
    let active: Vec<_> = workers
        .iter()
        .filter(|worker| worker["revoked"] == false)
        .collect();
    assert_eq!(workers.len(), 5);
    assert_eq!(active.len(), 1);
    let id = active[0]["id"].as_str().unwrap();
    assert!(
        outcome.contains(&format!("Could not revoke worker {id}")),
        "{outcome}"
    );
    assert_demo_handovers_removed(&board);
}

#[test]
fn demo_preserves_the_leads_failure_after_successful_worker_reports() {
    assert_demo_worker_outcome("0 0 0 0 0", 5, 29, 29);
}

#[test]
fn demo_runs_an_accepted_codex_launch_to_completion() {
    let board = Fixture::new();
    let result = output(kept_demo_fixture(&board, "codex", "success"));
    assert_eq!(result.status.code(), Some(0), "{result:?}");
    assert!(demo_outcome(&board).contains("5 of 5 workers reported"));
    assert_eq!(
        messages(&demo_lead(&board, &["read", "lead", "--json"])).len(),
        5
    );
}

#[test]
fn demo_waits_for_the_lead_to_finish() {
    let board = Fixture::new();
    let (events, address) = demo_events(&board);
    let mut command = kept_demo_fixture(&board, "claude", "background");
    command.env("MALAK_TEST_DEMO_EVENTS", address);
    let (finished, received) = std::sync::mpsc::channel();
    let demo = std::thread::spawn(move || {
        let _ = finished.send(output(command));
    });
    assert_eq!(next_demo_event(&events), "lead");
    let workspace = demo_workspace(&board);
    let outcome = workspace.join("outcome.txt");

    wait_for_event("demo workers to report", || {
        (messages(&demo_lead(&board, &["read", "lead", "--json"])).len() >= 5).then_some(())
    });
    assert!(received.try_recv().is_err());
    assert!(!outcome.exists());
    fs::write(workspace.join("finish-demo"), "").unwrap();
    let finished = received.recv_timeout(TEST_WATCHDOG).unwrap();
    demo.join().unwrap();

    let stdout = stdout(finished);
    assert!(stdout.contains(outcome.to_str().unwrap()), "{stdout}");
    assert!(demo_outcome(&board).contains("5 of 5 workers reported"));
}

#[test]
fn demo_stops_the_codex_probe_even_when_it_ignores_termination() {
    let board = Fixture::new();
    board.start();
    let mut demo = kept_demo_fixture(&board, "codex", "success");
    demo.env("MALAK_TEST_CODEX_PROBE_STALLS", "1");

    run(demo);

    let workspace = demo_workspace(&board);
    let pid = fs::read_to_string(workspace.join("codex-probe.pid")).unwrap();
    assert_demo_process_stopped(&pid);
    assert!(!workspace.join("codex-probe-expired").exists());
    assert!(demo_outcome(&board).contains("Demo exit  0"));
}

#[test]
fn demo_counts_an_accepted_report_after_its_launcher_is_stopped() {
    let board = Fixture::new();
    board.start();
    let cat = board.home.path().join("cat");
    fs::write(&cat, include_str!("fixtures/demo_pause_report.py")).unwrap();
    fs::set_permissions(cat, fs::Permissions::from_mode(0o700)).unwrap();

    let result = output(kept_demo_fixture(&board, "claude", "pause-report"));
    let outcome = demo_outcome(&board);

    assert!(
        demo_workspace(&board)
            .join("report-launcher-paused")
            .is_file()
    );
    assert!(messages(&demo_lead(&board, &["read", "lead", "--json"])).is_empty());
    for position in 1..=5 {
        let retained: Value = serde_json::from_str(&demo_lead(
            &board,
            &["get", "--json", "lead", &position.to_string()],
        ))
        .unwrap();
        assert_eq!(retained["kind"], "report", "{retained}");
    }
    assert!(outcome.contains("5 of 5 delivered"), "{outcome}");
    assert!(outcome.contains("5 of 5 workers reported"), "{outcome}");
    assert_eq!(result.status.code(), Some(0), "{result:?}");
}

#[test]
fn demo_excludes_local_receipts_without_board_acceptance() {
    let board = Fixture::new();
    board.start();
    let launcher = board.home.path().join("fixture-malak");
    fs::write(
        &launcher,
        include_str!("fixtures/demo_receipt_without_send.py"),
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let fixture = demo_fixture(&board, "claude", "receipt-without-send");

    // Substitute a test launcher at the process boundary. Its reporter claims
    // it sent a report, while all board queries still use the real CLI.
    let mut demo = Command::new("python3");
    demo.current_dir(board.home.path())
        .envs(
            fixture
                .get_envs()
                .filter_map(|(key, value)| value.map(|v| (key, v))),
        )
        .env("MALAK_RECIPE_EXE", launcher)
        .env("MALAK_TEST_EXE", MALAK)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes/demo.py"))
        .args(["claude", "Fixture goal", "--keep"]);
    let result = output(demo);

    assert_eq!(
        fs::read_dir(demo_workspace(&board))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("synthetic-receipt-"))
            .count(),
        5,
        "{result:?}"
    );
    assert!(messages(&demo_lead(&board, &["read", "lead", "--json"])).is_empty());
    let outcome = demo_outcome(&board);
    assert!(
        outcome.contains("Demo stopped because worker reports were missing."),
        "{outcome}"
    );
    assert!(outcome.contains("0 of 5 delivered"), "{outcome}");
    assert!(outcome.contains("0 of 5 workers reported"), "{outcome}");
    assert_eq!(result.status.code(), Some(3), "{result:?}");
}

#[test]
fn demo_writes_its_outcome_when_a_worker_report_is_refused() {
    let board = Fixture::new();
    let result = output(kept_demo_fixture(&board, "codex", "refused"));
    assert_eq!(result.status.code(), Some(3), "{result:?}");
    let recorded = demo_outcome(&board);
    assert!(recorded.contains("Demo exit  3"), "{recorded}");
    assert!(recorded.contains("4 of 5 workers reported"), "{recorded}");
    assert!(recorded.contains("4 of 5 delivered"), "{recorded}");
    assert_eq!(
        messages(&demo_lead(&board, &["read", "lead", "--json"])).len(),
        4
    );
    assert_demo_workers_revoked(&board, 5);
}

#[test]
fn demo_writes_its_outcome_when_a_worker_log_cannot_be_opened() {
    let board = Fixture::new();
    board.start();
    let result = output(kept_demo_fixture(&board, "claude", "unopened-log"));
    assert_eq!(result.status.code(), Some(3), "{result:?}");
    let outcome = fs::read_to_string(demo_workspace(&board).join("outcome.txt"))
        .unwrap_or_else(|error| panic!("{error}: {result:?}"));
    assert!(outcome.contains("4 of 5 delivered"), "{result:?}");
    assert_demo_workers_revoked(&board, 4);
}

#[test]
fn demo_stops_a_worker_waiting_for_an_answer_after_the_lead_exits() {
    let board = Fixture::new();
    board.start();
    let mut demo = RunningWorker(
        kept_demo_fixture(&board, "claude", "dead-lead")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let status = wait_for_event("demo to stop after the lead exits", || {
        demo.0.try_wait().unwrap()
    });
    let workspace = demo_workspace(&board);
    assert_eq!(status.code(), Some(29), "{status}");
    let outcome = demo_outcome(&board);
    assert!(outcome.contains("Lead exit  29"), "{outcome}");
    assert!(outcome.contains("Demo exit  29"), "{outcome}");
    assert!(outcome.contains("4 of 5 workers reported"), "{outcome}");
    assert!(outcome.contains("4 of 5 delivered"), "{outcome}");
    assert_eq!(
        messages(&demo_lead(&board, &["read", "questions", "--json"])).len(),
        1
    );
    assert_demo_process_stopped(&fs::read_to_string(workspace.join("worker-1.running")).unwrap());
    assert_demo_workers_revoked(&board, 5);
}

#[test]
fn demo_fails_if_the_lead_exits_zero_before_launching_workers() {
    let board = Fixture::new();
    board.start();
    let mut demo = demo_with_claude(&board, "exit 0");
    demo.args(["Fixture goal", "--keep"]);

    let result = output(demo);

    assert_eq!(result.status.code(), Some(3), "{result:?}");
    assert!(demo_outcome(&board).contains("0 of 5 workers reported"));
}

#[test]
fn demo_keeps_the_lead_status_if_outcome_publication_fails() {
    let board = Fixture::new();
    board.start();
    let mut demo = demo_with_claude(&board, "mkdir outcome.txt; exit 29");
    demo.args(["Fixture goal", "--keep"]);

    let result = output(demo);

    assert_eq!(result.status.code(), Some(29), "{result:?}");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("Lead exit  29"), "{stderr}");
    assert!(stderr.contains("publish the demo outcome"), "{stderr}");
    assert!(demo_workspace(&board).join("outcome.txt").is_dir());
    assert!(!fs::read_dir(demo_workspace(&board)).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".outcome.")
    }));
}

#[test]
fn demo_sigint_during_startup_stops_the_lead_and_removes_the_board() {
    let board = Fixture::new();
    let (events, address) = demo_events(&board);
    let mut demo = RunningWorker(
        demo_fixture(&board, "claude", "start-signal")
            .env("MALAK_TEST_DEMO_EVENTS", address)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_eq!(next_demo_event(&events), "lead");
    let name = demo_folder(&board).file_name().unwrap().to_owned();
    let pid = fs::read_to_string(demo_workspace(&board).join("lead.running")).unwrap();
    signal_demo_process("-INT", &demo.0.id().to_string());
    let status = wait_for_event("demo to stop after Ctrl-C", || demo.0.try_wait().unwrap());

    assert_eq!(status.code(), Some(130), "{status}");
    assert_demo_removed(&board, name.to_str().unwrap());
    assert_demo_process_stopped(&pid);
}

#[test]
fn demo_sigterm_stops_the_lead_and_workers_and_writes_an_outcome() {
    assert_interrupted_demo_stops_workers("-TERM", 143);
}

#[test]
fn demo_revokes_workers_after_receiving_sigint() {
    assert_interrupted_demo_stops_workers("-INT", 130);
}

#[test]
fn demo_revokes_a_worker_enrolled_just_before_sigint() {
    let board = Fixture::new();
    board.start();
    let mut demo = kept_demo_fixture(&board, "claude", "success");
    demo.env("MALAK_TEST_DEMO_INTERRUPT_ENROLMENT", "1");

    let result = output(demo);
    assert_eq!(result.status.code(), Some(130), "{result:?}");
    assert_demo_workers_revoked(&board, 1);
}

fn assert_interrupted_demo_stops_workers(signal: &str, exit: i32) {
    let board = Fixture::new();
    board.start();
    let (events, address) = demo_events(&board);
    let mut demo = RunningWorker(
        kept_demo_fixture(&board, "claude", "signal")
            .env("MALAK_TEST_DEMO_EVENTS", address)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut running = std::collections::BTreeSet::new();
    for _ in 0..6 {
        assert!(running.insert(next_demo_event(&events)));
    }
    assert!(running.contains("lead"));
    for number in 1..=5 {
        assert!(running.contains(&format!("worker-{number}")));
    }
    let workspace = demo_workspace(&board);
    let pids: Vec<_> = std::iter::once(workspace.join("lead.running"))
        .chain((1..=5).map(|number| workspace.join(format!("worker-{number}.running"))))
        .map(|path| fs::read_to_string(path).unwrap())
        .collect();
    signal_demo_process(signal, &demo.0.id().to_string());
    let status = wait_for_event("the interrupted demo to return", || {
        demo.0.try_wait().unwrap()
    });
    assert_eq!(status.code(), Some(exit), "{status}");
    let outcome = demo_outcome(&board);
    assert!(outcome.contains(&format!("Demo exit  {exit}")), "{outcome}");
    assert!(outcome.contains("0 of 5 workers reported"), "{outcome}");
    assert!(!fs::read_dir(&workspace).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("tmp.")
    }));
    for pid in pids {
        assert_demo_process_stopped(&pid);
    }
    assert_demo_workers_revoked(&board, 5);
}

fn signal_demo_process(signal: &str, pid: &str) {
    let mut kill = Command::new("/bin/kill");
    kill.args([signal, pid]);
    run(kill);
}

fn demo_events(board: &Fixture) -> (UnixDatagram, PathBuf) {
    let address = board.home.path().join("demo-events");
    let events = UnixDatagram::bind(&address).unwrap();
    events.set_read_timeout(Some(TEST_WATCHDOG)).unwrap();
    (events, address)
}

fn next_demo_event(events: &UnixDatagram) -> String {
    let mut message = [0; 32];
    let length = events.recv(&mut message).expect("demo checkpoint stalled");
    String::from_utf8(message[..length].to_vec()).unwrap()
}

fn assert_demo_process_stopped(pid: &str) {
    wait_for_event(&format!("demo process {pid} to stop"), || {
        let mut ps = Command::new("/bin/ps");
        ps.args(["-o", "stat=", "-p", pid]);
        let state = output(ps);
        (!state.status.success()
            || String::from_utf8_lossy(&state.stdout)
                .trim()
                .starts_with('Z'))
        .then_some(())
    });
}

fn demo_folders(board: &Fixture) -> Vec<PathBuf> {
    fs::read_dir(board.home.path().join(".malak"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("state/.malak-demo").is_file())
        .collect()
}

fn demo_folder(board: &Fixture) -> PathBuf {
    let folders = demo_folders(board);
    assert_eq!(folders.len(), 1, "{folders:?}");
    folders.into_iter().next().unwrap()
}

fn demo_owner(board: &Fixture, folder: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(MALAK);
    command
        .arg("--config")
        .arg(folder.join("board.toml"))
        .env("HOME", board.home.path())
        .args(args);
    command
}

fn demo_status(board: &Fixture) -> Value {
    let command = demo_owner(board, &demo_folder(board), &["board", "status", "--json"]);
    serde_json::from_str(&run(command)).unwrap()
}

fn demo_lead(board: &Fixture, args: &[&str]) -> String {
    let handover = demo_folder(board).join("state/lead.handover");
    run(participant_command(&handover, args))
}

fn find_demo_workspace(board: &Fixture) -> Option<PathBuf> {
    let folder = demo_folders(board).into_iter().next()?;
    fs::read_dir(folder.join("state"))
        .unwrap()
        .filter_map(Result::ok)
        .find_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("demo.")
                .then(|| entry.path())
        })
}

fn demo_workspace(board: &Fixture) -> PathBuf {
    find_demo_workspace(board).expect("demo workspace")
}

fn demo_outcome(board: &Fixture) -> String {
    let path = demo_workspace(board).join("outcome.txt");
    demo_outcome_at(&path)
}

fn demo_outcome_at(path: &Path) -> String {
    wait_for_event("demo outcome", || match fs::read_to_string(path) {
        Ok(outcome) => Some(outcome),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("read {}: {error}", path.display()),
    })
}

#[test]
fn uninstall_removes_stock_recipe_state_and_keeps_unrelated_files() {
    let board = Fixture::new();
    board.start();
    fs::write(board.home.path().join("brief"), "Review the build.\n").unwrap();
    run(board.worker_start(&["--brief", "brief"], &["printf", "Build finished.\n"]));
    let mut queue = board.recipe("queue");
    queue.args([
        "reviewer",
        "--board",
        BOARD,
        "--no-watcher",
        "--after",
        "alice",
        "--brief",
        "brief",
        "--",
        "printf",
        "Review finished.\n",
    ]);
    run(queue);
    assert!(board.state.join("launches").is_dir());
    // Exercise the workspace layout used before demos had their own boards.
    let legacy_demo = board.state.join("demo.abcdefgh");
    fs::create_dir(&legacy_demo).unwrap();
    fs::write(legacy_demo.join("task-1.md"), "Legacy demo task").unwrap();
    // Malak made these private whatever the umask, so do not inherit it.
    fs::set_permissions(&legacy_demo, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(
        legacy_demo.join("task-1.md"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        fs::read_dir(&board.state)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("worker."))
            .count(),
        2
    );

    let foreign = board.state.join("my-notes.txt");
    fs::write(&foreign, "Keep my notes.").unwrap();
    let mut uninstall = board.owner(&["uninstall", "--yes", "--delete-data"]);
    board.recipe_environment(&mut uninstall);
    uninstall
        .env("CARGO_HOME", board.home.path().join("cargo-home"))
        .env_remove("CARGO_INSTALL_ROOT");
    let output = run(uninstall);

    let retained: Vec<_> = fs::read_dir(&board.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(retained, ["my-notes.txt"], "{output}");
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "Keep my notes.");
    assert!(output.contains("my-notes.txt (outside Malak's managed data)"));
}

fn demo_name(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("The demo runs on its own board, "))
        .and_then(|line| line.strip_suffix('.'))
        .unwrap_or_else(|| panic!("demo did not name its board: {stdout}"))
        .to_owned()
}

fn board_registry(board: &Fixture) -> String {
    fs::read_to_string(board.home.path().join(".malak/boards.jsonl")).unwrap()
}

fn assert_demo_removed(board: &Fixture, name: &str) {
    let folder = board.home.path().join(".malak").join(name);
    assert!(!folder.exists(), "left behind {}", folder.display());
    assert!(
        !board_registry(board).contains(name),
        "{name} is still registered"
    );
}

fn demo_workers(status: &Value) -> Vec<&Value> {
    status["participants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|worker| worker["name"].as_str().unwrap().starts_with("demo-"))
        .collect()
}

fn assert_demo_workers_revoked(board: &Fixture, count: usize) {
    let outcome = demo_outcome(board);
    let status = demo_status(board);
    let workers = demo_workers(&status);
    assert_eq!(workers.len(), count, "{workers:?}");
    assert!(
        workers.iter().all(|worker| worker["revoked"] == true),
        "{workers:?}"
    );
    let revocations = format!("Revoked    {count} of {count} workers");
    assert!(outcome.contains(&revocations), "{outcome}");
    assert_demo_handovers_removed(board);
}

fn assert_demo_handovers_removed(board: &Fixture) {
    assert!(
        fs::read_dir(demo_folder(board).join("state"))
            .unwrap()
            .all(|entry| {
                let entry = entry.unwrap();
                !entry.file_name().to_str().unwrap().starts_with("worker.")
                    || !entry.path().join("handover").exists()
            })
    );
}

fn demo_fixture(board: &Fixture, tool: &str, mode: &str) -> Command {
    let mut demo = demo_command(board, tool);
    configure_demo_fixture(board, &mut demo, tool, mode);
    demo.arg("Fixture goal");
    if tool == "codex" {
        demo.arg("--yes");
    }
    demo
}

fn kept_demo_fixture(board: &Fixture, tool: &str, mode: &str) -> Command {
    let mut demo = demo_fixture(board, tool, mode);
    demo.arg("--keep");
    demo
}

fn configure_demo_fixture(board: &Fixture, demo: &mut Command, tool: &str, mode: &str) {
    configure_demo_agent(board, demo, tool, include_str!("fixtures/demo_agent.py"));
    demo.env("MALAK_TEST_DEMO_MODE", mode);
}

fn assert_demo_worker_outcome(
    worker_statuses: &str,
    succeeded: usize,
    lead_exit: i32,
    expected_exit: i32,
) {
    let board = Fixture::new();
    board.start();
    let mut demo = kept_demo_fixture(&board, "claude", "worker-status");
    demo.env("MALAK_TEST_WORKER_STATUSES", worker_statuses)
        .env("MALAK_TEST_LEAD_EXIT", lead_exit.to_string());

    let result = output(demo);
    let recorded = demo_outcome(&board);
    assert!(
        recorded.contains(&format!("Lead exit  {lead_exit}")),
        "{recorded}"
    );
    assert!(
        recorded.contains(&format!("Demo exit  {expected_exit}")),
        "{recorded}"
    );
    let reports = messages(&demo_lead(&board, &["read", "lead", "--json"]));
    assert_eq!(reports.len(), 5);
    for report in reports {
        let text = report["text"].as_str().unwrap();
        let (status, stdout) = text.split_once("\n\n").unwrap();
        let code = stdout.trim().strip_prefix("Fixture worker exit ").unwrap();
        assert_eq!(status, format!("Worker exit status: {code}"));
    }
    assert_eq!(result.status.code(), Some(expected_exit), "{result:?}");
    let count = format!("{succeeded} of 5 workers reported");
    assert!(recorded.contains(&count), "{recorded}");
    assert_demo_workers_revoked(&board, 5);
}

fn demo_with_claude(board: &Fixture, script: &str) -> Command {
    let mut demo = demo_command(board, "claude");
    let script = format!("#!/bin/sh\n{script}\n");
    configure_demo_agent(board, &mut demo, "claude", &script);
    demo
}

fn demo_command(board: &Fixture, tool: &str) -> Command {
    let path = env::var_os("PATH").unwrap();
    let path = env::join_paths(
        std::iter::once(board.home.path().to_owned()).chain(env::split_paths(&path)),
    )
    .unwrap();
    let mut command = board.recipe("demo");
    command.arg(tool).env("PATH", path);
    command
}

fn configure_demo_agent(board: &Fixture, demo: &mut Command, tool: &str, script: &str) {
    let path = board.home.path().join(tool);
    fs::write(&path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    demo.env("MALAK_TEST_WATCHDOG", TEST_WATCHDOG.as_secs().to_string());
    recipe_python_startup(board, demo, include_bytes!("fixtures/demo_checkpoints.py"));
}

#[test]
fn repeating_a_handoff_does_not_post_another_task_or_stand_down() {
    let board = Fixture::new();
    board.start();
    board.enrol("alice", &[]);
    board.enrol("carol", &[]);
    let status = board.status();
    let inbox = |name| format!("to/{}", participant(&status, name)["id"].as_str().unwrap());
    let old = inbox("alice");
    let new = inbox("carol");
    board.lead(&["send", "--kind", "task", &old, "Keep working on this"]);
    let handoff = || {
        let mut command = board.recipe("handoff");
        command
            .args(["alice", "1", "--to", "carol", "--config"])
            .arg(&board.config);
        command
    };

    run(handoff());
    run(handoff());

    assert_eq!(messages(&board.lead(&["read", "--json", &new])).len(), 1);
    assert_eq!(messages(&board.lead(&["read", "--json", &old])).len(), 2);
}

#[test]
fn lead_up_starts_a_board_from_an_explicit_config() {
    let board = Fixture::new();
    let mut shell = Command::new("sh");
    board.recipe_environment(&mut shell);
    shell
        .args(["-ec", "unset MALAK_HANDOVER; eval \"$(malak recipe lead-up --config \"$1\")\"; printf '%s' \"$MALAK_HANDOVER\"", "lead-up-test"])
        .arg(&board.config);

    assert_eq!(
        run(shell),
        board.state.join("lead.handover").to_str().unwrap()
    );
    assert!(board.status()["run"].is_string());
}

#[test]
fn lead_up_prints_the_required_watcher_and_ack_steps_before_the_guide() {
    let board = Fixture::new();
    let result = output(board.recipe("lead-up"));
    assert!(result.status.success(), "{result:?}");
    let diagnostics = String::from_utf8(result.stderr).unwrap();
    assert!(
        diagnostics.contains("ready. Lead check passed."),
        "{diagnostics}"
    );
    let steps = [
        "Run `malak --board malak mailbox --plain --for lead --next` in the background.",
        "Handle the output, acknowledge lead messages, and run it again.",
        "Acknowledge each lead message once handled with `malak --board malak ack --creation CREATION lead POSITION`.",
        "Without the watcher, reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.",
        "malak recipe --help",
    ];
    let mut remaining = diagnostics.as_str();
    for step in steps {
        remaining = remaining
            .split_once(step)
            .unwrap_or_else(|| panic!("missing or out of order: {step}\n{diagnostics}"))
            .1;
    }
}

#[test]
fn lead_up_reuses_a_running_board() {
    let board = Fixture::new();
    run(board.recipe("lead-up"));
    let before = board.status()["run"].clone();

    let mut command = board.recipe("lead-up");
    command.arg(BOARD);
    run(command);

    assert_eq!(board.status()["run"], before);
}

#[test]
fn checking_a_handover_leaves_its_call_budget_intact_on_a_loaded_board() {
    let board = Fixture::new();
    board.configure(r#"persistence = "sqlite""#);
    board.start();
    run(board.owner(&["channel", "create", "shared"]));
    let lead = board.state.join("lead.handover");
    retained::seed(&lead, "lead", 2_000);
    retained::seed(&lead, "shared", 100);
    board.lead(&["send", "questions", "Still open?"]);
    let handover = board.enrol("reader", &["shared:get", "questions:get"]);

    let mut command = board.recipe("lead-up");
    command.env("MALAK_HANDOVER", &handover);
    run(command);

    // The board admits 600 calls per minute for this participant. The
    // check needs an attachment, but should spend none of those calls.
    let mut reader = retained::connect(&handover);
    for rid in 2..602 {
        assert_eq!(retained::get(&mut reader, rid, "shared"), "OK");
    }
}

#[test]
fn checking_a_handover_refuses_a_revoked_credential() {
    let board = Fixture::new();
    board.start();
    let handover = board.enrol("reader", &[]);
    let status = board.status();
    let id = participant(&status, "reader")["id"].as_str().unwrap();
    run(board.owner(&["revoke", id]));

    let mut command = board.recipe("__check-board");
    command.env("MALAK_HANDOVER", handover);
    let result = output(command);

    assert_eq!(result.status.code(), Some(3), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("could not attach"),
        "{result:?}"
    );
}

#[test]
fn checking_a_handover_identifies_a_stopped_board() {
    let board = Fixture::new();
    board.start();
    run(board.owner(&["board", "stop"]));
    wait_for_event("the owner listener to close", || {
        let status = output(board.owner(&["board", "status"]));
        String::from_utf8_lossy(&status.stderr)
            .contains("no board is answering")
            .then_some(())
    });

    let result = output(board.recipe("__check-board"));

    assert_eq!(result.status.code(), Some(3), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("no board is answering"),
        "{result:?}"
    );
}

#[test]
fn a_board_runs_with_an_unavailable_configured_parent() {
    let board = Fixture::new();
    board.configure(r#"parent = "absent""#);

    board.start();

    board.lead(&["send", "lead", "For this board"]);
    assert!(board.lead(&["read", "lead"]).contains("For this board"));
}

#[test]
fn lead_up_acknowledges_the_probe_without_consuming_lead_mail() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "waiting"]);

    board.start();

    assert!(messages(&board.lead(&["read", "selftest/lead", "--json"])).is_empty());
    assert!(board.lead(&["read", "lead"]).contains("waiting"));
}

#[test]
fn lead_up_reports_a_failed_ack_without_printing_ready() {
    let board = Fixture::new();
    board.start();
    run(board.owner(&["grant", "lead", "selftest/lead:send,read"]));

    let result = output(board.recipe("lead-up"));

    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("self-test failed at acknowledge note"),
        "{error}"
    );
    assert!(!error.lines().any(|line| line == "ready"), "{error}");
}

#[test]
fn lead_up_lists_running_and_stopped_boards() {
    let board = Fixture::new();
    let home = board.home.path();
    Fixture::write_config(home, "parked", &home.join("parked"), "e");
    board.start();
    board.enrol("alice", &[]);
    board.lead(&["send", "lead", "first"]);
    board.lead(&["send", "lead", "second"]);

    let mut command = board.recipe("lead-up");
    command.arg("--list");
    let listed = run(command);

    assert!(
        board_row(&listed, "malak") == ["Running", "alice, lead", "2"],
        "{listed}"
    );
    assert!(
        board_row(&listed, "parked") == ["Stopped", "Unavailable", "Unavailable"],
        "{listed}"
    );
}

#[test]
fn board_list_reports_live_counts_and_keeps_stopped_boards() {
    let board = Fixture::new();
    let home = board.home.path();
    Fixture::write_config(home, "parked", &home.join("parked"), "e");
    board.start();
    board.enrol("alice", &[]);
    board.lead(&["send", "lead", "first"]);
    board.lead(&["read", "--ack", "lead"]);
    board.lead(&["send", "lead", "second"]);
    let mut list = Command::new(MALAK);
    board.recipe_environment(&mut list);
    list.args(["board", "list", "--json"]);

    let listed: Value = serde_json::from_str(&run(list)).unwrap();

    assert_eq!(
        listed,
        json!([
            {"name": "malak", "status": "running", "participants": 2, "lead_waiting": 1,
             "build_warning": null},
            {"name": "parked", "status": "stopped", "participants": null, "lead_waiting": null,
             "build_warning": null}
        ])
    );
}

fn board_tree(board: &Fixture) -> String {
    let mut command = board.recipe("lead-up");
    command.arg("--tree");
    run(command)
}

fn board_row<'a>(table: &'a str, name: &str) -> Vec<&'a str> {
    let row = table
        .lines()
        .skip(2)
        .find_map(|line| line.trim_start().strip_prefix(name))
        .unwrap();
    row.split("  ")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect()
}

fn tree_names(tree: &str) -> Vec<&str> {
    tree.lines()
        .skip(2)
        .map(|line| {
            let line = line.strip_prefix("  ").unwrap();
            let name = line.trim_start();
            let width = name.split("  ").next().unwrap().len();
            &line[..line.len() - name.len() + width]
        })
        .collect()
}

#[test]
fn lead_up_displays_three_layers_in_parent_order() {
    let root = Fixture::new();
    let middle = root.sibling("middle", "e");
    let leaf = root.sibling("leaf", "f");
    middle.configure(r#"parent = "malak""#);
    leaf.configure(r#"parent = "middle""#);

    let tree = board_tree(&root);

    assert_eq!(tree_names(&tree), ["malak", "  middle", "    leaf"]);
}

#[test]
fn lead_up_tree_counts_participants_and_waiting_messages_on_running_boards() {
    let board = Fixture::new();
    board.start();
    board.enrol("alice", &[]);
    board.lead(&["send", "lead", "Waiting"]);

    let tree = board_tree(&board);

    assert_eq!(board_row(&tree, "malak"), ["Running", "2", "1"], "{tree}");
}

#[test]
fn lead_up_tree_marks_missing_parents_at_the_top_level() {
    let board = Fixture::new();
    board.configure(r#"parent = "absent""#);

    let tree = board_tree(&board);

    assert_eq!(tree_names(&tree), ["malak [missing parent absent]"]);
    assert_eq!(
        board_row(&tree, "malak [missing parent absent]"),
        ["Stopped", "Unavailable", "Unavailable"]
    );
}

#[test]
fn lead_up_tree_marks_a_parent_cycle_and_visits_each_board_once() {
    let board = Fixture::new();
    let other = board.sibling("other", "e");
    board.configure(r#"parent = "other""#);
    other.configure(r#"parent = "malak""#);

    let tree = board_tree(&board);

    assert_eq!(tree_names(&tree), ["malak [parent cycle]", "  other"]);
}

fn lead_up_with_parent(child: &Fixture) -> Command {
    let mut command = child.recipe("lead-up");
    let name = child.config.parent().unwrap().file_name().unwrap();
    command.arg(name).args(["--parent", BOARD]);
    command
}

#[test]
fn lead_up_writes_parent_in_the_board_table() {
    let parent = Fixture::new();
    parent.start();
    let handover = parent.enrol("alice", &["lead:send"]);
    let child = parent.sibling("review", "e");
    child.configure("[board.inbox_limits]\nenabled = true");
    let mut command = lead_up_with_parent(&child);
    command
        .args(["--id", &parent.worker_id(), "--handover"])
        .arg(handover);

    run(command);

    let config = fs::read_to_string(&child.config).unwrap();
    let settings: toml::Value = toml::from_str(&config).unwrap();
    assert_eq!(settings["board"]["parent"].as_str(), Some(BOARD));
    assert_eq!(
        settings["board"]["inbox_limits"]["enabled"].as_bool(),
        Some(true)
    );
}

#[test]
fn lead_up_reports_the_child_board_from_its_enrolled_manager() {
    let parent = Fixture::new();
    parent.start();
    let child = parent.sibling("review", "e");
    child.start();
    child.enrol("researcher", &[]);

    run(parent.worker_start(
        &[],
        &[
            "sh",
            "-ec",
            "setup=$(malak recipe lead-up review --parent malak); test -n \"$setup\"",
        ],
    ));

    let reports = messages(&parent.lead(&["read", "lead", "--json"]));
    let board_report = &reports[0];
    assert_eq!(board_report["sender"], parent.worker_id());
    assert_eq!(board_report["kind"], "report");
    let expected = format!(
        "Board review\nConfig {}\nParticipants lead, researcher\n",
        child.config.display()
    );
    assert_eq!(body(board_report), expected.as_bytes());
}

#[test]
fn lead_up_guides_a_manager_with_no_active_enrolment_on_the_parent() {
    let parent = Fixture::new();
    parent.start();
    let handover = parent.enrol("alice", &["lead:send"]);
    let id = parent.worker_id();
    run(parent.owner(&["revoke", &id]));
    let child = parent.sibling("review", "e");
    let mut command = lead_up_with_parent(&child);
    command.args(["--id", &id, "--handover"]).arg(handover);

    let result = output(command);

    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(
        error.contains("is not enrolled on parent 'malak'"),
        "{error}"
    );
    assert!(error.contains("worker-start NAME --board malak"), "{error}");
}

#[test]
fn lead_up_refuses_mismatched_parent_handover_before_writing() {
    let parent = Fixture::new();
    parent.start();
    parent.enrol("alice", &["lead:send"]);
    let other = parent.sibling("other", "e");
    other.start();
    let wrong_handover = other.enrol("bob", &["lead:send"]);
    let child = parent.sibling("review", "f");
    let mut command = lead_up_with_parent(&child);
    command
        .args(["--id", &parent.worker_id(), "--handover"])
        .arg(wrong_handover);

    let result = output(command);

    assert!(!result.status.success());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(messages(&other.lead(&["read", "--json", "lead"])).is_empty());
    assert!(
        error.contains("does not match the selected board"),
        "{error}"
    );
    assert!(!child.state.exists());
}

#[test]
fn worker_start_grants_access_to_the_worker_channels() {
    let board = Fixture::new();
    board.start();

    run(board.worker_start(&[], &["true"]));

    let status = board.status();
    let worker = participant(&status, "alice");
    let id = worker["id"].as_str().unwrap();
    let inbox = format!("to/{id}");
    let probe = format!("selftest/{id}");
    // The owner status encodes send/read/get/ack as 1/2/4/8.
    assert_eq!(
        worker["grant"][1],
        json!([
            ["lead", 1],
            ["relay/*", 1],
            ["questions", 1],
            ["orders", 6],
            [inbox, 14],
            [probe, 11]
        ])
    );
    let channels = status["channels"].as_array().unwrap();
    assert!(channels.iter().any(|channel| channel["name"] == inbox));
    assert!(channels.iter().any(|channel| channel["name"] == probe));
    assert!(channels.iter().all(|channel| channel["name"] != "to/alice"));
}

fn announced_handover(output: &str) -> &Path {
    let path = output
        .lines()
        .find_map(|line| line.strip_prefix("  Handover  "))
        .unwrap();
    Path::new(path)
}

#[test]
fn worker_start_creates_a_relay_channel_named_for_the_worker() {
    let board = Fixture::new();
    board.start();

    run(board.worker_start(&[], &["true"]));

    let status = board.status();
    let channels = status["channels"].as_array().unwrap();
    assert!(
        channels
            .iter()
            .any(|channel| channel["name"] == "relay/alice")
    );
}

#[test]
fn worker_start_announces_the_canonical_inbox() {
    let board = Fixture::new();
    board.start();

    let launched = run(board.worker_start(&[], &["true"]));

    let inbox = format!("to/{}", board.worker_id());
    assert!(launched.starts_with("Worker alice ready."));
    assert!(
        launched
            .lines()
            .any(|line| line.trim_start().starts_with("Inbox") && line.ends_with(&inbox))
    );
}

#[test]
fn worker_start_launches_with_the_workers_handover() {
    let board = Fixture::new();
    board.start();
    let seen = board.home.path().join("seen");

    let launched = run(board.worker_start(
        &[],
        &[
            "sh",
            "-c",
            r#"printf '%s' "$MALAK_HANDOVER" > "$1""#,
            "worker",
            seen.to_str().unwrap(),
        ],
    ));

    let seen = fs::read_to_string(seen).unwrap();
    assert_eq!(Path::new(&seen), announced_handover(&launched));
    assert_ne!(Path::new(&seen), board.state.join("lead.handover"));
}

#[test]
fn worker_start_retries_a_dropped_lead_attachment() {
    let board = Fixture::new();
    board.start();

    // Drop the first connection at the lead entrance, then restore the real listener.
    let socket = board.home.path().join("d/l/s");
    let parked = socket.with_file_name("parked");
    fs::rename(&socket, &parked).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child = board
        .worker_start(&[], &["true"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let connection = wait_for_event("the recipe handover check to connect", || {
        match listener.accept() {
            Ok((connection, _)) => Some(connection),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(error) => panic!("could not accept the test connection: {error}"),
        }
    });
    fs::rename(parked, socket).unwrap();
    drop(connection);

    stdout(child.wait_with_output().unwrap());
}

#[test]
fn worker_start_folds_the_nested_handover_error() {
    let board = Fixture::new();
    board.start();
    let mut command = board.worker_start(&[], &["true"]);
    command.env("MALAK_HANDOVER", board.home.path().join("missing.handover"));

    let result = output(command);

    assert_eq!(result.status.code(), Some(2));
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("no handover file at"), "{error}");
    assert!(!error.contains(r"\n"), "{error}");
}

#[test]
fn worker_hook_state_uses_a_private_root_outside_the_board() {
    let board = Fixture::new();
    board.start();
    let shared = board.home.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    let mut worker = board.worker_start(
        &[],
        &[
            "sh",
            "-ec",
            "test -w \"$MALAK_HOOK_STATE\"; printf '%s' \"$MALAK_HOOK_STATE\"",
        ],
    );
    worker.env("TMPDIR", &shared);

    run(worker);

    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    let state = reports[0]["text"].as_str().unwrap();
    let state = Path::new(state.strip_prefix("Worker exit status: 0\n\n").unwrap());
    assert_eq!(
        state.parent(),
        Some(board.home.path().canonicalize().unwrap().as_path())
    );
    assert!(!state.starts_with(shared));
    assert!(!state.starts_with(&board.state));
    assert!(!state.exists());
}

#[test]
fn worker_start_reports_an_exit_under_the_workers_identity() {
    let board = Fixture::new();
    board.start();

    let result = output(board.worker_start(&[], &["sh", "-c", "echo private >&2; exit 9"]));

    assert_eq!(result.status.code(), Some(9));
    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(reports.len(), 1);
    let report = &reports[0];
    assert_eq!(report["sender"], board.worker_id());
    assert_eq!(participant(&board.status(), "alice")["revoked"], false);
    assert!(announced_handover(&String::from_utf8_lossy(&result.stdout)).is_file());
    assert_eq!(report["kind"], "report");
    assert_eq!(
        body(report),
        b"Worker exited without a result (exit status 9)."
    );
}

#[test]
fn worker_start_reports_and_returns_the_status_of_a_signalled_command() {
    let board = Fixture::new();
    board.start();

    let result = output(board.worker_start(&[], &["sh", "-c", "kill -TERM \"$$\""]));

    assert_eq!(result.status.code(), Some(143));
    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["sender"], board.worker_id());
    assert_eq!(
        body(&reports[0]),
        b"Worker exited without a result (exit status 143)."
    );
}

fn finish_worker_with_board_stopped(board: &Fixture, worker: &mut Command) -> Child {
    let child = worker.stdout(Stdio::piped()).spawn().unwrap();
    wait_for_file(board, "worker-ready");
    run(board.owner(&["board", "stop"]));
    fs::write(board.home.path().join("worker-release"), "").unwrap();
    child
}

const FINISH_ON_RELEASE: &[&str] = &[
    "sh",
    "-ec",
    r#"touch worker-ready
    checks=0
    while [ ! -f worker-release ]; do
        [ "$checks" -lt 600 ] || exit 1
        checks=$((checks + 1))
        sleep 0.05
    done
    echo finished"#,
];

#[test]
fn worker_start_delivers_one_final_report_after_board_restart() {
    let board = Fixture::new();
    board.start();
    let mut worker = board.worker_start(&[], FINISH_ON_RELEASE);
    let log = board.home.path().join("launcher-stderr");
    worker.stderr(fs::File::create(&log).unwrap());
    let mut child = finish_worker_with_board_stopped(&board, &mut worker);
    wait_for_event("the launcher to retry its report", || {
        let stderr = fs::read_to_string(&log).unwrap();
        assert_eq!(child.try_wait().unwrap(), None, "{stderr}");
        stderr.contains("Retrying the worker report").then_some(())
    });

    run(board.owner(&["board", "start", "--detach"]));
    stdout(child.wait_with_output().unwrap());

    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["sender"], board.worker_id());
    assert_eq!(body(&reports[0]), b"Worker exit status: 0\n\nfinished\n");
}

#[test]
fn worker_start_keeps_the_retry_hint_when_the_board_stays_stopped() {
    let board = Fixture::new();
    board.start();
    let mut worker = board.worker_start(&[], FINISH_ON_RELEASE);
    worker
        .env("MALAK_TEST_REPORT_RETRY_SECONDS", "6")
        .stderr(Stdio::piped());
    let started = Instant::now();
    let result = finish_worker_with_board_stopped(&board, &mut worker)
        .wait_with_output()
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(25), "{result:?}");
    assert_eq!(result.status.code(), Some(3), "{result:?}");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("Retry the report with the same ID."),
        "{stderr}"
    );
    assert!(stderr.contains("Retrying the worker report"), "{stderr}");
}

#[test]
fn worker_start_does_not_retry_a_silent_refusal_of_a_revoked_credential() {
    let board = Fixture::new();
    board.start();
    let child = board
        .worker_start(&[], FINISH_ON_RELEASE)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&board, "worker-ready");

    run(board.owner(&["revoke", "alice"]));
    fs::write(board.home.path().join("worker-release"), "").unwrap();
    let result = child.wait_with_output().unwrap();

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("the worker enrolment was revoked"),
        "{stderr}"
    );
    assert!(!stderr.contains("Retrying the worker report"), "{stderr}");
}

#[test]
fn worker_start_verifies_a_duplicate_receipt_after_losing_the_first_reply() {
    let board = Fixture::new();
    board.start();
    let mut worker = board.worker_start(&[], &["echo", "finished"]);
    recipe_python_startup(
        &board,
        &mut worker,
        br#"
import pathlib
import subprocess

class DropFirstReceipt(subprocess.Popen):
    dropped = False

    def communicate(self, *args, **kwargs):
        stdout, stderr = super().communicate(*args, **kwargs)
        if (not self.dropped and self.returncode == 0
                and pathlib.Path(self.args[0]).name == "report.sh"):
            DropFirstReceipt.dropped = True
            self.returncode = 75
            return b"", stderr
        return stdout, stderr

subprocess.Popen = DropFirstReceipt
"#,
    );

    let launched = run(worker);

    assert!(
        launched.contains("Report already recorded in lead #"),
        "{launched}"
    );
    let mut awaiting = board.recipe("await");
    awaiting.args(["alice", "--timeout", "120s"]);
    let report = run(awaiting);
    assert!(report.contains("finished"), "{report}");
    assert_eq!(messages(&board.lead(&["read", "lead", "--json"])).len(), 1);
}

#[test]
fn worker_start_returns_a_report_refusal_and_retains_the_completed_worker() {
    let board = Fixture::new();
    board.start();
    let result = output(board.worker_start(&[], &["head", "-c", "262145", "/dev/zero"]));

    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(!String::from_utf8_lossy(&result.stderr).contains("Retrying the worker report"));
    assert!(messages(&board.lead(&["read", "lead", "--json"])).is_empty());
    assert_eq!(participant(&board.status(), "alice")["revoked"], false);
    assert!(announced_handover(&String::from_utf8_lossy(&result.stdout)).is_file());
}

#[test]
fn worker_start_keeps_the_worker_failure_when_the_report_is_also_refused() {
    let board = Fixture::new();
    board.start();
    let result = output(board.worker_start(&[], &["sh", "-c", "head -c 262145 /dev/zero; exit 9"]));

    assert_eq!(result.status.code(), Some(9), "{result:?}");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("Could not send the worker report"),
        "{stderr}"
    );
    assert!(stderr.contains("Report exit  2"), "{stderr}");
    assert!(messages(&board.lead(&["read", "lead", "--json"])).is_empty());
}

#[test]
fn worker_start_points_to_stderr_after_a_failed_launch() {
    let board = Fixture::new();
    board.start();

    let result = output(board.worker_start(&[], &["sh", "-c", "echo private >&2; exit 9"]));

    let stderr = String::from_utf8(result.stderr).unwrap();
    let log = stderr
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("Worker stderr"))
        .expect("missing stderr path")
        .trim();
    assert_eq!(fs::read_to_string(log).unwrap(), "private\n");
}

#[test]
fn worker_start_again_enrols_a_new_worker_and_reposts_the_brief() {
    let board = Fixture::new();
    board.start();
    let brief = board.home.path().join("brief");
    fs::write(&brief, b"Check the fixture.\n").unwrap();
    let failed = output(board.worker_start(
        &["--brief", brief.to_str().unwrap()],
        &["./absent-worker-command"],
    ));
    assert_eq!(failed.status.code(), Some(3));
    let old_id = board.worker_id();
    let ready = String::from_utf8(failed.stdout).unwrap();
    let old_handover = announced_handover(&ready);
    assert!(!old_handover.exists());
    fs::remove_file(brief).unwrap();

    let relaunched = run(board.worker_start(&["--again"], &["true"]));

    let status = board.status();
    let worker = status["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|person| person["name"] == "alice" && person["revoked"] == false)
        .unwrap();
    let new_id = worker["id"].as_str().unwrap();
    assert_ne!(new_id, old_id);
    assert_ne!(announced_handover(&relaunched), old_handover);
    let inbox = format!("to/{new_id}");
    let tasks = messages(&board.lead(&["read", &inbox, "--json"]));
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["kind"], "task");
    assert_eq!(
        body(&tasks[0]),
        format!("Your inbox: {inbox}\nCheck the fixture.\n").as_bytes()
    );
}

#[test]
fn worker_start_again_refuses_a_name_without_an_enrolment() {
    let board = Fixture::new();
    board.start();

    let result = output(board.worker_start(&["--again"], &["touch", "relaunched"]));

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no earlier enrolment"));
    assert!(!board.home.path().join("relaunched").exists());
    assert_eq!(board.status()["participants"].as_array().unwrap().len(), 1);
}

#[test]
fn worker_start_refuses_to_enrol_without_a_watcher_on_the_selected_board() {
    let board = Fixture::new();
    board.start();
    let other = board.sibling("other", "other-socket");
    other.start();
    let _watcher = agents::lead_watcher(&other);
    let before = board.status();

    let result = output(board.worker_start_command(&[], &["touch", "started"]));

    let diagnostics = String::from_utf8_lossy(&result.stderr);
    for text in [
        "Error: Could not run worker-start.",
        "no lead watcher is running on board malak; reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen.",
        "start it in the background with `malak --board malak mailbox --plain --for lead --next`, then run worker-start again (or pass --no-watcher).",
    ] {
        assert!(diagnostics.contains(text), "{diagnostics}");
    }
    assert!(!result.status.success(), "{diagnostics}");
    assert!(!board.home.path().join("started").exists());
    let after = board.status();
    assert_eq!(after["participants"], before["participants"]);
    assert_eq!(after["channels"], before["channels"]);
}

#[test]
fn worker_start_launches_with_a_watcher_on_the_selected_board() {
    let board = Fixture::new();
    board.start();
    let _watcher = agents::lead_watcher(&board);

    run(board.worker_start_command(&[], &["touch", "started"]));

    assert!(board.home.path().join("started").is_file());
}

#[test]
fn worker_start_launches_without_a_watcher_when_overridden() {
    let board = Fixture::new();
    board.start();

    let result = output(board.worker_start_command(&["--no-watcher"], &["touch", "started"]));

    assert!(result.status.success(), "{result:?}");
    assert!(board.home.path().join("started").is_file());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("lead watcher"));
}

#[test]
fn worker_start_warns_and_launches_when_watcher_detection_is_unavailable() {
    let board = Fixture::new();
    board.start();
    let mut command = board.worker_start_command(&[], &["touch", "started"]);
    recipe_python_startup(
        &board,
        &mut command,
        br#"import os
from pathlib import Path

open_path = os.open

def open_with_unavailable_watcher(path, flags, *args, **kwargs):
    if path == Path.home() / 'd' / 'o':
        raise PermissionError('watcher directory is unavailable')
    return open_path(path, flags, *args, **kwargs)

os.open = open_with_unavailable_watcher
"#,
    );

    let result = output(command);

    assert!(result.status.success(), "{result:?}");
    assert!(board.home.path().join("started").is_file());
    let diagnostics = String::from_utf8_lossy(&result.stderr);
    assert!(
        diagnostics
            .contains("Warning: Could not tell whether the lead watcher is running. Continuing."),
        "{diagnostics}"
    );
}

#[test]
fn worker_start_announces_when_no_native_hooks_are_configured() {
    let board = Fixture::new();
    board.start();

    let result = output(board.worker_start(&[], &["true"]));

    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("No native Malak hooks configured"));
}

#[test]
fn worker_start_stops_before_launch_when_the_hook_probe_fails() {
    let board = Fixture::new();
    board.start();
    let hook_refusal = board.home.path().join("malak");
    fs::write(
        &hook_refusal,
        concat!(
            "#!/bin/sh\n",
        "if [ \"$1\" = hook ] && [ \"$2\" = --check ]; then echo probe-refused >&2; exit 2; fi\n",
            "exec \"$TEST_MALAK\" \"$@\"\n",
        ),
    )
    .unwrap();
    fs::set_permissions(&hook_refusal, fs::Permissions::from_mode(0o700)).unwrap();
    let native = board.home.path().join("claude");
    fs::write(&native, "#!/bin/sh\ntouch tool-started\n").unwrap();
    fs::set_permissions(&native, fs::Permissions::from_mode(0o700)).unwrap();
    let path = env::var_os("PATH").unwrap();
    let path = env::join_paths(
        std::iter::once(board.home.path().to_owned()).chain(env::split_paths(&path)),
    )
    .unwrap();

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("recipes/worker-start");
    let hook_state = board.home.path().join("hooks");
    fs::create_dir(&hook_state).unwrap();
    let mut command = Command::new(script);
    board.recipe_environment(&mut command);
    command
        .current_dir(board.home.path())
        .args(["alice", "--board", BOARD, "--no-watcher", "--"])
        .arg(native)
        .env("PATH", path)
        .env("TEST_MALAK", MALAK)
        .env("MALAK_RECIPE_HOOK_STATE", hook_state)
        .env("MALAK_RECIPE_EXE", hook_refusal);

    let result = output(command);
    assert_eq!(result.status.code(), Some(3));
    assert!(!board.home.path().join("tool-started").exists());
    let revoked = board.status();
    assert_eq!(participant(&revoked, "alice")["revoked"], true);
    assert!(!announced_handover(&String::from_utf8_lossy(&result.stdout)).exists());
    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(reports[0]["sender"], participant(&revoked, "alice")["id"]);
    assert!(String::from_utf8_lossy(&result.stderr).contains("malak hook --check --tool claude"));
    assert!(String::from_utf8_lossy(&result.stderr).contains("probe-refused"));
}

#[test]
fn worker_start_revokes_the_unstarted_command_and_allows_fresh_enrolment() {
    let board = Fixture::new();
    board.start();
    let missing = board.home.path().join("missing-command");
    let result = output(board.worker_start(&[], &[missing.to_str().unwrap()]));

    assert_eq!(result.status.code(), Some(3), "{result:?}");
    let status = board.status();
    let failed = participant(&status, "alice");
    assert_eq!(failed["revoked"], true);
    assert!(!announced_handover(&String::from_utf8_lossy(&result.stdout)).exists());
    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(reports[0]["sender"], failed["id"]);

    run(board.worker_start(&[], &["true"]));
    let status = board.status();
    assert!(
        status["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|person| {
                person["name"] == "alice"
                    && person["revoked"] == false
                    && person["id"] != failed["id"]
            })
    );
}

#[test]
fn worker_start_puts_the_workers_inbox_on_the_first_line_of_its_first_task() {
    let board = Fixture::new();
    board.start();
    let file = board.home.path().join("brief");
    let brief = b"First line\nSecond line's content";
    fs::write(&file, brief).unwrap();

    let launched = run(board.worker_start(&["--brief", file.to_str().unwrap()], &["true"]));

    let inbox = format!("to/{}", board.worker_id());
    let mut read = Command::new(MALAK);
    read.args(["read", "--json", &inbox])
        .env("MALAK_HANDOVER", announced_handover(&launched));
    let messages = messages(&run(read));
    assert_eq!(messages.len(), 1);
    let task = &messages[0];
    assert_eq!(task["position"], 1);
    assert_eq!(task["sender"], "lead");
    assert_eq!(task["kind"], "task");
    let mut expected = format!("Your inbox: {inbox}\n").into_bytes();
    expected.extend_from_slice(brief);
    assert_eq!(body(task), expected);
}

#[test]
fn worker_start_links_the_automatic_report_to_its_brief() {
    assert_worker_message_links_to_brief("lead", &["true"]);
}

#[test]
fn worker_start_links_a_task_question_to_its_brief() {
    assert_worker_message_links_to_brief(
        "questions",
        &[
            "sh",
            "-ec",
            "printf 'Which choice?\n' | malak recipe ask --blocks work --option first --option second --recommend first",
        ],
    );
}

fn assert_worker_message_links_to_brief(channel: &str, command: &[&str]) {
    let board = Fixture::new();
    board.start();
    let brief = board.home.path().join("brief");
    fs::write(&brief, "Check the result").unwrap();
    run(board.worker_start(&["--brief", brief.to_str().unwrap()], command));

    let inbox = format!("to/{}", board.worker_id());
    let tasks = messages(&board.lead(&["read", "--json", &inbox]));
    let sent = messages(&board.lead(&["read", "--json", channel]));
    assert_eq!(sent[0]["headers"]["trace"], tasks[0]["headers"]["trace"]);
    assert_eq!(
        sent[0]["headers"]["reply_to"],
        json!([inbox, tasks[0]["creation"], 1])
    );
}

#[test]
fn worker_start_registers_the_claude_start_hook() {
    assert_worker_start_hook(
        "claude",
        r#"import json
assert sys.argv[1] == '--settings'
command = json.loads(sys.argv[2])['hooks']['SessionStart'][0]['hooks'][0]['command']
"#,
    );
}

#[test]
fn worker_start_allows_a_claude_worker_to_run_only_malak() {
    assert_worker_start_hook(
        "claude",
        r#"import json
settings = json.loads(sys.argv[2])
command = settings['hooks']['SessionStart'][0]['hooks'][0]['command']
assert settings['permissions'] == {'allow': ['Bash(malak:*)', 'Bash(' + os.environ['MALAK_RECIPE_EXE'] + ':*)']}, settings
assert sorted(settings) == ['hooks', 'permissions'], settings
"#,
    );
}

#[test]
fn worker_start_registers_the_codex_start_hook() {
    assert_worker_start_hook(
        "codex",
        r#"import tomllib
assert sys.argv[1] == '-c'
command = tomllib.loads(sys.argv[2])['hooks']['SessionStart'][0]['hooks'][0]['command']
"#,
    );
}

fn assert_worker_start_hook(tool: &str, load_command: &str) {
    let board = Fixture::new();
    board.start();
    let brief = board.home.path().join("brief.md");
    fs::write(&brief, "the brief reached the native hook").unwrap();
    let agent = board.home.path().join(tool);
    fs::write(
        &agent,
        format!(
            "#!/usr/bin/env python3\nimport os, pathlib, subprocess, sys\n{load_command}\n\
             payload = b'{{\"session_id\":\"worker-test\",\"hook_event_name\":\"SessionStart\"}}'\n\
             duplicate = subprocess.check_output([os.environ['MALAK_RECIPE_EXE'], 'hook', '--tool', '{tool}'], input=payload)\n\
             assert duplicate == b'', duplicate\n\
             subprocess.run(command, shell=True, check=True, env={{**os.environ, 'PATH': ''}}, input=payload)\n\
             state = pathlib.Path(os.environ['MALAK_HOOK_STATE'])\n\
             before = {{p: p.read_bytes() for p in state.rglob('*') if p.is_file()}}\n\
             assert before\n\
             prompt = payload.replace(b'SessionStart', b'UserPromptSubmit')\n\
             assert subprocess.check_output([os.environ['MALAK_RECIPE_EXE'], 'hook', '--tool', '{tool}'], input=prompt) == b''\n\
             assert {{p: p.read_bytes() for p in state.rglob('*') if p.is_file()}} == before\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();

    run(board.worker_start(
        &["--brief", brief.to_str().unwrap()],
        &[agent.to_str().unwrap()],
    ));

    let reports = messages(&board.lead(&["read", "lead", "--json"]));
    let inbox = format!("to/{}", board.worker_id());
    let tasks = board.lead(&["read", "--json", &inbox]);
    assert_eq!(messages(&tasks).len(), 1);
    let report: Value = serde_json::from_str(
        reports[0]["text"]
            .as_str()
            .unwrap()
            .strip_prefix("Worker exit status: 0\n\n")
            .unwrap(),
    )
    .unwrap();
    assert!(
        report["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("the brief reached the native hook")
    );
}

struct RunningWorker(Child);

impl Drop for RunningWorker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn worker_start_resumes_when_a_lead_task_arrives_after_exit() {
    let board = Fixture::new();
    board.start();
    let handover_output = r#"printf '%s\n%s' "$MALAK_PARTICIPANT_ID" "$MALAK_HANDOVER""#;
    let mut command = board.worker_start(
        &["--resume", handover_output],
        &["sh", "-c", handover_output],
    );
    recipe_python_startup(
        &board,
        &mut command,
        include_bytes!("fixtures/worker_resume_checkpoint.py"),
    );
    let mut worker = RunningWorker(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let read_reports = || messages(&board.lead(&["read", "lead", "--json", "--wait", "30"]));
    let mut wait_for_report = |description| {
        wait_for_event(description, || {
            let reports = read_reports();
            if !reports.is_empty() {
                return Some(reports);
            }
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited waiting for {description}"
            );
            None
        })
    };
    let first = wait_for_report("the initial worker report");
    assert_eq!(first.len(), 1);
    let initial = &first[0];
    let position = initial["position"].as_u64().unwrap().to_string();
    let creation = initial["creation"].as_str().unwrap();
    board.lead(&["ack", "lead", &position, "--creation", creation]);
    let inbox = format!("to/{}", board.worker_id());
    assert!(
        body(initial)
            .starts_with(format!("Worker exit status: 0\n\n{}\n", board.worker_id()).as_bytes())
    );

    board.lead(&["send", "--kind", "note", &inbox, "A note stays unread"]);
    wait_for_file(&board, "resume-note-checked");
    assert!(messages(&board.lead(&["read", "lead", "--json"])).is_empty());
    board.lead(&["send", "--kind", "task", &inbox, "Resume work"]);

    let reports = wait_for_report("the resumed worker report");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["sender"], initial["sender"]);
    assert_ne!(reports[0]["id"], initial["id"]);
    assert_eq!(body(&reports[0]), body(initial));

    let record = board
        .state
        .join("launches")
        .join(format!("{}.json", board.worker_id()));
    let evidence = wait_for_event("the latest verified worker report", || {
        let value: Value = serde_json::from_slice(&fs::read(&record).ok()?).ok()?;
        (value["last_report"]["id"] == reports[0]["id"]).then_some(value)
    });
    assert_eq!(evidence["report"]["id"], initial["id"]);

    run(board.owner(&["board", "stop"]));
    wait_for_event("worker to stop after the board stops", || {
        worker.0.try_wait().unwrap()
    });
}

#[test]
fn answer_links_a_question_and_preserves_the_reply_body() {
    let board = Fixture::new();
    board.start();
    let question = board.lead(&["send", "--json", "lead", "question"]);
    let receipt: Value = serde_json::from_str(&question).unwrap();
    let creation = receipt["creation"].as_str().unwrap();
    let position = receipt["position"].to_string();
    let reply = b"Try the second option.\nIt's ready.\n";
    let file = board.home.path().join("reply");
    fs::write(&file, reply).unwrap();

    let mut answer = board.recipe("answer");
    answer
        .args(["lead", "lead", creation, &position])
        .stdin(fs::File::open(file).unwrap());
    run(answer);

    let received = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(
        received[1]["headers"]["reply_to"],
        json!(["lead", creation, 1])
    );
    assert_eq!(body(&received[1]), reply);
}

#[test]
fn answer_resolves_a_lead_inbox_position_and_replies_to_its_sender() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["lead:send"]);
    let inbox = format!("to/{}", board.worker_id());
    run(participant_command(&worker, &["send", "lead", "question"]));
    let original = messages(&board.lead(&["get", "lead", "1", "--json"]));
    let reply = b"Try the first option.\n";
    let file = board.home.path().join("reply");
    fs::write(&file, reply).unwrap();

    let mut answer = board.recipe("answer");
    answer.arg("1").stdin(fs::File::open(file).unwrap());
    run(answer);

    let received = messages(&board.lead(&["read", &inbox, "--json"]));
    assert_eq!(
        received[0]["headers"]["reply_to"],
        json!(["lead", original[0]["creation"], 1])
    );
    assert_eq!(received[0]["kind"], "note");
    assert_eq!(body(&received[0]), reply);
}

#[test]
fn answer_task_keeps_its_reply_address_and_trace() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);
    let inbox = format!("to/{}", board.worker_id());
    let question = run(participant_command(
        &worker,
        &["send", "--json", "--trace", "abcd", "questions", "Which?"],
    ));
    let receipt: Value = serde_json::from_str(&question).unwrap();
    let reply = board.home.path().join("reply");
    fs::write(&reply, "Use the first option.\n").unwrap();
    let mut answer = board.recipe("answer");
    answer
        .args(["--task", "questions", "1"])
        .stdin(fs::File::open(reply).unwrap());

    run(answer);

    let received = messages(&board.lead(&["read", &inbox, "--json"]));
    assert_eq!(received[0]["kind"], "task");
    assert_eq!(
        received[0]["headers"]["reply_to"],
        json!(["questions", receipt["creation"], 1])
    );
    assert_eq!(received[0]["headers"]["trace"], "abcd");
    assert_eq!(
        body(&received[0]),
        b"Decided by: owner\nRecorded by: lead\n\nUse the first option.\n"
    );
}

fn relay_board(settings: &str) -> (Fixture, PathBuf) {
    let board = Fixture::new();
    board.configure(settings);
    board.start();
    run(board.worker_start(&[], &["true"]));
    let sender = board.enrol("sender", &["relay/*:send"]);
    (board, sender)
}

#[test]
fn relay_traffic_leaves_the_leads_inbox_count_unchanged() {
    let (board, worker) = relay_board("\n[board.inbox_limits]\nenabled = true\nmax_messages = 1");
    let before = messages(&board.lead(&["read", "lead", "--json"])).len();
    assert_eq!(before, 1);

    run(participant_command(
        &worker,
        &["send", "relay/alice", "For Alice"],
    ));

    assert_eq!(
        messages(&board.lead(&["read", "lead", "--json"])).len(),
        before
    );
}

#[test]
fn a_relay_override_caps_messages_due_to_the_lead_on_that_channel() {
    let (board, worker) = relay_board(
        r#"
[board.inbox_limits]
enabled = true
max_messages = 3

[[board.inbox_limits.inbox]]
pattern = "relay/*"
max_messages = 1
"#,
    );
    run(participant_command(
        &worker,
        &["send", "relay/alice", "first"],
    ));
    let refused = output(participant_command(
        &worker,
        &["send", "relay/alice", "second"],
    ));
    assert_eq!(refused.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("Reason  inbox_full (1 of 1"));

    board.lead(&["read", "relay/alice"]);
    board.lead(&["ack", "relay/alice", "1"]);
    run(participant_command(
        &worker,
        &["send", "relay/alice", "second"],
    ));
}

#[test]
fn forwarding_a_worker_task_delivers_a_linked_note_from_the_lead() {
    let (board, sender) = relay_board("");
    let sender_id = participant(&board.status(), "sender")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let payload = b"Original with \0 and \xff\n";
    let file = board.home.path().join("original");
    fs::write(&file, payload).unwrap();
    let mut send = participant_command(&sender, &["send", "--kind", "task", "relay/alice", "-"]);
    send.stdin(fs::File::open(file).unwrap());
    run(send);
    let original = messages(&board.lead(&["get", "relay/alice", "1", "--json"]));
    assert_eq!(original[0]["kind"], "task");

    let mut forward = board.recipe("forward");
    forward.args(["--board", BOARD, "relay/alice", "1"]);
    run(forward);

    let inbox = format!("to/{}", board.worker_id());
    let received = messages(&board.lead(&["read", &inbox, "--json"]));
    assert_eq!(received.len(), 1);
    let note = &received[0];
    assert_eq!(note["kind"], "note");
    assert_eq!(note["sender"], "lead");
    assert_eq!(
        note["headers"]["reply_to"],
        json!(["relay/alice", original[0]["creation"], 1])
    );
    let mut expected = format!("Forwarded from sender ({sender_id}):\n\n").into_bytes();
    expected.extend_from_slice(payload);
    assert_eq!(body(note), expected);
}

#[test]
fn forward_uses_the_selected_board_for_an_explicit_target() {
    let board = Fixture::new();
    board.start();
    board.enrol("alice", &[]);
    board.lead(&["send", "lead", "For Alice"]);
    let other = Fixture::new();
    other.start();
    other.lead(&["send", "lead", "Other board"]);

    let mut forward = board.recipe("forward");
    forward
        .arg("--config")
        .arg(&board.config)
        .args(["lead", "1", "alice"])
        .env_remove("MALAK_HANDOVER");
    run(forward);

    let inbox = format!("to/{}", board.worker_id());
    let received = messages(&board.lead(&["read", &inbox, "--json"]));
    assert!(body(&received[0]).ends_with(b"For Alice"));
}

#[test]
fn inbox_names_forward_accepts_an_inbox_address() {
    let board = Fixture::new();
    board.start();
    board.enrol("alice", &[]);
    board.lead(&["send", "lead", "For Alice"]);

    let mut forward = board.recipe("forward");
    forward.args(["lead", "1", "to/alice"]);
    run(forward);

    let received = messages(&board.lead(&["get", "to/alice", "1", "--json"]));
    assert!(body(&received[0]).ends_with(b"For Alice"));
}

#[test]
fn inbox_names_worker_start_refuses_new_underscore_names_before_enrolling() {
    let board = Fixture::new();
    board.start();
    let mut start = board.recipe("worker-start");
    start.args(["old_worker", "--no-watcher", "--", "true"]);

    let result = output(start);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 to 24"));
    assert!(
        !board.status()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|person| person["name"] == "old_worker")
    );
}

#[test]
fn inbox_names_guest_refuses_new_underscore_names_before_enrolling() {
    let board = Fixture::new();
    board.start();
    let mut guest = board.recipe("guest");
    guest
        .args(["old_guest", "--out"])
        .arg(board.home.path().join("new-guest"));

    let result = output(guest);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("1 to 24"));
    assert!(
        !board.status()["participants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|person| person["name"] == "old_guest")
    );
}

#[test]
fn forwarding_refuses_a_manager_handover_for_another_board() {
    let parent = Fixture::new();
    parent.start();
    let child = parent.sibling("review", "e");
    child.configure(&format!("parent = {BOARD:?}"));
    child.start();
    child.lead(&["send", "questions", "Private fixture question?"]);
    let other = parent.sibling("other", "f");
    other.start();
    let handover = other.enrol("manager", &["questions:send"]);
    let mut forward = child.recipe("forward");
    forward
        .args(["--config"])
        .arg(&child.config)
        .args(["questions", "1"])
        .env("MALAK_HANDOVER", handover);

    let result = output(forward);

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not match the selected board"));
    assert!(messages(&other.lead(&["read", "--json", "questions"])).is_empty());
}

#[test]
fn the_four_address_answer_refuses_a_mismatched_handover() {
    let board = Fixture::new();
    board.start();
    let receipt: Value =
        serde_json::from_str(&board.lead(&["send", "--json", "questions", "Which?"])).unwrap();
    let other = Fixture::new();
    other.start();
    let file = board.home.path().join("reply");
    fs::write(&file, "The first.\n").unwrap();
    let mut answer = board.recipe("answer");
    answer
        .arg("--config")
        .arg(&board.config)
        .args([
            "questions",
            "questions",
            receipt["creation"].as_str().unwrap(),
            "1",
        ])
        .env("MALAK_HANDOVER", other.state.join("lead.handover"))
        .stdin(fs::File::open(file).unwrap());

    let result = output(answer);

    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not match the selected board"));
    assert!(messages(&other.lead(&["read", "--json", "questions"])).is_empty());
}

#[test]
fn lead_up_refuses_to_bootstrap_a_root_with_another_boards_handover() {
    let board = Fixture::new();
    board.start();
    let other = board.sibling("other", "e");
    let mut setup = other.recipe("lead-up");
    setup
        .arg("--config")
        .arg(&other.config)
        .env("MALAK_HANDOVER", board.state.join("lead.handover"));

    let result = output(setup);

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("does not match the selected board"));
    assert!(!other.state.exists());
}

#[test]
fn forward_refuses_a_revoked_recipient() {
    let board = Fixture::new();
    board.start();
    board.enrol("alice", &[]);
    let id = board.worker_id();
    run(board.owner(&["revoke", &id]));
    board.lead(&["send", "lead", "For Alice"]);

    let mut forward = board.recipe("forward");
    forward.args(["lead", "1", "alice"]);
    let result = output(forward);

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no enrolled participant named"));
    assert!(messages(&board.lead(&["read", &format!("to/{id}"), "--json"])).is_empty());
}

#[test]
fn lead_up_creates_the_questions_channel() {
    let board = Fixture::new();

    board.start();

    let status = board.status();
    let channels = status["channels"].as_array().unwrap();
    assert!(
        channels
            .iter()
            .any(|channel| channel["name"] == "questions")
    );
}

fn ask(board: &Fixture, handover: &Path, arguments: &[&str], question: &str) -> Output {
    let file = board.home.path().join("question");
    fs::write(&file, question).unwrap();
    let mut ask = board.recipe("ask");
    ask.args(arguments)
        .env("MALAK_HANDOVER", handover)
        .stdin(fs::File::open(file).unwrap());
    output(ask)
}

#[test]
fn ask_posts_what_it_blocks_the_options_and_a_recommendation() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);

    let sent = ask(
        &board,
        &worker,
        &[
            "--blocks",
            "the release",
            "--option",
            "ship now",
            "--option",
            "wait",
            "--recommend",
            "wait",
        ],
        "Ship the release?\n",
    );

    assert!(sent.status.success());
    let received = messages(&board.lead(&["read", "questions", "--json"]));
    assert_eq!(received[0]["sender"], board.worker_id());
    assert_eq!(
        body(&received[0]),
        b"Ship the release?\n\nFor: lead\nBlocks: the release\nOptions:\n- ship now\n- wait\nRecommendation: wait\n"
    );
}

#[test]
fn a_worker_can_ask_with_no_python_on_path() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);
    let input = board.home.path().join("question");
    fs::write(&input, "Which?\n").unwrap();
    let mut ask = board.recipe("ask");
    ask.args(["--blocks", "work", "--option", "a", "--recommend", "a"])
        .env("MALAK_HANDOVER", worker)
        .env("PATH", "")
        .stdin(fs::File::open(input).unwrap());

    run(ask);

    assert_eq!(
        messages(&board.lead(&["read", "--json", "questions"])).len(),
        1
    );
}

#[test]
fn ask_can_link_a_task_outside_worker_start() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send", "to/*:get"]);
    let inbox = format!("to/{}", board.worker_id());
    let task: Value = serde_json::from_str(&board.lead(&[
        "send",
        "--json",
        "--kind",
        "task",
        &inbox,
        "Choose a format.",
    ]))
    .unwrap();
    let creation = task["creation"].as_str().unwrap();

    let result = ask(
        &board,
        &worker,
        &[
            "--blocks",
            "work",
            "--option",
            "a",
            "--recommend",
            "a",
            "--reply-to",
            &inbox,
            creation,
            "1",
        ],
        "Which format?\n",
    );

    assert!(result.status.success(), "{result:?}");
    let questions = messages(&board.lead(&["read", "--json", "questions"]));
    assert_eq!(
        questions[0]["headers"]["reply_to"],
        json!([inbox, creation, 1])
    );
    assert_eq!(questions[0]["headers"]["trace"], task["trace"]);
}

#[test]
fn global_config_selects_the_board_for_a_recipe() {
    let default = Fixture::new();
    default.start();
    let selected = default.sibling("selected", "e");
    selected.start();
    let input = default.home.path().join("scope");
    fs::write(&input, "Choose a format.\n").unwrap();
    let mut order = Command::new(MALAK);
    default.recipe_environment(&mut order);
    order
        .arg("--config")
        .arg(&selected.config)
        .args(["recipe", "order", "--for", "5m"])
        .env_remove("MALAK_HANDOVER")
        .stdin(fs::File::open(input).unwrap());

    run(order);

    assert!(messages(&default.lead(&["read", "--json", "orders"])).is_empty());
    let orders = messages(&selected.lead(&["read", "--json", "orders"]));
    assert_eq!(orders.len(), 1);
    assert!(String::from_utf8_lossy(&body(&orders[0])).contains("Choose a format."));
}

#[test]
fn ask_counts_question_lines_after_trimming_trailing_space() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);
    let result = ask(
        &board,
        &worker,
        &["--blocks", "work", "--option", "a", "--recommend", "a"],
        "First? \nSecond?\t\n",
    );

    assert_eq!(result.status.code(), Some(2));
    assert!(messages(&board.lead(&["read", "--json", "questions"])).is_empty());
}

#[test]
fn ask_refuses_empty_input() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);
    let result = ask(
        &board,
        &worker,
        &["--blocks", "work", "--option", "a", "--recommend", "a"],
        " \t\n",
    );

    assert_eq!(result.status.code(), Some(2));
    assert!(messages(&board.lead(&["read", "--json", "questions"])).is_empty());
}

#[test]
fn ask_refuses_a_question_that_does_not_say_what_it_blocks() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);

    let sent = ask(
        &board,
        &worker,
        &["--option", "a", "--recommend", "a"],
        "Which?\n",
    );

    assert_eq!(sent.status.code(), Some(2));
}

#[test]
fn ask_refuses_two_question_lines_without_posting() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);

    let sent = ask(
        &board,
        &worker,
        &[
            "--blocks",
            "release",
            "--option",
            "wait",
            "--recommend",
            "wait",
        ],
        "Ship the release?\n\nWhat about the migration?\n",
    );

    assert_eq!(sent.status.code(), Some(2));
    let error = String::from_utf8(sent.stderr).unwrap();
    assert!(error.contains("one question per message"), "{error}");
    assert!(
        error.contains("Send each question with its own ask"),
        "{error}"
    );
    assert!(messages(&board.lead(&["read", "questions", "--json"])).is_empty());
}

#[test]
fn answer_replies_to_a_question_on_the_questions_channel() {
    let board = Fixture::new();
    board.start();
    let worker = board.enrol("alice", &["questions:send"]);
    run(participant_command(
        &worker,
        &["send", "questions", "Which?"],
    ));
    let inbox = format!("to/{}", board.worker_id());
    let original = messages(&board.lead(&["get", "questions", "1", "--json"]));
    let file = board.home.path().join("reply");
    fs::write(&file, "The first.\n").unwrap();

    let mut answer = board.recipe("answer");
    answer
        .args(["questions", "1"])
        .stdin(fs::File::open(file).unwrap());
    run(answer);

    let received = messages(&board.lead(&["read", &inbox, "--json"]));
    assert_eq!(
        received[0]["headers"]["reply_to"],
        json!(["questions", original[0]["creation"], 1])
    );
    assert_eq!(
        body(&received[0]),
        b"Decided by: owner\nRecorded by: lead\n\nThe first.\n"
    );
}

#[test]
fn answer_posts_an_owner_reply_on_the_questions_channel() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "questions", "Ready to release?"]);
    let original = messages(&board.lead(&["get", "questions", "1", "--json"]));
    let file = board.home.path().join("reply");
    fs::write(&file, "Owner, in chat at 17:46: \"Yes.\"\n").unwrap();

    let mut answer = board.recipe("answer");
    answer
        .args(["questions", "1"])
        .stdin(fs::File::open(file).unwrap());
    run(answer);

    let received = messages(&board.lead(&["read", "questions", "--json"]));
    assert_eq!(received.len(), 2);
    assert_eq!(
        received[1]["headers"]["reply_to"],
        json!(["questions", original[0]["creation"], 1])
    );
}

fn answer_at(board: &Fixture, address: &[&str]) -> String {
    let file = board.home.path().join("reply");
    fs::write(&file, "Use the first option.\n").unwrap();
    let mut answer = board.recipe("answer");
    answer.args(address).stdin(fs::File::open(file).unwrap());
    run(answer)
}

#[test]
fn answer_prefers_the_open_question_when_both_channels_have_the_position() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "Setup note."]);
    board.lead(&["send", "questions", "Which option?"]);
    let question = messages(&board.lead(&["get", "questions", "1", "--json"]));

    let receipt = answer_at(&board, &["1"]);

    assert!(receipt.contains("Answered questions #1."), "{receipt}");
    let received = messages(&board.lead(&["read", "questions", "--json"]));
    assert_eq!(
        received[1]["headers"]["reply_to"],
        json!(["questions", question[0]["creation"], 1])
    );
    assert_eq!(messages(&board.lead(&["read", "lead", "--json"])).len(), 1);
}

#[test]
fn answer_uses_the_named_board_of_an_inherited_lead_handover() {
    let board = Fixture::new();
    board.start();
    let other = board.sibling("named", "e");
    other.start();
    other.lead(&["send", "questions", "Which option on this board?"]);

    let receipt = answer_at(&other, &["1"]);

    assert!(receipt.contains("Answered questions #1."), "{receipt}");
}

#[test]
fn answer_at_a_closed_questions_position_falls_back_to_lead() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "Setup note."]);
    board.lead(&["send", "questions", "Which option?"]);
    answer_at(&board, &["questions", "1"]);

    let receipt = answer_at(&board, &["1"]);

    assert!(receipt.contains("Answered lead #1."), "{receipt}");
}

#[test]
fn answer_with_explicit_lead_leaves_an_open_question_at_the_same_position() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "Setup note."]);
    board.lead(&["send", "questions", "Which option?"]);

    let receipt = answer_at(&board, &["lead", "1"]);

    assert!(receipt.contains("Answered lead #1."), "{receipt}");
    assert_eq!(
        messages(&board.lead(&["read", "questions", "--json"])).len(),
        1
    );
}

#[test]
fn forward_sends_a_question_to_the_parent_boards_questions_channel() {
    let parent = Fixture::new();
    parent.start();
    let manager = parent.enrol("manager", &["questions:send"]);
    let child = parent.sibling("review", "e");
    child.configure(&format!("parent = {BOARD:?}"));
    child.start();
    let asker = child.enrol("alice", &["questions:send"]);
    run(participant_command(
        &asker,
        &["send", "questions", "Which?"],
    ));

    let mut forward = child.recipe("forward");
    forward
        .arg("--config")
        .arg(&child.config)
        .args(["questions", "1"])
        .env("MALAK_HANDOVER", &manager);
    run(forward);

    let received = messages(&parent.lead(&["read", "questions", "--json"]));
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0]["sender"],
        participant(&parent.status(), "manager")["id"]
    );
    assert!(body(&received[0]).ends_with(b"\n\nWhich?"));
}

#[test]
fn lead_up_creates_the_orders_channel() {
    let board = Fixture::new();

    board.start();

    let status = board.status();
    let channels = status["channels"].as_array().unwrap();
    assert!(channels.iter().any(|channel| channel["name"] == "orders"));
}

fn post_order(board: &Fixture, arguments: &[&str], scope: &str) -> Output {
    let file = board.home.path().join("order");
    fs::write(&file, scope).unwrap();
    let mut order = board.recipe("order");
    order.args(arguments).stdin(fs::File::open(file).unwrap());
    output(order)
}

fn order_text(board: &Fixture, position: &str) -> String {
    let order = messages(&board.lead(&["get", "orders", position, "--json"]));
    String::from_utf8(body(&order[0])).unwrap()
}

#[test]
fn order_posts_the_scope_and_ends_with_what_is_never_delegated() {
    let board = Fixture::new();
    board.start();

    stdout(post_order(
        &board,
        &["--until", "2999-01-01T00:00:00Z"],
        "Pick between test fixtures.\n",
    ));

    let text = order_text(&board, "1");
    assert!(text.starts_with("Until: 2999-01-01T00:00:00Z\n\nPick between test fixtures.\n\n"));
    assert!(text.ends_with(
        "- changing the spec\n- deleting data\n- publishing or pushing anything\n\
         - spending money\n- anything that cannot be undone\n"
    ));
}

fn in_hours(hours: i64) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let at =
        time::OffsetDateTime::from_unix_timestamp(i64::try_from(seconds).unwrap() + hours * 3600)
            .unwrap();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        at.month() as u8,
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
    )
}

fn assert_order_end(board: &Fixture, before: &str, after: &str) {
    let text = order_text(board, "1");
    let until = text
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Until: ")
        .unwrap();
    assert!(
        (before..=after).contains(&until),
        "{until} fell outside {before} through {after}"
    );
}

#[test]
fn order_with_no_end_time_ends_8_hours_from_now() {
    let board = Fixture::new();
    board.start();
    let before = in_hours(8);

    stdout(post_order(&board, &[], "Pick between test fixtures."));

    assert_order_end(&board, &before, &in_hours(8));
}

#[test]
fn order_refuses_an_end_time_that_has_passed() {
    let board = Fixture::new();
    board.start();

    let result = post_order(&board, &["--until", "2000-01-01T00:00:00Z"], "Anything.");

    assert!(!result.status.success());
    assert!(messages(&board.lead(&["read", "orders", "--json"])).is_empty());
}

#[test]
fn order_refuses_an_empty_scope() {
    let board = Fixture::new();
    board.start();

    assert!(!post_order(&board, &[], "\n").status.success());
}

#[test]
fn order_for_a_duration_ends_that_long_from_now() {
    let board = Fixture::new();
    board.start();

    let before = in_hours(48);

    stdout(post_order(
        &board,
        &["--for", "2d"],
        "Pick between test fixtures.",
    ));

    assert_order_end(&board, &before, &in_hours(48));
}

#[test]
fn order_refuses_both_until_and_for() {
    let board = Fixture::new();
    board.start();

    let result = post_order(
        &board,
        &["--until", "2999-01-01T00:00:00Z", "--for", "2h"],
        "Anything.",
    );

    assert!(!result.status.success());
    assert!(messages(&board.lead(&["read", "orders", "--json"])).is_empty());
}

#[test]
fn order_refuses_a_malformed_duration() {
    let board = Fixture::new();
    board.start();

    for duration in ["8", "0h", "-1h", "2w", "h"] {
        let result = post_order(&board, &["--for", duration], "Anything.");
        assert!(!result.status.success(), "{duration} was accepted");
    }
    assert!(messages(&board.lead(&["read", "orders", "--json"])).is_empty());
}

#[test]
fn an_order_refuses_a_handover_for_the_wrong_board() {
    let board = Fixture::new();
    board.start();
    let other = Fixture::new();
    other.start();

    let file = board.home.path().join("order");
    fs::write(&file, "Anything.").unwrap();
    let mut order = board.recipe("order");
    order
        .arg("--config")
        .arg(&board.config)
        .stdin(fs::File::open(file).unwrap())
        .env("MALAK_HANDOVER", other.state.join("lead.handover"));
    let result = output(order);

    assert!(!result.status.success());
    assert!(messages(&board.lead(&["read", "orders", "--json"])).is_empty());
    assert!(messages(&other.lead(&["read", "orders", "--json"])).is_empty());
}

#[test]
fn order_selects_a_board_by_name() {
    let board = Fixture::new();
    board.start();

    stdout(post_order(&board, &["--board", BOARD], "Anything."));

    assert_eq!(
        messages(&board.lead(&["read", "orders", "--json"])).len(),
        1
    );
}

#[test]
fn answer_under_an_order_on_the_selected_board() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "question"]);
    board.lead(&["send", "orders", "Until: 2999-01-01T00:00:00Z\n\nAnything."]);
    let other = Fixture::new();
    other.start();

    let file = board.home.path().join("reply");
    fs::write(&file, "The first.\n").unwrap();
    let mut answer = board.recipe("answer");
    answer
        .arg("--config")
        .arg(&board.config)
        .args(["--under-order", "1", "lead", "1"])
        .stdin(fs::File::open(file).unwrap())
        .env_remove("MALAK_HANDOVER");
    stdout(output(answer));

    let received = messages(&board.lead(&["read", "lead", "--json"]));
    assert!(
        String::from_utf8(body(&received[1]))
            .unwrap()
            .starts_with("Under order: ")
    );
    assert!(messages(&other.lead(&["read", "lead", "--json"])).is_empty());
}

fn answer_under(board: &Fixture, order: &str, reply: &str) -> Output {
    let file = board.home.path().join("reply");
    fs::write(&file, reply).unwrap();
    let mut answer = board.recipe("answer");
    answer
        .args(["--under-order", order, "lead", "1"])
        .stdin(fs::File::open(file).unwrap());
    output(answer)
}

#[test]
fn answer_under_an_order_names_it_on_the_first_line() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "question"]);
    stdout(post_order(
        &board,
        &["--until", "2999-01-01T00:00:00Z"],
        "Anything.",
    ));
    let creation = messages(&board.lead(&["get", "orders", "1", "--json"]))[0]["creation"]
        .as_str()
        .unwrap()
        .to_owned();

    stdout(answer_under(&board, "1", "The first.\n"));

    let received = messages(&board.lead(&["read", "lead", "--json"]));
    assert_eq!(
        String::from_utf8(body(&received[1])).unwrap(),
        format!("Under order: {creation} 1\n\nThe first.\n")
    );
}

#[test]
fn answer_under_an_order_refuses_empty_stdin() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "question"]);
    board.lead(&["send", "orders", "Until: 2999-01-01T00:00:00Z\n\nAnything."]);

    let result = answer_under(&board, "1", "");

    let diagnostic = String::from_utf8_lossy(&result.stderr);
    assert_eq!(result.status.code(), Some(2), "{diagnostic}");
    assert!(diagnostic.contains("standard input"), "{diagnostic}");
    assert_eq!(messages(&board.lead(&["read", "lead", "--json"])).len(), 1);
}

#[test]
fn answer_under_an_ended_order_is_refused_and_sends_nothing() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "question"]);
    board.lead(&["send", "orders", "Until: 2000-01-01T00:00:00Z\n\nAnything."]);

    let result = answer_under(&board, "1", "The first.\n");

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("has ended"));
    assert_eq!(messages(&board.lead(&["read", "lead", "--json"])).len(), 1);
}

#[test]
fn answer_under_a_missing_order_is_refused() {
    let board = Fixture::new();
    board.start();
    board.lead(&["send", "lead", "question"]);

    assert!(!answer_under(&board, "7", "The first.\n").status.success());
}

#[test]
fn worker_start_refuses_hostile_names_before_launch_for_each_disk_store() {
    for store in ["markdown", "sqlite"] {
        let board = Fixture::new();
        board.configure(&format!("persistence = {store:?}"));
        board.start();
        let initial = board.status();
        let long = "a".repeat(65);
        for name in ["../lead", "Lead", "worker\u{1b}[2J", &long] {
            let output = board
                .recipe("worker-start")
                .args([name, "--board", BOARD, "--", "false"])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(2), "{store} {name:?}");
            assert!(output.stdout.is_empty());
            assert!(!output.stderr.contains(&0x1b));
        }
        let after = board.status();
        assert_eq!(after["participants"], initial["participants"], "{store}");
        assert_eq!(after["channels"], initial["channels"], "{store}");
    }
}

#[path = "recipes/queue.rs"]
mod queue;

#[path = "recipes/examples.rs"]
mod examples;
#[path = "recipes/guest.rs"]
mod guest;
#[path = "fixtures/terminal.rs"]
mod terminal;

#[path = "recipes/agents.rs"]
mod agents;

#[path = "recipes/board_names.rs"]
mod board_names;

#[test]
fn a_bound_worker_gets_one_claim_notice_and_one_combined_exit_report() {
    let board = Fixture::new();
    run(board.recipe("lead-up"));
    let mut launcher = board.recipe("worker-start");
    launcher.args([
        "bound",
        "--no-watcher",
        "--",
        "sh",
        "-c",
        "malak send lead 'from bound child' && malak send lead 'second command'",
    ]);
    let output = run(launcher);
    assert!(output.contains("Report sent"), "{output}");
    assert_eq!(
        participant(&board.status(), "bound")["claim"]["state"],
        "ended"
    );

    let output = board.lead(&["read", "--json", "lead"]);
    let rows = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    let bodies = rows
        .iter()
        .filter_map(|row| row["text"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.contains("bound is on the board"))
            .count(),
        1
    );
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.contains("bound's session ended"))
            .count(),
        1
    );
    let report = rows.iter().find(|row| row["kind"] == "report").unwrap();
    assert!(
        report["text"]
            .as_str()
            .unwrap()
            .contains("bound's session ended")
    );
    assert!(bodies.contains(&"from bound child"));
    assert!(bodies.contains(&"second command"));
}

mod unread_at_exit {
    use super::*;

    const PRINT_DONE: [&str; 3] = ["sh", "-c", "printf done"];

    fn report_body(board: &Fixture) -> String {
        let reports = messages(&board.lead(&["read", "lead", "--json"]));
        let report = reports.iter().find(|row| row["kind"] == "report").unwrap();
        String::from_utf8(body(report)).unwrap()
    }

    fn bound_worker(board: &Fixture, brief: bool) {
        let mut launcher = board.recipe("worker-start");
        launcher.args(["alice", "--board", BOARD, "--no-watcher"]);
        if brief {
            fs::write(board.home.path().join("brief"), "Do the work.\n").unwrap();
            launcher.args(["--brief", "brief"]);
        }
        launcher.arg("--").args(PRINT_DONE);
        run(launcher);
    }

    #[test]
    fn a_bound_workers_exit_report_counts_the_brief_it_never_acknowledged() {
        let board = Fixture::new();
        board.start();

        bound_worker(&board, true);

        let report = report_body(&board);
        assert!(
            report.starts_with("Worker exit status: 0, 1 unread (lead #1)\n\n"),
            "{report}"
        );
    }

    #[test]
    fn a_bound_workers_exit_report_is_unchanged_with_an_empty_inbox() {
        let board = Fixture::new();
        board.start();

        bound_worker(&board, false);

        let report = report_body(&board);
        assert!(!report.contains("unread"), "{report}");
    }

    #[test]
    fn an_unbound_workers_exit_report_is_unchanged_by_an_unread_brief() {
        let board = Fixture::new();
        board.start();
        fs::write(board.home.path().join("brief"), "Do the work.\n").unwrap();

        run(board.worker_start(&["--brief", "brief"], &PRINT_DONE));

        assert_eq!(report_body(&board), "Worker exit status: 0\n\ndone");
    }

    #[test]
    fn workers_shows_the_unread_count_after_the_worker_exits() {
        let board = Fixture::new();
        board.start();
        bound_worker(&board, true);

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD]);
        let text = run(workers);

        assert!(text.contains("Unread"), "{text}");
        assert!(text.contains("1 unread (lead #1)"), "{text}");
    }

    #[test]
    fn workers_json_carries_the_unread_words_of_an_exited_worker() {
        let board = Fixture::new();
        board.start();
        bound_worker(&board, true);

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD, "--json"]);
        let rows: Value = serde_json::from_str(&run(workers)).unwrap();

        assert_eq!(rows["workers"][0]["unread"], "1 unread (lead #1)");
    }

    #[test]
    fn workers_names_the_worker_start_command_once_a_bound_session_ends() {
        let board = Fixture::new();
        board.start();
        bound_worker(&board, false);

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD]);
        let text = run(workers);

        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            words.contains("Re-issue malak recipe worker-start alice --again"),
            "{text}"
        );
    }

    #[test]
    fn workers_names_no_command_for_a_session_that_never_ended() {
        let board = Fixture::new();
        board.start();
        run(board.worker_start(&[], &PRINT_DONE));

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD]);

        assert!(!run(workers).contains("Re-issue"));
    }

    /// Enrol alice bound to this test, revoke her, and enrol her again.
    /// Returns the old and the new inbox.
    fn relaunched_alice(board: &Fixture) -> (String, String) {
        let enrol = |handover: &str| {
            let session = std::process::id().to_string();
            let mut command = board.owner(&["enrol", "alice", "--json", "--session", &session]);
            command.arg("--out").arg(board.home.path().join(handover));
            let receipt: Value = serde_json::from_str(&run(command)).unwrap();
            format!("to/{}", receipt["id"].as_str().unwrap())
        };
        let old = enrol("alice-old");
        run(board.owner(&["revoke", "alice"]));
        (old, enrol("alice-new"))
    }

    #[test]
    fn workers_name_shows_only_the_active_enrolment_by_default() {
        let board = Fixture::new();
        board.start();
        let (old, new) = relaunched_alice(&board);

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD]);
        let text = run(workers);

        assert!(text.contains(&new), "{text}");
        assert!(!text.contains(&old), "{text}");
    }

    #[test]
    fn workers_name_all_lists_the_active_enrolment_first() {
        let board = Fixture::new();
        board.start();
        let (old, new) = relaunched_alice(&board);

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--all", "--board", BOARD]);
        let text = run(workers);

        assert!(
            text.find(&new).unwrap() < text.find(&old).unwrap(),
            "{text}"
        );
    }

    #[test]
    fn workers_without_a_name_shows_each_names_active_enrolment() {
        let board = Fixture::new();
        board.start();
        let (old, new) = relaunched_alice(&board);

        let mut workers = board.recipe("workers");
        workers.args(["--board", BOARD]);
        let text = run(workers);

        assert!(text.contains(&new) && !text.contains(&old), "{text}");
    }

    #[test]
    fn workers_json_lists_the_active_enrolment_first_and_flags_it() {
        let board = Fixture::new();
        board.start();
        let (old, new) = relaunched_alice(&board);

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD, "--json"]);
        let rows: Value = serde_json::from_str(&run(workers)).unwrap();

        let rows = rows["workers"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row["inbox"].as_str().unwrap(), row["active"].as_bool()))
                .collect::<Vec<_>>(),
            [(new.as_str(), Some(true)), (old.as_str(), Some(false))]
        );
    }

    #[test]
    fn workers_shows_no_unread_line_for_an_empty_inbox() {
        let board = Fixture::new();
        board.start();
        run(board.worker_start(&[], &PRINT_DONE));

        let mut workers = board.recipe("workers");
        workers.args(["alice", "--board", BOARD]);

        assert!(!run(workers).contains("Unread"));
    }
}

#[test]
fn a_failed_launch_leaves_no_bound_claim_and_a_revoked_enrolment() {
    let board = Fixture::new();
    board.start();
    let brief = board.home.path().join("brief");
    // A brief over the body limit fails the launch after the claim is issued.
    fs::write(&brief, vec![b'x'; 40 << 20]).unwrap();
    let mut launch = board.recipe("worker-start");
    launch
        .args(["alice", "--board", BOARD, "--no-watcher", "--brief"])
        .arg(&brief)
        .args(["--", "sleep", "600"]);
    let failed = output(launch);
    assert!(!failed.status.success());

    let mut workers = board.recipe("workers");
    workers.args(["alice", "--board", BOARD, "--json"]);
    let listing: Value = serde_json::from_str(&run(workers)).unwrap();

    let worker = &listing["workers"][0];
    assert_eq!(worker["enrolment"], "revoked");
    assert_eq!(worker["claim"]["state"], "ended");
}
