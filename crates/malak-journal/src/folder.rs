use malak_fs::{Directory, check_acl};
use nix::dir::Dir;
use nix::fcntl::OFlag;
use nix::unistd::geteuid;
use std::fs::{File, TryLockError};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Exclusive custody of a private board state folder.
///
/// The lock follows the directory itself and is released when this handle is
/// dropped. Acquiring an existing folder leaves its contents and mode intact.
#[derive(Debug)]
pub struct StateFolder {
    directory: Directory,
    parent: Option<Directory>,
}

impl StateFolder {
    /// Create or open a board-owned directory and acquire its nonblocking lock.
    ///
    /// The directory's final path component cannot be a symbolic link. A
    /// directory another account could enter, read or modify is refused.
    pub fn acquire(path: impl AsRef<Path>) -> io::Result<Self> {
        let (parent, directory, created) = Directory::open_or_create(path.as_ref())?;

        let folder = Self::lock(directory, Some(parent))?;
        if created {
            folder.parent.as_ref().expect("parent descriptor").sync()?;
        }
        Ok(folder)
    }

    /// Lock an existing private state folder without creating anything.
    pub fn acquire_existing(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::lock(Directory::open(path.as_ref())?, None)
    }

    fn lock(directory: Directory, parent: Option<Directory>) -> io::Result<Self> {
        let metadata = directory.file().metadata()?;
        if metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state folder is not private",
            ));
        }
        check_acl(directory.file(), true)?;

        match directory.file().try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(TryLockError::Error(error)) => return Err(error),
        }

        Ok(Self { directory, parent })
    }

    pub(crate) fn files(&self) -> io::Result<Dir> {
        self.directory.entries()
    }

    pub(crate) fn open_file(&self, name: &str, flags: OFlag) -> io::Result<File> {
        let file = self.directory.open_file(name, flags)?;
        let meta = file.metadata()?;
        if meta.uid() != geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state file is not private",
            ));
        }
        check_acl(&file, true)?;
        Ok(file)
    }

    pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
        self.directory.remove(name)
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.directory.rename(from, to)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.directory.sync()
    }

    pub(crate) fn settle_parent(&mut self) -> io::Result<()> {
        if let Some(parent) = self.parent.take() {
            parent.sync()?;
        }
        Ok(())
    }
}

impl Drop for StateFolder {
    fn drop(&mut self) {
        // A child preparing to exec may still have the descriptor open.
        let _ = self.directory.file().unlock();
    }
}
