use super::{board_group, board_list, config};
use malak::Error;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The name displayed for a discovered entry, including an unreadable board.
pub(super) fn name(entry: &board_group::Entry) -> String {
    if let Some(name) = entry
        .board
        .as_ref()
        .ok()
        .and_then(|board| board.name.as_ref())
    {
        return name.clone();
    }
    if config::default_path().is_ok_and(|path| path == entry.path) {
        return "default".to_owned();
    }
    entry
        .path
        .parent()
        .and_then(|path| path.file_name())
        .map_or_else(
            || entry.path.display().to_string(),
            |folder| folder.to_string_lossy().into_owned(),
        )
}

/// One local board can be reached through several configuration files.
pub(super) struct LocalBoard {
    configs: Vec<board_group::Entry>,
}

impl LocalBoard {
    pub fn primary(&self) -> &board_group::Entry {
        &self.configs[0]
    }

    pub fn configs(&self) -> impl Iterator<Item = &board_group::Entry> {
        self.configs.iter()
    }
}

/// Discover the named, recorded and default boards for the current account.
pub(super) fn list() -> Result<Vec<board_group::Entry>, Error> {
    list_with(|root| discover(root, None))
}

pub(super) fn list_quiet() -> Result<Vec<board_group::Entry>, Error> {
    list_with(|root| group(board_list::discover_quiet(root)))
}

fn list_with(
    discover: impl FnOnce(&Path) -> Vec<LocalBoard>,
) -> Result<Vec<board_group::Entry>, Error> {
    let root = board_list::directory()?;
    Ok(discover(&root)
        .into_iter()
        .map(|board| {
            board
                .configs
                .into_iter()
                .next()
                .expect("one config per board")
        })
        .collect())
}

pub(super) fn discover(root: &Path, explicit: Option<PathBuf>) -> Vec<LocalBoard> {
    group(board_list::discover(root, explicit))
}

fn group(paths: BTreeSet<PathBuf>) -> Vec<LocalBoard> {
    let entries: Vec<_> = paths
        .into_iter()
        .filter(|path| {
            !path
                .symlink_metadata()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        })
        .map(board_group::Entry::read)
        .collect();
    let mut parents: Vec<_> = (0..entries.len()).collect();
    let mut states = BTreeMap::new();
    let mut sockets = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        let Ok(board) = &entry.board else { continue };
        for (folder, seen) in [
            (&board.state, &mut states),
            (&board.socket_folder, &mut sockets),
        ] {
            // Use canonical paths for comparison only. Commands still open
            // the configured paths through the existing trust checks.
            let key = folder.canonicalize().unwrap_or_else(|_| folder.clone());
            if let Some(previous) = seen.insert(key, index) {
                let first = group_root(&mut parents, previous);
                let second = group_root(&mut parents, index);
                parents[first.max(second)] = first.min(second);
            }
        }
    }
    let mut groups: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (index, entry) in entries.into_iter().enumerate() {
        groups
            .entry(group_root(&mut parents, index))
            .or_default()
            .push(entry);
    }
    let default = config::default_path().ok();
    groups
        .into_values()
        .map(|mut configs| {
            // A named config represents the board when the default is an alias.
            if let Some(named) = configs
                .iter()
                .position(|entry| default.as_ref() != Some(&entry.path))
            {
                configs.swap(0, named);
            }
            LocalBoard { configs }
        })
        .collect()
}

fn group_root(parents: &mut [usize], mut index: usize) -> usize {
    while parents[index] != index {
        parents[index] = parents[parents[index]];
        index = parents[index];
    }
    index
}

pub(super) enum ResolveError {
    Unknown(Error),
    Other(Error),
}

impl From<ResolveError> for Error {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::Unknown(error) | ResolveError::Other(error) => error,
        }
    }
}

/// Resolve a unique name without starting a board or selecting another board.
pub(super) fn resolve(requested: &str) -> Result<PathBuf, ResolveError> {
    let mut known = BTreeSet::new();
    let mut matches = Vec::new();
    let root = board_list::directory().map_err(ResolveError::Other)?;
    let boards = discover(&root, None);
    for board in &boards {
        let mut matching = None;
        for entry in board.configs() {
            let label = name(entry);
            if label == requested {
                matching.get_or_insert(&entry.path);
            }
            known.insert(label);
        }
        if let Some(path) = matching {
            matches.push((path, &board.primary().path));
        }
    }
    match matches.len() {
        1 => Ok(matches[0].1.clone()),
        0 => {
            let names = if known.is_empty() {
                "(none)".to_owned()
            } else {
                known.into_iter().collect::<Vec<_>>().join(", ")
            };
            Err(ResolveError::Unknown(Error::refused(format!(
                "unknown board name {requested:?}. Known boards: {names}"
            ))))
        }
        _ => {
            let paths = matches
                .iter()
                .map(|(path, _)| path.to_string_lossy())
                .collect::<Vec<_>>();
            Err(ResolveError::Other(Error::refused(format!(
                "ambiguous board name {requested:?}. Matching configurations: {}",
                paths.join(", ")
            ))))
        }
    }
}
