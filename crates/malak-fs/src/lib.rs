//! Descriptor-relative filesystem operations shared by persistent layers.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use nix::dir::Dir;
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag, open, openat, renameat};
use nix::sys::stat::{Mode, mkdirat};
use nix::unistd::{AccessFlags, UnlinkatFlags, faccessat, unlinkat};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod acl;

mod shared;
pub use shared::{AccountAccess, can_traverse, check_credential, open_credential, share_with};

const DIRECTORY: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);

/// An opened directory for relative file operations.
#[derive(Debug)]
pub struct Directory(File);

impl AsFd for Directory {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Directory {
    /// Open a directory without following a final symbolic link.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = path.components().collect::<PathBuf>();
        Ok(Self(File::from(open(&path, DIRECTORY, Mode::empty())?)))
    }

    /// Open an existing directory or create it with owner-only permissions.
    ///
    /// Returns its parent, the opened directory, and whether it was created.
    /// Inspect the opened directory before using it and sync the parent when
    /// the directory's name must be durable.
    pub fn open_or_create(path: &Path) -> io::Result<(Self, Self, bool)> {
        let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
        let parent = path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = Self::open(parent)?;
        let (directory, created) = parent.child_or_create(name)?;
        Ok((parent, directory, created))
    }

    /// Open a directory within this one without following a symbolic link.
    pub fn child(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        Ok(Self(File::from(openat(
            self,
            leaf(Path::new(name))?,
            DIRECTORY,
            Mode::empty(),
        )?)))
    }

    fn search_child(&self, name: &std::ffi::OsStr) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let search = OFlag::O_PATH;
        #[cfg(target_os = "macos")]
        let search = OFlag::from_bits_retain(nix::libc::O_SEARCH);
        Ok(Self(File::from(openat(
            self,
            leaf(Path::new(name))?,
            DIRECTORY | search,
            Mode::empty(),
        )?)))
    }

    /// Open a child directory, creating it with owner-only permissions if absent.
    pub fn child_or_create(&self, name: &std::ffi::OsStr) -> io::Result<(Self, bool)> {
        match self.child(name) {
            Ok(directory) => Ok((directory, false)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let created = match mkdirat(self, leaf(Path::new(name))?, Mode::S_IRWXU) {
                    Ok(()) => true,
                    Err(Errno::EEXIST) => false,
                    Err(error) => return Err(error.into()),
                };
                Ok((self.child(name)?, created))
            }
            Err(error) => Err(error),
        }
    }

    /// Borrow the directory's file for metadata and locking.
    pub const fn file(&self) -> &File {
        &self.0
    }

    /// Open an iterator over the contents of this directory.
    pub fn entries(&self) -> io::Result<Dir> {
        Ok(Dir::openat(self, ".", DIRECTORY, Mode::empty())?)
    }

    /// Open a regular file without following a link or blocking on a FIFO.
    ///
    /// Newly created files have owner-only permissions. An existing file's
    /// permissions can be checked by the caller after opening.
    pub fn open_file(&self, name: &str, flags: OFlag) -> io::Result<File> {
        if flags.contains(OFlag::O_TRUNC) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let flags = flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK;
        let fd = openat(
            self,
            leaf(Path::new(name))?,
            flags,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )?;
        let file = File::from(fd);
        let meta = file.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a regular file with one link",
            ));
        }
        Ok(file)
    }

    /// Replace a file in this directory with another from the same directory.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        Ok(renameat(
            self,
            leaf(Path::new(from))?,
            self,
            leaf(Path::new(to))?,
        )?)
    }

    /// Atomically publish a sibling at a destination that does not yet exist.
    pub fn rename_to_new(&self, from: &str, to: &str) -> io::Result<()> {
        Ok(rustix::fs::renameat_with(
            self,
            leaf(Path::new(from))?,
            self,
            leaf(Path::new(to))?,
            rustix::fs::RenameFlags::NOREPLACE,
        )?)
    }

    /// Remove a file or symbolic link in this directory.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        Ok(unlinkat(
            self,
            leaf(Path::new(name))?,
            UnlinkatFlags::NoRemoveDir,
        )?)
    }

    /// Flush this directory's entries to stable storage.
    pub fn sync(&self) -> io::Result<()> {
        sync(&self.0)
    }
}

/// Flush data and metadata, requesting the device flush on macOS.
pub fn sync(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    nix::fcntl::fcntl(file, nix::fcntl::FcntlArg::F_FULLFSYNC)?;
    #[cfg(target_os = "linux")]
    file.sync_all()?;
    Ok(())
}

/// Reject an extended ACL that grants access, or just one granting changes.
///
/// The caller must also check POSIX owner and mode. On Linux an effective
/// POSIX ACL grant is constrained by the group permission mask in the mode.
pub fn check_acl(file: &File, private: bool) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    acl::check(file, private)?;
    #[cfg(target_os = "linux")]
    let _ = (file, private);
    Ok(())
}

/// Open an absolute path's parent through caller- or root-owned directories
/// whose contents and permissions cannot be changed by other accounts.
///
/// Every component is opened without following links. The caller still checks
/// the final file or directory before using it.
pub fn trusted_parent(path: &Path, uid: u32) -> io::Result<(Directory, &std::ffi::OsStr)> {
    checked_parent(path, |folder| {
        check_user_permissions(folder.file(), uid, false)
    })
}

/// Check trusted ancestry and the recipient's search access before writing a credential.
pub fn trusted_parent_for_account(
    path: &Path,
    owner: u32,
    reader: u32,
) -> io::Result<(Directory, &std::ffi::OsStr)> {
    if owner == reader {
        return trusted_parent(path, owner);
    }
    checked_parent(path, |folder| {
        check_user_permissions(folder.file(), owner, false)?;
        can_traverse(folder, reader)
    })
}

fn checked_parent(
    path: &Path,
    check: impl Fn(&Directory) -> io::Result<()>,
) -> io::Result<(Directory, &std::ffi::OsStr)> {
    if !path.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut folder = Directory::open(Path::new("/"))?;
    check(&folder)?;
    for component in parent.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                folder = folder.child(name)?;
                check(&folder)?;
            }
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    faccessat(&folder, ".", AccessFlags::X_OK, AtFlags::AT_EACCESS)?;
    Ok((folder, name))
}

/// Verify an opened file's owner, mode, and platform access list.
///
/// Private files must belong to the given account and grant no access to
/// other accounts. Trusted files may also belong to root and allow reading
/// or traversal, but may grant no write access to other accounts.
pub fn check_user_permissions(file: &File, uid: u32, private: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    let extra = if private { 0o077 } else { 0o022 };
    if (metadata.uid() != uid && (private || metadata.uid() != 0)) || metadata.mode() & extra != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file owner or permissions do not match the access policy",
        ));
    }
    check_acl(file, private)
}

fn leaf(path: &Path) -> io::Result<&std::ffi::OsStr> {
    match path.components().next() {
        Some(Component::Normal(part)) if part == path.as_os_str() => Ok(part),
        _ => Err(io::ErrorKind::InvalidInput.into()),
    }
}
