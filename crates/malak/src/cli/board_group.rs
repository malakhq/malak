use super::{board_list, board_names, config, output, owner_commands, owner_for_board};
use malak::{CreationId, Error, Outcome};
use std::fs::{File, TryLockError};
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) struct Entry {
    pub path: PathBuf,
    pub board: Result<config::Board, Error>,
}

impl Entry {
    pub(super) fn read(path: PathBuf) -> Self {
        Self {
            board: config::Config::read(Some(path.clone())).map(|config| config.board),
            path,
        }
    }
}

pub(super) fn stop_all(explicit: Option<PathBuf>) -> Result<(), Error> {
    let root = board_list::directory()?;
    let boards = board_names::discover(&root, explicit);
    stop(&boards, false, |board, title, detail| {
        let config = board.primary().path.to_string_lossy();
        let mut fields = vec![("Config", config.as_ref())];
        fields.extend(detail);
        output::details(title, &fields)
    })
}

pub(super) fn stop_selected(path: Option<PathBuf>, run: Option<CreationId>) -> Result<(), Error> {
    let settings = config::Config::read(path)?.board;
    let (mut client, _) = owner_for_board(&settings)?;
    let run = match run {
        Some(run) => run.to_string(),
        None => owner_commands::current_run(&mut client)?,
    };
    stop_board(&settings, client, &run)?;
    output::details("Board stopped.", &[("Run", &run)])
}

pub(super) fn stop_board(
    settings: &config::Board,
    mut client: owner_commands::Client,
    run: &str,
) -> Result<(), Error> {
    let socket =
        config::open_owned_directory(&settings.socket_folder, false).map_err(config::io_error)?;
    owner_commands::stop_run(&mut client, run)?;
    drop(client);
    if let Some(socket) = socket {
        await_release(socket.file()).map_err(config::io_error)?;
    }
    Ok(())
}

pub(super) fn stop(
    boards: &[board_names::LocalBoard],
    dry_run: bool,
    report: impl Fn(&board_names::LocalBoard, &str, Option<(&str, &str)>) -> Result<(), Error>,
) -> Result<(), Error> {
    if boards.is_empty() {
        return output::result("No board configurations found.");
    }
    let mut failed = false;
    for board in boards {
        let (title, detail) = match &board.primary().board {
            Ok(board) => match stop_one(board, dry_run) {
                Ok(Some(run)) => (
                    if dry_run {
                        "Would stop board."
                    } else {
                        "Board stopped."
                    },
                    Some(("Run", run)),
                ),
                Ok(None) => ("Board already stopped.", None),
                Err(StopError::Unknown(error)) => (
                    "Could not inspect board.",
                    Some(("Reason", error.to_string())),
                ),
                Err(StopError::Running(error)) => {
                    failed = true;
                    (
                        "Could not stop running board.",
                        Some(("Reason", error.to_string())),
                    )
                }
            },
            Err(error) => (
                "Could not inspect board.",
                Some(("Reason", error.to_string())),
            ),
        };
        report(
            board,
            title,
            detail
                .as_ref()
                .map(|(label, value)| (*label, value.as_str())),
        )?;
    }
    if failed {
        return Err(Error::unavailable(
            "one or more running boards could not be stopped",
        ));
    }
    Ok(())
}

enum StopError {
    Unknown(Error),
    Running(Error),
}

fn stop_one(board: &config::Board, dry_run: bool) -> Result<Option<String>, StopError> {
    let inspect = |error| StopError::Unknown(config::io_error(error));
    let Some(directory) =
        config::open_owned_directory(&board.socket_folder, false).map_err(inspect)?
    else {
        return Ok(None);
    };
    let folder = directory.file();
    if try_lock(folder).map_err(inspect)? {
        return Ok(None);
    }
    match request(board, dry_run) {
        Ok(run) => {
            if !dry_run {
                await_release(folder)
                    .map_err(|error| StopError::Running(config::io_error(error)))?;
            }
            Ok(Some(run))
        }
        // Another command may have stopped the run after we checked the
        // lock. Its owner door can already be closing as we connect.
        Err(ref error)
            if error.outcome() == Outcome::Unavailable && await_release(folder).is_ok() =>
        {
            Ok(None)
        }
        Err(_) if try_lock(folder).unwrap_or(false) => Ok(None),
        Err(error) => Err(StopError::Running(error)),
    }
}

fn request(board: &config::Board, dry_run: bool) -> Result<String, Error> {
    let (mut client, _) = owner_for_board(board)?;
    let run = owner_commands::current_run(&mut client)?;
    if !dry_run {
        owner_commands::stop_run(&mut client, &run)?;
    }
    Ok(run)
}

pub(super) fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

pub(super) fn is_running(settings: &config::Board) -> Result<bool, Error> {
    let Some(directory) =
        config::open_owned_directory(&settings.socket_folder, false).map_err(config::io_error)?
    else {
        return Ok(false);
    };
    try_lock(directory.file())
        .map(|unlocked| !unlocked)
        .map_err(config::io_error)
}

fn await_release(file: &File) -> io::Result<()> {
    // The socket lock stays held after the STOP receipt until the doors
    // close and the store is released.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if try_lock(file)? {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the board still holds its socket folder",
            ));
        }
        std::thread::sleep(left.min(Duration::from_millis(25)));
    }
}
