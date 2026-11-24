use super::layout::Kind;
use super::{board_group, board_list, config, output};
use crate::cli::board::check_owner_address;
use malak::Error;
use malak_core::OwnerCredential;
use malak_fs::{Directory, check_user_permissions, trusted_parent};
use nix::fcntl::{AtFlags, OFlag, openat};
use nix::libc::{dev_t, ino_t};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
use nix::unistd::{UnlinkatFlags, geteuid, unlinkat};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(super) struct Plan {
    targets: Vec<(PathBuf, Node)>,
    locks: Vec<Directory>,
}

type FolderId = (dev_t, ino_t);

fn folder_id(stat: &FileStat) -> FolderId {
    (stat.st_dev, stat.st_ino)
}

impl Plan {
    pub(super) fn prepare<'a>(
        entries: impl IntoIterator<Item = &'a board_group::Entry>,
        root: &Path,
        exe: &Path,
        has_list: bool,
        dry_run: bool,
    ) -> Result<Self, Error> {
        let mut plan = Self {
            targets: Vec::new(),
            locks: Vec::new(),
        };
        let mut added = BTreeMap::new();
        for entry in entries {
            let board = match &entry.board {
                Ok(board) => board,
                Err(_)
                    if matches!(
                        entry.path.symlink_metadata(),
                        Err(error) if error.kind() == io::ErrorKind::NotFound
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(refused(&entry.path, error)),
            };
            if [&board.socket_folder, &board.state, &entry.path]
                .iter()
                .any(|path| exe.starts_with(path))
            {
                return Err(refused(
                    exe,
                    "the running binary is inside a board data location",
                ));
            }
            let socket = open_root(&board.socket_folder, root, false, dry_run, &added)?;
            let state = open_root(&board.state, root, true, dry_run, &added)?;
            if socket
                .as_ref()
                .zip(state.as_ref())
                .is_some_and(|((_, sockets), (_, state))| folder_id(sockets) == folder_id(state))
            {
                return Err(refused(
                    &board.state,
                    "board state and sockets share a directory",
                ));
            }
            if socket.is_some() || state.is_some() {
                let credential = OwnerCredential::read(&board.state).map_err(|error| {
                    Error::refused(format!(
                        "cannot verify board data at {}: {error}",
                        board.state.display()
                    ))
                })?;
                check_owner_address(board, &credential)?;
            }
            for (path, directory, kind) in [
                (&board.socket_folder, socket, Kind::SocketTree),
                (&board.state, state, Kind::StateTree),
            ] {
                if let Some((directory, locked)) = directory {
                    let node = collect(path, kind)?;
                    if !same_entry(&node.stat, &locked) {
                        return Err(refused(
                            path,
                            "board folder changed while planning deletion",
                        ));
                    }
                    plan.targets.push((path.clone(), node));
                    plan.locks.push(directory);
                    added.insert(folder_id(&locked), matches!(kind, Kind::StateTree));
                }
            }
            plan.targets
                .push((entry.path.clone(), collect(&entry.path, Kind::TrustedFile)?));
        }
        if has_list {
            let list = board_list::path(root);
            plan.targets
                .push((list.clone(), collect(&list, Kind::PrivateFile)?));
        }
        plan.check_overlaps()?;
        Ok(plan)
    }

    fn check_overlaps(&mut self) -> Result<(), Error> {
        let mut trees = BTreeSet::new();
        for (path, node) in &self.targets {
            if node.children.is_some() {
                if trees
                    .iter()
                    .any(|other: &PathBuf| path.starts_with(other) || other.starts_with(path))
                {
                    return Err(Error::refused(format!(
                        "overlapping board directories at {}",
                        path.display()
                    )));
                }
                trees.insert(path.clone());
            }
        }
        // A board tree's layout governs the entries within it.
        self.targets.retain(|(path, node)| {
            node.children.is_some() || !trees.iter().any(|tree| path.starts_with(tree))
        });
        Ok(())
    }

    pub(super) fn show(&self, verbose: bool, external: Option<&Path>) -> Result<(), Error> {
        output::result("Will delete")?;
        if !verbose && !self.targets.iter().any(|(_, node)| node.remove_self) {
            output::line(format_args!(
                "  (No entire folders or files at the listed roots.)"
            ))?;
        }
        for (path, node) in &self.targets {
            node.show(path, verbose)?;
        }
        if !verbose && self.targets.iter().any(|(_, node)| !node.remove_self) {
            output::result(
                "Malak will also delete managed entries from the folders listed below.",
            )?;
        }
        self.show_kept("Will keep", external)
    }

    pub(super) fn delete(self, verbose: bool, external: Option<&Path>) -> Result<(), Error> {
        let uid = geteuid().as_raw();
        for (path, node) in &self.targets {
            let (parent, name) = trusted_parent(path, uid).map_err(|error| refused(path, error))?;
            node.remove(&parent, name, path, verbose)?;
            if !verbose && node.remove_self {
                deleted(path)?;
            }
        }
        self.show_kept("Left these paths.", external)
    }

    fn show_kept(&self, title: &str, external: Option<&Path>) -> Result<(), Error> {
        if external.is_some() || self.targets.iter().any(|(_, node)| !node.remove_self) {
            output::result(title)?;
            for (path, node) in &self.targets {
                node.show_kept(path)?;
            }
            if let Some(path) = external {
                show_external(path)?;
            }
        }
        Ok(())
    }
}

fn open_root(
    path: &Path,
    registry: &Path,
    private: bool,
    dry_run: bool,
    added: &BTreeMap<FolderId, bool>,
) -> Result<Option<(Directory, FileStat)>, Error> {
    if registry.starts_with(path) {
        return Err(Error::refused(format!(
            "board folder {} contains the board list",
            path.display()
        )));
    }
    let Some(directory) =
        config::open_owned_directory(path, private).map_err(|error| refused(path, error))?
    else {
        return Ok(None);
    };
    let stat = fstat(&directory).map_err(|error| refused(path, error))?;
    if let Some(is_state) = added.get(&folder_id(&stat)) {
        if *is_state != private {
            return Err(refused(path, "board state and sockets share a directory"));
        }
        return Ok(None);
    }
    if !dry_run {
        directory
            .file()
            .try_lock()
            .map_err(|error| refused(path, error))?;
    }
    Ok(Some((directory, stat)))
}

struct Node {
    stat: FileStat,
    children: Option<Children>,
    remove_self: bool,
}

struct Children {
    managed: Vec<(OsString, Node)>,
    kept: Vec<OsString>,
}

fn collect(path: &Path, kind: Kind) -> Result<Node, Error> {
    let (parent, name) =
        trusted_parent(path, geteuid().as_raw()).map_err(|error| refused(path, error))?;
    let stat = fstatat(&parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)
        .map_err(|error| refused(path, error))?;
    Node::collect(&parent, name, path, kind, stat)
}

impl Node {
    fn collect(
        parent: &Directory,
        name: &OsStr,
        path: &Path,
        kind: Kind,
        stat: FileStat,
    ) -> Result<Self, Error> {
        let file_type = SFlag::from_bits_truncate(stat.st_mode);
        if file_type == SFlag::S_IFLNK {
            return Err(refused(path, "symbolic link"));
        }
        if file_type != kind.file_type() {
            return Err(refused(path, "unexpected file type"));
        }
        check_owner(&stat).map_err(|error| refused(path, error))?;
        let children = match file_type {
            SFlag::S_IFDIR => {
                let directory = parent.child(name).map_err(|error| refused(path, error))?;
                check_opened(directory.file(), &stat, path)?;
                check_mount(parent, directory.file(), path)?;
                check_user_permissions(directory.file(), geteuid().as_raw(), kind.private())
                    .map_err(|error| refused(path, error))?;
                Some(Children::collect(&directory, path, kind)?)
            }
            SFlag::S_IFREG => {
                let fd = openat(
                    parent,
                    name,
                    OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
                    Mode::empty(),
                )
                .map_err(|error| refused(path, error))?;
                let file = File::from(fd);
                let opened = check_opened(&file, &stat, path)?;
                check_mount(parent, &file, path)?;
                if opened.st_nlink != 1 {
                    return Err(refused(path, "expected a regular file with one link"));
                }
                check_user_permissions(&file, geteuid().as_raw(), kind.private())
                    .map_err(|error| refused(path, error))?;
                None
            }
            SFlag::S_IFSOCK => {
                check_socket_mount(parent, name, &stat, path)?;
                None
            }
            _ => return Err(refused(path, "unexpected file type")),
        };
        let remove_self = children.as_ref().is_none_or(|children| {
            children.kept.is_empty() && children.managed.iter().all(|(_, node)| node.remove_self)
        });
        Ok(Self {
            stat,
            children,
            remove_self,
        })
    }

    fn show(&self, path: &Path, verbose: bool) -> Result<(), Error> {
        if self.remove_self {
            output::line(format_args!(
                "  {}",
                output::display(&path.to_string_lossy())
            ))?;
        }
        if verbose && let Some(children) = &self.children {
            for (name, child) in &children.managed {
                child.show(&path.join(name), verbose)?;
            }
        }
        Ok(())
    }

    fn show_kept(&self, path: &Path) -> Result<(), Error> {
        if self.remove_self {
            return Ok(());
        }
        if let Some(children) = &self.children {
            kept(path, "directory contains other entries")?;
            for name in &children.kept {
                kept(&path.join(name), "outside Malak's managed data")?;
            }
            for (name, child) in &children.managed {
                child.show_kept(&path.join(name))?;
            }
        }
        Ok(())
    }

    fn remove(
        &self,
        parent: &Directory,
        name: &OsStr,
        path: &Path,
        verbose: bool,
    ) -> Result<(), Error> {
        let current = entry_stat(parent, name).map_err(|error| refused(path, error))?;
        if !same_entry(&current, &self.stat)
            || (current.st_mode, current.st_ctime, current.st_ctime_nsec)
                != (
                    self.stat.st_mode,
                    self.stat.st_ctime,
                    self.stat.st_ctime_nsec,
                )
        {
            return Err(refused(path, "entry changed after it was listed"));
        }
        let flags = if let Some(children) = &self.children {
            let directory = parent.child(name).map_err(|error| refused(path, error))?;
            check_opened(directory.file(), &self.stat, path)?;
            check_mount(parent, directory.file(), path)?;
            for (name, child) in &children.managed {
                child.remove(&directory, name, &path.join(name), verbose)?;
            }
            let current = entry_stat(parent, name).map_err(|error| refused(path, error))?;
            if !same_entry(&current, &self.stat) {
                return Err(refused(path, "directory changed during deletion"));
            }
            UnlinkatFlags::RemoveDir
        } else {
            UnlinkatFlags::NoRemoveDir
        };
        if self.remove_self {
            unlinkat(parent, name, flags).map_err(|error| refused(path, error))?;
            if verbose {
                deleted(path)?;
            }
        }
        Ok(())
    }
}

impl Children {
    fn collect(directory: &Directory, path: &Path, kind: Kind) -> Result<Self, Error> {
        let mut managed = Vec::new();
        let mut kept = Vec::new();
        for entry in directory.entries().map_err(|error| refused(path, error))? {
            let entry = entry.map_err(|error| refused(path, error))?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            let Some(child_kind) = kind.child(name) else {
                kept.push(name.to_owned());
                continue;
            };
            let child_path = path.join(name);
            let stat = fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(|error| refused(&child_path, error))?;
            let file_type = SFlag::from_bits_truncate(stat.st_mode);
            if file_type != child_kind.file_type() && file_type != SFlag::S_IFLNK {
                kept.push(name.to_owned());
                continue;
            }
            let child = Node::collect(directory, name, &child_path, child_kind, stat)?;
            managed.push((name.to_owned(), child));
        }
        // Keep power-cut retries possible until the rest of the state is gone.
        let credential = OsStr::new(OwnerCredential::FILE_NAME);
        managed.sort_by(|(left, _), (right, _)| {
            (left == credential, left).cmp(&(right == credential, right))
        });
        kept.sort_unstable();
        Ok(Self { managed, kept })
    }
}

fn same_entry(left: &FileStat, right: &FileStat) -> bool {
    (left.st_dev, left.st_ino) == (right.st_dev, right.st_ino)
}

fn check_mount(parent: &Directory, file: &File, path: &Path) -> Result<(), Error> {
    let parent = mount_id(parent).map_err(|error| refused(path, error))?;
    let child = mount_id(file).map_err(|error| refused(path, error))?;
    check_same_mount(parent, child, path)
}

fn check_same_mount<T: Eq>(parent: T, child: T, path: &Path) -> Result<(), Error> {
    if parent != child {
        return Err(refused(path, "entry is on a different mount"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount_id(file: impl std::os::fd::AsFd) -> io::Result<u64> {
    use std::os::fd::AsRawFd;

    // Overlayfs can report different device numbers for entries on one mount.
    // The mount ID also distinguishes bind mounts on the same device.
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_fd().as_raw_fd()))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .and_then(|id| id.trim().parse().ok())
        .ok_or_else(|| io::Error::other("could not determine the mount ID"))
}

#[cfg(target_os = "macos")]
fn mount_id(file: impl std::os::fd::AsFd) -> io::Result<dev_t> {
    Ok(fstat(file)?.st_dev)
}

#[cfg(target_os = "linux")]
fn check_socket_mount(
    parent: &Directory,
    name: &OsStr,
    stat: &FileStat,
    path: &Path,
) -> Result<(), Error> {
    let fd = openat(
        parent,
        name,
        OFlag::O_PATH | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| refused(path, error))?;
    let file = File::from(fd);
    check_opened(&file, stat, path)?;
    check_mount(parent, &file, path)
}

#[cfg(target_os = "macos")]
fn check_socket_mount(
    parent: &Directory,
    _name: &OsStr,
    stat: &FileStat,
    path: &Path,
) -> Result<(), Error> {
    let parent = mount_id(parent).map_err(|error| refused(path, error))?;
    check_same_mount(parent, stat.st_dev, path)
}

fn entry_stat(parent: &Directory, name: &OsStr) -> io::Result<FileStat> {
    let stat = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    check_owner(&stat)?;
    Ok(stat)
}

fn check_owner(stat: &FileStat) -> io::Result<()> {
    if stat.st_uid != geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "entry belongs to another account",
        ));
    }
    Ok(())
}

fn check_opened(file: &File, expected: &FileStat, path: &Path) -> Result<FileStat, Error> {
    let opened = fstat(file).map_err(|error| refused(path, error))?;
    if !same_entry(&opened, expected) || opened.st_mode != expected.st_mode {
        return Err(refused(path, "entry changed while opening it"));
    }
    Ok(opened)
}

fn kept(path: &Path, reason: &str) -> Result<(), Error> {
    output::line(format_args!(
        "  {} ({reason})",
        output::display(&path.to_string_lossy())
    ))
}

pub(super) fn show_external(path: &Path) -> Result<(), Error> {
    kept(path, "handover outside board folders")
}

fn deleted(path: &Path) -> Result<(), Error> {
    output::details("Deleted.", &[("Path", &path.to_string_lossy())])
}

fn refused(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::refused(format!("cannot delete {}: {error}", path.display()))
}
