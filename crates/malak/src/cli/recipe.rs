use super::{
    board_group, board_list, board_names, config, config::io_error, examples, output, participant,
};
use malak::Error;
use malak_fs::{Directory, check_user_permissions, trusted_parent};
use nix::errno::Errno;
use nix::fcntl::OFlag;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, geteuid};
use signal_hook::SigId;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Permissions};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    process::{CommandExt, ExitStatusExt},
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::Duration;

mod quiet;

const CHECK_BOARD: &str = "__check-board";
const BOARD_CONFIG: &str = "__board-config";
const BOARD_NAME: &str = "__board-name";
const BOARD_LIST: &str = "__board-list";
const REGISTER_GUEST: &str = "__register-guest";
const GUEST_REPLIES: &str = "__guest-replies";
const SESSION_PROCESS: &str = "__session-process";
const WORKER_EXIT: &str = "__worker-exit";
const WORKER_STARTED: &str = "malak-worker-started\n";
pub(super) const BUNDLE_TEMP_PREFIX: &str = "malak-recipe";
pub(super) const WORKER_HOOK_TEMP_PREFIX: &str = "malak-worker-hooks";
pub(super) const BUNDLE_FOLDER: &str = "bundle";
// Cargo packages this link as a copy of the guide for installed binaries.
pub(super) const AGENT_SETUP: &str = include_str!("../../agent-setup.md");
const REFRESHED_HOOKS: u8 = 10; // Private recipe status.
const REFRESH_PENDING: u8 = 11;

#[derive(clap::Args)]
pub(super) struct Args {
    /// Recipe name followed by arguments passed directly to it.
    #[arg(
        value_name = "NAME [ARGS...]",
        trailing_var_arg = true,
        allow_hyphen_values = true
    )]
    words: Vec<OsString>,
}

impl Args {
    pub(super) fn shows_help(&self) -> bool {
        self.words.split_first().is_some_and(|(name, words)| {
            name.to_str()
                .is_some_and(|name| help_before_recipe_position(name, words))
        })
    }

    pub(super) fn is_demo(&self) -> bool {
        self.words.first().is_some_and(|name| name == "demo")
    }

    pub(super) fn operation(&self) -> String {
        match self.words.first() {
            Some(name) if name == CHECK_BOARD => "check the recipe handover".to_owned(),
            Some(name) if name == BOARD_CONFIG || name == BOARD_NAME || name == BOARD_LIST => {
                "select the recipe board".to_owned()
            }
            Some(name) => format!("run recipe {}", name.to_string_lossy()),
            None => "list the shipped recipes".to_owned(),
        }
    }
}

struct Script {
    file: &'static str,
    bytes: &'static [u8],
}

macro_rules! script {
    ($file:literal) => {
        Script {
            file: $file,
            bytes: include_bytes!(concat!("../../recipes/", $file)),
        }
    };
}

const AGENTS: (&str, &str, Script) = (
    "agents",
    "Install or remove the Malak agent skill and reminders.",
    script!("agents"),
);

const RECIPES: &[(&str, &str, Script)] = &[
    (
        "adopt",
        "Bring a running Claude Code or Codex session onto the board.",
        script!("adopt"),
    ),
    AGENTS,
    (
        "agents-md",
        "Add Malak instructions to a project.",
        script!("agents-md"),
    ),
    (
        "answer",
        "Reply to a question or assign follow-up work.",
        script!("answer"),
    ),
    (
        "ask",
        "Post a question with options and a recommendation.",
        script!("ask"),
    ),
    (
        "await",
        "Wait for a worker's verified final report.",
        script!("await"),
    ),
    (
        "demo",
        "Run a lead and five workers on a disposable board.",
        script!("demo"),
    ),
    (
        "forward",
        "Pass a message through the lead with its source.",
        script!("forward"),
    ),
    (
        "guest",
        "Enrol a cross-board guest with a reply inbox.",
        script!("guest"),
    ),
    (
        "handoff",
        "Pass a task or part of it to another agent.",
        script!("handoff"),
    ),
    (
        "lead-up",
        "Set up an agent chat to become or take over as the lead.",
        script!("lead-up"),
    ),
    (
        "observe",
        "Show the Malak reminder in an agent session.",
        script!("observe"),
    ),
    (
        "order",
        "Record what the lead can decide in your absence.",
        script!("order"),
    ),
    (
        "queue",
        "Start a worker after another worker's verified report.",
        script!("queue"),
    ),
    (
        "worker-start",
        "Enrol and launch a worker with its inbox and hooks.",
        script!("worker-start"),
    ),
    (
        "workers",
        "Show launched and adopted workers.",
        script!("workers"),
    ),
    (
        "wrapper",
        "Run a command and post its result as a worker.",
        script!("wrapper.sh"),
    ),
];

const SUPPORT: &[Script] = &[
    script!("adapter_claude.py"),
    script!("adapter_codex.py"),
    script!("adapter_codex_notice.py"),
    script!("adapter_durative.py"),
    script!("adapter_generic.py"),
    script!("adapters.py"),
    script!("agent_settings.py"),
    script!("agent_launch.py"),
    script!("board.sh"),
    script!("board_data.py"),
    script!("delegation.py"),
    script!("demo.py"),
    script!("demo_board.py"),
    script!("hook_format.py"),
    script!("lead_status.py"),
    script!("lead_turn_end.py"),
    script!("malak-skill.md"),
    script!("question_hooks.py"),
    script!("recipe_board.py"),
    script!("recipe_output.py"),
    script!("report.sh"),
    script!("worker.py"),
    script!("worker_queue.py"),
];

pub(super) fn bundle_file(name: &str) -> bool {
    SUPPORT.iter().any(|script| script.file == name)
        || RECIPES.iter().any(|(_, _, script)| script.file == name)
}

pub(super) fn is_shipped(name: &OsStr) -> bool {
    RECIPES.iter().any(|(known, _, _)| name == *known)
}

pub(super) fn run(args: Args, config: Option<PathBuf>) -> Result<ExitCode, Error> {
    let Some((name, arguments)) = args.words.split_first() else {
        let format = output::Format::stdout();
        if format.interactive() {
            let mut screen = format.details("Malak recipes", &[]);
            let descriptions: Vec<_> = RECIPES
                .iter()
                .map(|(name, description, _)| (*name, *description))
                .collect();
            format.screen_fields(&mut screen, &descriptions);
            format.next(&mut screen, "malak recipe NAME --help");
            output::write(&screen)?;
        } else {
            for (name, _, _) in RECIPES {
                output::line(format_args!("{name}"))?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    };
    match name.to_str() {
        Some(REGISTER_GUEST) => {
            let [board, handover] = arguments else {
                return Err(Error::refused(
                    "supply the receiving board name and guest handover",
                ));
            };
            let board = board
                .to_str()
                .ok_or_else(|| Error::refused("use a UTF-8 board name"))?;
            super::guest_inbox::register(config, board, Path::new(handover))?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(GUEST_REPLIES) if arguments.is_empty() => {
            let records: Vec<_> = super::guest_inbox::for_lead(&config)
                .into_iter()
                .flat_map(|guest| guest.read(None))
                .map(|record| participant::json_message(record.label, &record.body))
                .collect();
            output::line(format_args!("{}", serde_json::Value::Array(records)))?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(CHECK_BOARD) if arguments.is_empty() => {
            participant::check_recipe_handover(config)?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(WORKER_EXIT) => {
            let [id, status, path, trace] = arguments else {
                return Err(Error::refused("invalid worker exit report"));
            };
            let utf8 = |arg: &std::ffi::OsStr| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::refused("worker exit report requires UTF-8 arguments"))
            };
            let [id, status, path, trace] = [utf8(id)?, utf8(status)?, utf8(path)?, utf8(trace)?];
            let report = (|| {
                let mut owner = super::owner(config)?;
                super::owner_commands::worker_exit(&mut owner, &id, &status, &path, &trace)
            })();
            if let Err(ref error) = report
                && (participant::report_connection_failed(error)
                    || error.outcome() == malak::Outcome::Unavailable)
            {
                eprintln!("{error}");
                return Ok(ExitCode::from(participant::REPORT_CONNECTION_EXIT));
            } else {
                report?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(SESSION_PROCESS) if arguments.is_empty() => {
            if let Some(process) = malak_transport_unix::local_session_process() {
                output::line(format_args!(
                    "{}",
                    serde_json::json!({
                        "pid": process.identity.pid,
                        "start_time": process.identity.start_time,
                    })
                ))?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(SESSION_PROCESS) => {
            let [pid] = arguments else {
                return Err(Error::refused("supply one session PID"));
            };
            let pid = pid
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|&pid| pid != 0)
                .ok_or_else(|| Error::refused("supply a positive session PID"))?;
            if let Some(session) = malak_transport_unix::session_at(pid) {
                let process = session.process;
                let details = serde_json::json!({
                    "pid": process.identity.pid,
                    "start_time": process.identity.start_time,
                    "program": process.program,
                    "cwd": session.folder,
                });
                output::line(format_args!("{details}"))?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(BOARD_CONFIG) => {
            let (name, create) = match arguments {
                [name] => (name, false),
                [name, mode] if mode == "--create" => (name, true),
                _ => return Err(Error::refused("supply one board name")),
            };
            let name = name
                .to_str()
                .ok_or_else(|| Error::refused("board name must be UTF-8"))?;
            let path = match board_names::resolve(name) {
                Err(board_names::ResolveError::Unknown(_)) if create => create_named_board(name)?,
                result => result?,
            };
            let path = path
                .to_str()
                .ok_or_else(|| Error::refused("board configuration path must be UTF-8"))?;
            output::line(format_args!("{path}"))?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(BOARD_NAME) if arguments.is_empty() => {
            let path = match config {
                Some(path) => std::path::absolute(path).map_err(io_error)?,
                None => config::default_path()?,
            };
            let name = board_names::name(&board_group::Entry::read(path));
            output::line(format_args!("{name}"))?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(BOARD_LIST) if arguments.is_empty() => {
            let entries: Vec<_> = board_names::list()?
                .into_iter()
                .map(|entry| (board_names::name(&entry), entry.path))
                .collect();
            let json = serde_json::to_string(&entries).map_err(|error| io_error(error.into()))?;
            output::line(format_args!("{json}"))?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => (),
    }
    let (name, description, script) = RECIPES
        .iter()
        .find(|(known, _, _)| name == *known)
        .ok_or_else(|| {
            let names: Vec<_> = RECIPES.iter().map(|(name, _, _)| *name).collect();
            Error::refused(format!(
                "unknown shipped recipe. Choose from {}.",
                names.join(", ")
            ))
        })?;
    if *name == "observe" && arguments.first().is_some_and(|first| first == "--reminder") {
        return Ok(observe_reminder(
            name,
            description,
            script,
            arguments,
            config.as_deref(),
        ));
    }
    launch(
        name,
        description,
        script,
        arguments,
        config.as_deref(),
        None,
    )
    .map_err(io_error)
}

/// The command Malak installs for every Claude Code and Codex hook event.
///
/// Both tools read exit code 2 as "block this call", so nothing here may fail
/// loudly: an event with nothing to say answers before the recipe is unpacked,
/// and any failure of the recipe or the board ends quietly with success.
fn observe_reminder(
    name: &str,
    description: &str,
    script: &Script,
    arguments: &[OsString],
    config: Option<&Path>,
) -> ExitCode {
    let mut input = Vec::new();
    if io::stdin().lock().read_to_end(&mut input).is_ok() && !quiet::nothing_to_say(&input) {
        let _ = launch(name, description, script, arguments, config, Some(&input));
    }
    ExitCode::SUCCESS
}

fn create_named_board(name: &str) -> Result<PathBuf, Error> {
    if matches!(name, "" | "." | "..") || name.contains('/') {
        return Err(Error::refused("use one board name without a slash"));
    }
    let folder = board_list::directory()?.join(name);
    config::create_named_board(&folder)?;
    // Another setup may have created the config after the first lookup.
    Ok(board_names::resolve(name)?)
}

pub(super) fn agents_installed(root: &Path) -> Result<bool, Error> {
    match root.join("agents.json").symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

pub(super) fn agents_pending(root: &Path) -> Result<bool, Error> {
    refresh_agent_integration(root, true)
}

pub(super) fn refresh_agents(root: &Path) -> Result<bool, Error> {
    refresh_agent_integration(root, false)
}

fn refresh_agent_integration(root: &Path, dry_run: bool) -> Result<bool, Error> {
    if !agents_installed(root)? {
        return Ok(false);
    }
    let (name, description, script) = &AGENTS;
    let mut args = vec!["--yes".into(), "--upgrade-refresh".into()];
    let expected = if dry_run {
        args.push("--dry-run".into());
        REFRESH_PENDING
    } else {
        REFRESHED_HOOKS
    };
    match launch(name, description, script, &args, None, None).map_err(io_error)? {
        status if status == ExitCode::SUCCESS => Ok(false),
        status if status == ExitCode::from(expected) => Ok(true),
        _ => Err(Error::unavailable("the Malak agents recipe failed")),
    }
}

pub(super) fn notify_upgrade(root: &Path, notice: &str) -> Result<(), Error> {
    if !agents_installed(root)? {
        return Ok(());
    }
    let (name, description, script) = &AGENTS;
    let args = [
        "--yes".into(),
        "--notify-open".into(),
        format!("--upgrade-notice={notice}").into(),
    ];
    if launch(name, description, script, &args, None, None).map_err(io_error)? != ExitCode::SUCCESS
    {
        return Err(Error::unavailable("the Malak agents recipe failed"));
    }
    Ok(())
}

/// Ask open Codex threads to reread their hooks after an upgrade changed them.
pub(super) fn reload_codex_hooks(root: &Path) -> Result<(), Error> {
    if !agents_installed(root)? {
        return Ok(());
    }
    let (name, description, script) = &AGENTS;
    let args = ["--yes".into(), "--upgrade-reload".into()];
    if launch(name, description, script, &args, None, None).map_err(io_error)? != ExitCode::SUCCESS
    {
        return Err(Error::unavailable("the Malak agents recipe failed"));
    }
    Ok(())
}

pub(super) fn remove_agents(home: &Path, dry_run: bool) -> Result<(), Error> {
    if !agents_installed(&home.join(".malak"))? {
        return Ok(());
    }
    let (name, description, script) = &AGENTS;
    let mut args = vec!["--remove".into(), "--yes".into()];
    if dry_run {
        args.push("--dry-run".into());
    }
    let status = launch(name, description, script, &args, None, None).map_err(io_error)?;
    if status != ExitCode::SUCCESS {
        return Err(Error::unavailable(
            "could not remove the Malak agent integration",
        ));
    }
    Ok(())
}

fn help_before_recipe_position(name: &str, arguments: &[OsString]) -> bool {
    let mut words = arguments.iter();
    while let Some(word) = words.next().and_then(|word| word.to_str()) {
        match word {
            "help" | "-h" | "--help" => return true,
            "--" => break,
            _ => {}
        }
        // The recipe parses its remaining arguments. Skip option values here
        // so a message, path or board name equal to "help" remains a value.
        let values: usize = match (name, word) {
            ("agents", "--yes" | "--dry-run" | "--remove" | "--notify-open")
            | ("agents-md", "--remove")
            | ("answer", "--task")
            | ("demo", "--yes" | "--keep")
            | ("guest", "--json")
            | ("lead-up", "--list" | "--tree")
            | ("queue", "--prepared" | "--no-watcher")
            | (
                "worker-start",
                "--again" | "--prepared" | "--detach" | "--no-watcher" | "--unbound",
            )
            | ("workers", "--json") => 0,
            ("ask", "--reply-to") => 3,
            (_, option) if option.starts_with("--") && option.contains('=') => 0,
            (_, option) if option.starts_with("--") => 1,
            _ => break,
        };
        if values > 0 {
            words.nth(values - 1);
        }
    }
    false
}

fn launch(
    name: &str,
    description: &str,
    script: &Script,
    arguments: &[OsString],
    config: Option<&Path>,
    // A hook payload already read from standard input. The recipe gets it as its input
    // and its error output is dropped.
    hook_input: Option<&[u8]>,
) -> io::Result<ExitCode> {
    let signals = RecipeSignals::register()?;
    let mut bundle = TemporaryDirectory::new(BUNDLE_TEMP_PREFIX)?;
    let (files, _) = bundle.opened.child_or_create(BUNDLE_FOLDER.as_ref())?;
    check_user_permissions(files.file(), geteuid().as_raw(), true)?;
    for file in SUPPORT.iter().chain(std::iter::once(script)) {
        let mut extracted =
            files.open_file(file.file, OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL)?;
        extracted.write_all(file.bytes)?;
        extracted.set_permissions(Permissions::from_mode(0o700))?;
    }

    let executable = env::current_exe()?.canonicalize()?;
    let bin = executable.parent().unwrap_or_else(|| Path::new("/"));
    let search_path = env::var_os("PATH").unwrap_or_default();
    let search_path =
        env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&search_path)))
            .map_err(io::Error::other)?;
    let help = ["--help".into()];
    let arguments = if help_before_recipe_position(name, arguments) {
        &help[..]
    } else {
        arguments
    };
    let mut command = Command::new(bundle.path.join(BUNDLE_FOLDER).join(script.file));
    command
        .env("MALAK_RECIPE_EXE", executable)
        .env(
            "MALAK_RECIPE_CONFIG",
            config.unwrap_or_else(|| Path::new("")),
        )
        .env("PATH", search_path)
        .args(arguments);
    command
        .env_remove("MALAK_RECIPE_EXAMPLE")
        .env_remove("MALAK_RECIPE_DESCRIPTION");
    let help = arguments
        .iter()
        .take_while(|argument| *argument != "--")
        .any(|argument| matches!(argument.to_str(), Some("-h" | "--help")));
    if help {
        command
            .env(
                "MALAK_RECIPE_EXAMPLE",
                examples::snippet(&format!("recipe {name}")),
            )
            .env("MALAK_RECIPE_DESCRIPTION", description);
    }
    // The stock worker runs as this account. A launcher that changes accounts
    // owns its worker's state and can clean it up independently of the bundle.
    let mut hook_state = matches!(script.file, "worker-start" | "queue")
        .then(|| TemporaryDirectory::new(WORKER_HOOK_TEMP_PREFIX))
        .transpose()?;
    if let Some(state) = &hook_state {
        command.env("MALAK_RECIPE_HOOK_STATE", &state.path);
    }
    if script.file == "queue" {
        command.env("MALAK_QUEUE_PARENT_PID", std::process::id().to_string());
    }
    command.env_remove("MALAK_RECIPE_DETACHED_BUNDLE");
    let detached = script.file == "worker-start"
        && !help
        && arguments
            .iter()
            .take_while(|arg| *arg != "--")
            .any(|arg| arg == "--detach");
    let status = if detached {
        let state = hook_state.as_mut().expect("worker hook state");
        match detach_worker(&mut command, &mut bundle, state, &signals)? {
            Some(status) => status,
            None => return Ok(ExitCode::SUCCESS),
        }
    } else {
        if hook_input.is_some() {
            command.stdin(Stdio::piped()).stderr(Stdio::null());
        }
        let mut child = command.spawn()?;
        let feeder = hook_input
            .zip(child.stdin.take())
            .map(|(input, mut stdin)| {
                let input = input.to_vec();
                // A recipe that exits early leaves a broken pipe, which is not an error here.
                thread::spawn(move || stdin.write_all(&input))
            });
        let status = signals.wait(&mut child, false)?;
        if let Some(feeder) = feeder {
            let _ = feeder.join();
        }
        status
    };
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(3)))
}

fn detach_worker(
    command: &mut Command,
    bundle: &mut TemporaryDirectory,
    hook_state: &mut TemporaryDirectory,
    signals: &RecipeSignals,
) -> io::Result<Option<ExitStatus>> {
    let mut child = command
        .env("MALAK_RECIPE_DETACHED_BUNDLE", &bundle.path)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped worker readiness");
    let mut readiness = Vec::new();
    let mut bytes = [0; 4096];
    loop {
        signals.forward(&child, true)?;
        let mut descriptors = [PollFd::new(stdout.as_fd(), PollFlags::POLLIN)];
        match poll(&mut descriptors, 20u16) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) => (),
            Err(error) => return Err(error.into()),
        }
        let count = stdout.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        readiness.extend_from_slice(&bytes[..count]);
    }
    signals.forward(&child, true)?;
    let readiness = String::from_utf8(readiness).map_err(io::Error::other)?;
    if let Some(details) = readiness.strip_prefix(WORKER_STARTED) {
        // The launcher now owns both paths, including across resume runs.
        bundle.remove_on_drop = false;
        hook_state.remove_on_drop = false;
        io::stdout().lock().write_all(details.as_bytes())?;
        Ok(None)
    } else {
        io::stdout().lock().write_all(readiness.as_bytes())?;
        signals.wait(&mut child, true).map(Some)
    }
}

// Keep the handlers installed until the temporary-directory guards have run.
struct RecipeSignals {
    pending: Arc<AtomicUsize>,
    handlers: Vec<SigId>,
}

impl RecipeSignals {
    fn register() -> io::Result<Self> {
        let mut signals = Self {
            pending: Arc::new(AtomicUsize::new(0)),
            handlers: Vec::new(),
        };
        for signal in [Signal::SIGINT, Signal::SIGTERM] {
            signals.handlers.push(signal_hook::flag::register_usize(
                signal as i32,
                Arc::clone(&signals.pending),
                signal as usize,
            )?);
        }
        Ok(signals)
    }

    fn forward(&self, child: &Child, detached: bool) -> io::Result<()> {
        if let Ok(signal) = Signal::try_from(self.pending.swap(0, Ordering::Relaxed) as i32) {
            let pid = Pid::from_raw(child.id() as i32);
            let result = if detached {
                killpg(pid, signal)
            } else {
                kill(pid, signal)
            };
            match result {
                Ok(()) | Err(Errno::ESRCH) => (),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn wait(&self, child: &mut Child, detached: bool) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            self.forward(child, detached)?;
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RecipeSignals {
    fn drop(&mut self) {
        for handler in self.handlers.drain(..) {
            signal_hook::low_level::unregister(handler);
        }
    }
}

struct TemporaryDirectory {
    path: PathBuf,
    opened: Directory,
    remove_on_drop: bool,
}

impl TemporaryDirectory {
    fn new(prefix: &str) -> io::Result<Self> {
        let uid = geteuid().as_raw();
        let create = |base: PathBuf| -> io::Result<Self> {
            let template = base.canonicalize()?.join(format!("{prefix}.XXXXXXXX"));
            trusted_parent(&template, uid)?;
            let path = nix::unistd::mkdtemp(&template)?;
            let open = || {
                let opened = Directory::open(&path)?;
                // Inspect inherited ACLs on the opened directory before extraction.
                check_user_permissions(opened.file(), uid, true)?;
                if opened.file().metadata()?.mode() & 0o7777 != 0o700 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "temporary directory must have mode 0700",
                    ));
                }
                Ok(opened)
            };
            match open() {
                Ok(opened) => Ok(Self {
                    path,
                    opened,
                    remove_on_drop: true,
                }),
                Err(error) => {
                    let _ = fs::remove_dir_all(path);
                    Err(error)
                }
            }
        };
        create(env::temp_dir())
            .or_else(|_| create(env::home_dir().ok_or(io::ErrorKind::PermissionDenied)?))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("no trusted writable temporary base. Set TMPDIR to a private writable directory. {error}"),
                )
            })
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
