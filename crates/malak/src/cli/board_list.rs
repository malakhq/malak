use super::{board_group, board_names, config, mailbox, output, owner_for_board_quiet};
use malak::Error;
use malak_fs::{Directory, check_user_permissions};
use nix::{fcntl::OFlag, unistd::geteuid};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const LIST: &str = "boards.jsonl";

#[derive(Serialize)]
struct Summary {
    name: String,
    status: &'static str,
    participants: Option<usize>,
    lead_waiting: Option<u64>,
    build_warning: Option<String>,
}

impl Summary {
    fn read(entry: board_group::Entry) -> Self {
        let mut row = Self {
            name: board_names::name(&entry),
            status: "unavailable",
            participants: None,
            lead_waiting: None,
            build_warning: None,
        };
        let Ok(board) = entry.board else { return row };
        let Ok(running) = board_group::is_running(&board) else {
            return row;
        };
        if !running {
            row.status = "stopped";
            return row;
        }
        row.status = "running";
        if let Ok((mut client, version, _)) = owner_for_board_quiet(&board) {
            row.build_warning = version.mismatch();
            if let Ok((participants, waiting)) = mailbox::board_counts(&mut client) {
                row.participants = Some(participants);
                row.lead_waiting = Some(waiting);
            }
        }
        row
    }
}

pub(super) fn show(json: bool) -> Result<(), Error> {
    let mut boards: Vec<_> = board_names::list()?
        .into_iter()
        .map(Summary::read)
        .collect();
    boards.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        let json =
            serde_json::to_string(&boards).map_err(|error| config::io_error(error.into()))?;
        return output::line(format_args!("{json}"));
    }
    if boards.is_empty() {
        return output::result("No boards found.");
    }
    let rows = boards
        .into_iter()
        .map(|board| {
            vec![
                board.name,
                match board.status {
                    "running" => "Running",
                    "stopped" => "Stopped",
                    _ => "Unavailable",
                }
                .into(),
                board
                    .participants
                    .map_or_else(|| "Unavailable".into(), |n| n.to_string()),
                board
                    .lead_waiting
                    .map_or_else(|| "Unavailable".into(), |n| n.to_string()),
                board.build_warning.unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>();
    let format = output::Format::stdout();
    let mut text = format.details("Boards", &[]);
    text.push_str(&format.table(
        &[
            "Board",
            "Status",
            "Participants",
            "Lead waiting",
            "Build warning",
        ],
        &rows,
    ));
    output::write(&text)
}

pub(super) fn directory() -> Result<PathBuf, malak::Error> {
    std::env::home_dir()
        .filter(|path| path.is_absolute())
        .map(|path| path.join(".malak"))
        .ok_or_else(|| malak::Error::refused("could not find the home folder for the board list"))
}

pub(super) fn path(root: &Path) -> PathBuf {
    root.join(LIST)
}

/// Record before announcing readiness. A list that cannot be used does not
/// prevent starting a board with an explicit configuration.
pub(super) fn record(config: &Path) {
    let result = directory().and_then(|root| {
        let directory = config::owned_directory(&root)?;
        append(&directory, config).map_err(config::io_error)
    });
    if let Err(error) = result {
        output::diagnostic(
            "Warning: Could not record the board configuration.",
            &[
                ("Config", &config.to_string_lossy()),
                ("Reason", &error.to_string()),
            ],
        );
    }
}

fn append(directory: &Directory, config: &Path) -> io::Result<()> {
    directory.file().lock()?;
    let mut file = directory.open_file(LIST, OFlag::O_CREAT | OFlag::O_RDWR | OFlag::O_APPEND)?;
    check_user_permissions(&file, geteuid().as_raw(), true)?;
    if !entries(&file)?.contains(config) {
        let entry = serde_json::to_string(config)?;
        // A previous writer may have left an incomplete last line.
        writeln!(file, "\n{entry}")?;
    }
    Ok(())
}

pub(super) fn discover(root: &Path, explicit: Option<PathBuf>) -> BTreeSet<PathBuf> {
    discover_with(root, explicit, list_error)
}

pub(super) fn discover_quiet(root: &Path) -> BTreeSet<PathBuf> {
    discover_with(root, None, |_, _| {})
}

fn discover_with(
    root: &Path,
    explicit: Option<PathBuf>,
    report: impl Fn(&Path, &io::Error),
) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    let mut add = |path: PathBuf| {
        if path.symlink_metadata().is_ok() {
            paths.insert(path);
        }
    };
    if let Some(path) = explicit
        && let Ok(path) = std::path::absolute(path)
    {
        add(path);
    }
    if let Ok(default) = config::default_path() {
        add(default);
    }
    match config::open_owned_directory(root, false) {
        Ok(Some(directory)) => {
            match directory.entries() {
                Ok(entries) => {
                    for entry in entries.into_iter().flatten() {
                        let name = entry.file_name().to_bytes();
                        if matches!(name, b"." | b"..") {
                            continue;
                        }
                        let name = std::ffi::OsStr::from_bytes(name);
                        if directory.child(name).is_ok() {
                            add(root.join(name).join("board.toml"));
                        }
                    }
                }
                Err(error) => report(root, &error),
            }
            match directory.open_file(LIST, OFlag::O_RDONLY) {
                Ok(file) => match check_user_permissions(&file, geteuid().as_raw(), true)
                    .and_then(|()| entries(&file))
                {
                    Ok(recorded) => paths.extend(recorded),
                    Err(error) => report(&path(root), &error),
                },
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => report(&path(root), &error),
            }
        }
        Ok(None) => {}
        Err(error) => report(root, &error),
    }
    paths
}

fn list_error(path: &Path, error: &io::Error) {
    output::diagnostic(
        "Warning: Could not inspect the board list.",
        &[
            ("Path", &path.to_string_lossy()),
            ("Reason", &error.to_string()),
        ],
    );
}

fn entries(file: &std::fs::File) -> io::Result<BTreeSet<PathBuf>> {
    let mut entries = BTreeSet::new();
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(entries);
        }
        if let Ok(path) = serde_json::from_slice::<PathBuf>(&line)
            && path.is_absolute()
            && !path
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            entries.insert(path);
        }
    }
}

/// Open the list for deletion while keeping starts from appending to it.
pub(super) fn lock_for_deletion(root: &Path, dry_run: bool) -> io::Result<Option<Directory>> {
    let Some(directory) = config::open_owned_directory(root, false)? else {
        return Ok(None);
    };
    if !dry_run {
        directory.file().lock()?;
    }
    let meta = match directory.open_file(LIST, OFlag::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    check_user_permissions(&meta, geteuid().as_raw(), true)?;
    Ok(Some(directory))
}
