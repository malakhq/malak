use super::{
    board, board_group, board_list, board_names, board_version, cargo_install, config, output,
    owner_commands, owner_for_board_quiet, recipe, user_id,
};
use clap::Args;
use malak::Error;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

mod notice;
mod source;
mod workers;
use workers::Worker;

pub(super) const FINISH: &str = "__upgrade-finish";
const HANDOFF: &str = "malak upgrade continuation 1";
const NOTICE_HANDOFF: &str = "malak upgrade continuation 2";
const HOOK_REVIEW: &str = "For chats opened before this upgrade, restart Claude Code once to load the new hook set. Open Codex threads reload without a restart where the app-server answers; either way, review and approve the Malak hooks in Codex /hooks or Settings > Hooks to receive reminders, record owner questions and answers, see delegation reminders and hand over pending work at turn end. Malak never approves its own hooks.";

#[derive(Args)]
pub(super) struct Options {
    /// Install from a local Malak checkout instead of a release tag.
    #[arg(long, value_name = "DIR", conflicts_with = "version")]
    path: Option<PathBuf>,
    /// Install this release instead of the latest vX.Y.Z tag.
    #[arg(long, value_name = "X.Y.Z")]
    version: Option<String>,
    /// Show the plan without installing or restarting anything.
    #[arg(long)]
    dry_run: bool,
    /// Accepted for compatibility. Upgrades run without a prompt.
    #[arg(long = "yes")]
    _yes: bool,
    /// Allow installation of an older version.
    #[arg(long)]
    force: bool,
}

#[derive(Deserialize, Serialize)]
struct Restart {
    config: PathBuf,
    name: String,
    run: String,
    workers: Vec<Worker>,
    #[serde(skip)]
    outdated: bool,
}

struct Restarted<'a> {
    name: &'a str,
    run: String,
    previous: Option<String>,
    client: owner_commands::Client,
}

#[derive(Default)]
struct Inventory {
    running: Vec<Restart>,
    stopped: Vec<String>,
}

enum BoardState {
    Stopped,
    Current,
    Restart(Restart),
}

enum Completion {
    Pending,
    Legacy,
    Upgrade(Option<notice::Previous>),
}

pub(super) fn run(explicit: Option<PathBuf>, options: Options) -> Result<ExitCode, Error> {
    let source = source::Source::find(options.path, options.version)?;
    let current = source::Version::parse(board_version::VERSION)?;
    let target = source.version();
    if target < current && !options.force {
        return Err(Error::refused(format!(
            "refusing to downgrade Malak from {current} to {target}; use --force to allow it"
        )));
    }
    let installing = source.local() || target != current;
    let root = board_list::directory()?;
    let mut boards = collect_boards(&root, explicit, !installing)?;
    let refresh_agents = if installing {
        recipe::agents_installed(&root)?
    } else {
        agents_need_attention(&root)
    };
    if !installing && boards.running.is_empty() && !refresh_agents {
        return up_to_date();
    }
    output::details(
        "Malak upgrade plan",
        &[
            ("Source", &source.description()),
            ("Version", &format!("{current} -> {target}")),
            (
                "Cargo",
                if installing {
                    "Install the target build."
                } else {
                    "The target version is installed."
                },
            ),
            (
                "Agent integration",
                if refresh_agents {
                    "Refresh the installed skill and reminders if they differ."
                } else {
                    "No installed skill and reminders to refresh."
                },
            ),
        ],
    )?;
    if source.local() {
        output::result("Cargo must finish installing before the local build IDs can be compared.")?;
    }
    show_boards(&boards)?;
    if options.dry_run {
        if installing {
            show_notice_preview(&source)?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(config::io_error)?;
    let previous = board_version::build_at(&exe).map_err(config::io_error)?;
    if !installing {
        return finish_pending(&exe, &previous, refresh_agents, &boards.running);
    }
    let (install_root, cargo) = cargo_install::cargo_for(&exe, &cargo_install::search_path())
        .filter(|_| exe.file_name().is_some_and(|name| name == "malak"))
        .ok_or_else(|| Error::refused("could not find Cargo for this Malak install root"))?;
    source.install(&install_root, &cargo)?;
    let installed = board_version::build_at(&exe).map_err(config::io_error)?;
    if installed == previous {
        boards.running.retain(|board| board.outdated);
        let refresh = refresh_agents && agents_need_attention(&root);
        return finish_pending(&exe, &installed, refresh, &boards.running);
    }
    let notices = supports_handoff(&exe, "probe-notice", NOTICE_HANDOFF)?;
    if !notices && !supports_handoff(&exe, "probe", HANDOFF)? {
        output::result("This Malak version needs the previous process to finish the upgrade.")?;
        return complete(
            &exe,
            &installed,
            &format!("Installed Malak {target}."),
            refresh_agents,
            &boards.running,
            Completion::Legacy,
        );
    }
    let mut next = Command::new(&exe);
    next.args([
        FINISH,
        if refresh_agents { "refresh" } else { "skip" },
        &installed,
    ]);
    if notices {
        let previous = notice::Previous::current(previous);
        next.arg("--previous")
            .arg(serde_json::to_string(&previous).map_err(|error| config::io_error(error.into()))?);
    }
    for board in boards.running {
        next.arg(serde_json::to_string(&board).map_err(|error| config::io_error(error.into()))?);
    }
    Err(Error::unavailable(format!(
        "could not hand over to the installed Malak at {}: {}",
        exe.display(),
        next.exec()
    )))
}

fn show_notice_preview(source: &source::Source) -> Result<(), Error> {
    let exe = std::env::current_exe().map_err(config::io_error)?;
    let previous =
        notice::Previous::current(board_version::build_at(&exe).map_err(config::io_error)?);
    let changes = source.lead_changes()?;
    let message = notice::message(
        Some(&previous.build),
        "to be determined by Cargo",
        changes.as_deref().unwrap_or_default(),
        previous.entry.as_deref(),
    );
    let format = output::Format::stdout();
    let mut preview = format.details("Notice preview if the installed build changes.", &[]);
    format.paragraph(&mut preview, "  ", &message);
    output::write(&preview)?;
    if changes.is_none() {
        output::result("The installed build supplies the lead-facing changelog.")?;
    }
    Ok(())
}

fn supports_handoff(exe: &Path, probe: &str, expected: &str) -> Result<bool, Error> {
    let result = Command::new(exe)
        .args([FINISH, probe])
        .output()
        .map_err(config::io_error)?;
    Ok(result.status.success() && result.stdout == format!("{expected}\n").as_bytes())
}

fn up_to_date() -> Result<ExitCode, Error> {
    output::result("Malak is already up to date.")?;
    Ok(ExitCode::SUCCESS)
}

fn agents_need_attention(root: &Path) -> bool {
    // A failed inspection still needs reporting during completion. Other
    // boards can finish even if the agent integration needs manual repair.
    recipe::agents_pending(root).unwrap_or(true)
}

fn finish_pending(
    exe: &Path,
    build: &str,
    refresh: bool,
    boards: &[Restart],
) -> Result<ExitCode, Error> {
    if !refresh && boards.is_empty() {
        return up_to_date();
    }
    complete(
        exe,
        build,
        &format!(
            "Finishing pending work for Malak {}.",
            board_version::VERSION
        ),
        refresh,
        boards,
        Completion::Pending,
    )
}

fn collect_boards(
    root: &Path,
    explicit: Option<PathBuf>,
    only_outdated: bool,
) -> Result<Inventory, Error> {
    let mut inventory = Inventory::default();
    for board in board_names::discover(root, explicit) {
        let mut inspection = Ok(BoardState::Stopped);
        for entry in board.configs() {
            match inspect(entry, only_outdated) {
                Ok(BoardState::Stopped) => {}
                Ok(state) => {
                    inspection = Ok(state);
                    break;
                }
                Err(error) => inspection = Err(error),
            }
        }
        match inspection {
            Ok(BoardState::Restart(plan)) => inventory.running.push(plan),
            Ok(BoardState::Current) => {}
            Ok(BoardState::Stopped) => {
                inventory.stopped.push(board_names::name(board.primary()));
            }
            Err(error) => {
                let entry = board.primary();
                return Err(Error::unavailable(format!(
                    "could not check board {} ({}): {error}",
                    board_names::name(entry),
                    entry.path.display(),
                )));
            }
        }
    }
    Ok(inventory)
}

fn show_boards(inventory: &Inventory) -> Result<(), Error> {
    for name in &inventory.stopped {
        output::result(&format!("{name} is stopped."))?;
    }
    for board in &inventory.running {
        let connected = worker_names(board.workers.iter().filter(|worker| worker.connected));
        let launchers = worker_names(board.workers.iter().filter(|worker| worker.launching));
        let config = board.config.to_string_lossy();
        let connected = if connected.is_empty() {
            "None"
        } else {
            &connected
        };
        let mut fields = vec![
            ("Config", config.as_ref()),
            ("Connected workers", connected),
        ];
        if !launchers.is_empty() {
            fields.push(("Active worker launchers", &launchers));
        }
        output::details(&format!("Will restart {}.", board.name), &fields)?;
    }
    if inventory.running.is_empty() {
        output::result("No running boards to restart.")?;
    } else {
        output::result(
            "Connected workers will be disconnected. Their hooks reconnect on the next call.",
        )?;
    }
    Ok(())
}

fn worker_names<'a>(workers: impl Iterator<Item = &'a Worker>) -> String {
    workers
        .map(|worker| worker.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn inspect(entry: &board_group::Entry, only_outdated: bool) -> Result<BoardState, Error> {
    let settings = entry.board.as_ref().map_err(Clone::clone)?;
    if !board_group::is_running(settings)? {
        return Ok(BoardState::Stopped);
    }
    let (mut client, version, _) = owner_for_board_quiet(settings)?;
    let outdated = version.mismatch().is_some();
    if only_outdated && !outdated {
        return Ok(BoardState::Current);
    }
    check_lead_account(settings)?;
    let (run, workers) = workers::observe(&mut client, settings)?;
    Ok(BoardState::Restart(Restart {
        config: entry.path.clone(),
        name: board_names::name(entry),
        run,
        workers,
        outdated,
    }))
}

fn check_lead_account(settings: &config::Board) -> Result<(), Error> {
    let owner = nix::unistd::geteuid().as_raw();
    let lead = settings
        .lead_account
        .as_deref()
        .map(user_id)
        .transpose()?
        .unwrap_or(owner);
    let door = settings.socket_folder.join(board::LEAD_FOLDER);
    let account = if lead == owner {
        config::open_owned_directory(&door, true).map(|_| ())
    } else {
        malak_fs::trusted_parent_for_account(&door.join(board::SOCKET_FILE), owner, lead)
            .map(|_| ())
    };
    account.map_err(|error| Error::refused(format!(
        "lead access differs from the board configuration; check lead_account before restarting: {error}"
    )))
}

pub(super) fn finish(arguments: &[OsString]) -> Result<ExitCode, Error> {
    if arguments == ["probe"] {
        output::line(format_args!("{HANDOFF}"))?;
        return Ok(ExitCode::SUCCESS);
    }
    if arguments == ["probe-notice"] {
        output::line(format_args!("{NOTICE_HANDOFF}"))?;
        return Ok(ExitCode::SUCCESS);
    }
    let [agent_action, build, boards @ ..] = arguments else {
        return Err(Error::refused("missing upgrade plan"));
    };
    let refresh = match agent_action.to_str() {
        Some("refresh") => true,
        Some("skip") => false,
        _ => return Err(Error::refused("invalid agent integration action")),
    };
    let (previous, boards) = match boards {
        [flag, previous, boards @ ..] if flag == "--previous" => (
            Some(
                serde_json::from_slice(previous.as_encoded_bytes())
                    .map_err(|error| Error::refused(format!("invalid previous build: {error}")))?,
            ),
            boards,
        ),
        _ => (None, boards),
    };
    let boards = boards
        .iter()
        .map(|board| serde_json::from_slice::<Restart>(board.as_encoded_bytes()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| Error::refused(format!("invalid board restart plan: {error}")))?;
    let exe = std::env::current_exe().map_err(config::io_error)?;
    let build = build
        .to_str()
        .ok_or_else(|| Error::refused("invalid installed build ID"))?;
    complete(
        &exe,
        build,
        &format!("Installed Malak {}.", board_version::VERSION),
        refresh,
        &boards,
        Completion::Upgrade(previous),
    )
}

fn complete(
    exe: &Path,
    build: &str,
    heading: &str,
    refresh: bool,
    boards: &[Restart],
    completion: Completion,
) -> Result<ExitCode, Error> {
    if board_version::build_at(exe).map_err(config::io_error)? != build {
        return Err(Error::unavailable(
            "the installed build changed during the upgrade",
        ));
    }
    output::result(heading)?;
    let mut failed = false;
    let mut hooks_changed = false;
    if refresh {
        let result = if matches!(completion, Completion::Legacy) {
            let status = Command::new(exe)
                .args(["recipe", "agents", "--yes"])
                .stdout(Stdio::null())
                .status()
                .map_err(config::io_error);
            status.and_then(|status| {
                Err(Error::unavailable(if status.success() {
                    "this version's agents recipe cannot refresh an existing installation"
                } else {
                    "this version's agents recipe failed"
                }))
            })
        } else {
            board_list::directory().and_then(|root| recipe::refresh_agents(&root))
        };
        match result {
            Ok(changed) => hooks_changed = changed,
            Err(error) => {
                failed = true;
                output::details(
                    "Could not refresh the agent integration.",
                    &[("Reason", &error.to_string())],
                )?;
            }
        }
    }
    if hooks_changed
        && let Err(error) =
            board_list::directory().and_then(|root| recipe::reload_codex_hooks(&root))
    {
        failed = true;
        output::details(
            "Could not ask open Codex threads to reload their hooks.",
            &[("Reason", &error.to_string())],
        )?;
    }
    let mut restarted = Vec::new();
    for plan in boards {
        match restart(exe, build, plan) {
            Ok(board) => {
                restarted.push(board);
                output::result(&format!("Restarted {}.", plan.name))?;
            }
            Err(error) => {
                failed = true;
                output::details(
                    &format!("Could not restart {}.", plan.name),
                    &[
                        ("Config", &plan.config.to_string_lossy()),
                        ("Reason", &error.to_string()),
                    ],
                )?;
            }
        }
    }
    if let Completion::Upgrade(previous) = completion {
        failed |= publish_notices(&mut restarted, previous.as_ref(), build)?;
    }
    if hooks_changed {
        output::result(HOOK_REVIEW)?;
    }
    if !restarted.is_empty() {
        output::result(
            "Restart each lead chat's watcher. The reminder will flag a stopped watcher.",
        )?;
    }
    if failed {
        return Err(Error::unavailable("some upgrade steps failed"));
    }
    Ok(ExitCode::SUCCESS)
}

fn publish_notices(
    boards: &mut [Restarted<'_>],
    previous: Option<&notice::Previous>,
    installed: &str,
) -> Result<bool, Error> {
    let previous_build = previous.map(|old| old.build.as_str());
    let entry = previous.and_then(|old| old.entry.as_deref());
    let mut failed = false;
    for board in &mut *boards {
        let old = board.previous.as_deref().or(previous_build);
        if old == Some(installed) {
            continue;
        }
        let board_entry = if old == previous_build { entry } else { None };
        if let Err(error) = owner_commands::post_upgrade_notice(
            &mut board.client,
            &board.run,
            old,
            installed,
            board_entry,
        ) {
            failed = true;
            output::details(
                &format!("Could not notify the lead of {}.", board.name),
                &[("Reason", &error.to_string())],
            )?;
        }
    }
    // The first version of the handoff omitted the previous executable's ID.
    // In that case use the prior served ID if the restarted boards agree.
    let old = previous_build.or_else(|| {
        let first = boards.first()?.previous.as_deref()?;
        boards
            .iter()
            .all(|board| board.previous.as_deref() == Some(first))
            .then_some(first)
    });
    if old != Some(installed) {
        let notice = notice::message(old, installed, notice::CHANGELOG, entry);
        if let Err(error) =
            board_list::directory().and_then(|root| recipe::notify_upgrade(&root, &notice))
        {
            failed = true;
            output::details(
                "Could not notify open Codex threads.",
                &[("Reason", &error.to_string())],
            )?;
        }
    }
    Ok(failed)
}

fn restart<'a>(exe: &Path, build: &str, plan: &'a Restart) -> Result<Restarted<'a>, Error> {
    let settings = config::Config::read(Some(plan.config.clone()))?.board;
    check_lead_account(&settings)?;
    let (mut client, version, _) = owner_for_board_quiet(&settings)?;
    let previous = version.build().map(str::to_owned);
    if owner_commands::current_run(&mut client)? != plan.run {
        return Err(Error::refused("the board run changed after the plan"));
    }
    board_group::stop_board(&settings, client, &plan.run)?;
    let status = Command::new(exe)
        .arg("--config")
        .arg(&plan.config)
        .args(["board", "start", "--detach"])
        .status()
        .map_err(config::io_error)?;
    if !status.success() {
        return Err(Error::unavailable(format!(
            "the detached board start failed ({status})"
        )));
    }
    let (mut client, version, _) = owner_for_board_quiet(&settings)?;
    let running = owner_commands::current_run(&mut client)?;
    if version.build() != Some(build) || running == plan.run {
        return Err(Error::unavailable(
            "the board did not report a new run on the installed build",
        ));
    }
    Ok(Restarted {
        name: &plan.name,
        run: running,
        previous,
        client,
    })
}
