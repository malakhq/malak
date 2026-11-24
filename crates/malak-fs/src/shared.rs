//! Access for a single named local account on board-owned files and doors.

use super::{Directory, check_user_permissions};
use nix::fcntl::OFlag;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

#[cfg(target_os = "macos")]
use super::acl as platform;
#[cfg(target_os = "linux")]
#[path = "posix_acl.rs"]
mod platform;

/// The access a named account can have in addition to the owner.
#[derive(Clone, Copy)]
pub enum AccountAccess {
    /// Read a regular credential file.
    Read,
    /// Traverse a directory without listing or changing its entries.
    Search,
}

/// Install an access list granting exactly one other account the given right.
///
/// The existing access list is replaced. Use this for a newly created
/// credential or a directory whose access policy the board owns.
pub fn share_with(file: &File, account: u32, access: AccountAccess) -> io::Result<()> {
    let metadata = file.metadata()?;
    if account == metadata.uid()
        || match access {
            AccountAccess::Read => !metadata.is_file() || metadata.nlink() != 1,
            AccountAccess::Search => !metadata.is_dir(),
        }
    {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    platform::share_with(file, account, access)
}

/// Confirm a credential grants access only to its owner and the named reader.
pub fn check_credential(file: &File, owner: u32, reader: u32) -> io::Result<()> {
    if owner == reader {
        return check_user_permissions(file, owner, true);
    }
    if file.metadata()?.uid() != owner || platform::sole_reader(file)? != Some(reader) {
        return Err(denied());
    }
    Ok(())
}

/// Check that a trusted directory can be traversed by the given account.
pub fn can_traverse(directory: &Directory, account: u32) -> io::Result<()> {
    if platform::can_traverse(directory.file(), account)? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handover or entrance directory cannot be traversed by the named account",
        ))
    }
}

/// Open a readable credential, checking every ancestor against its file owner.
///
/// The returned descriptor grants read access to the owner or the single
/// named reader. Every path component is opened without following links.
pub fn open_credential(path: &Path, account: u32) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut ancestors = vec![Directory::open(Path::new("/"))?];
    for component in parent.components() {
        match component {
            Component::RootDir => (),
            Component::Normal(name) => {
                ancestors.push(ancestors.last().unwrap().search_child(name)?)
            }
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    let file = ancestors.last().unwrap().open_file(leaf, OFlag::O_RDONLY)?;
    let owner = file.metadata()?.uid();
    let reader = platform::sole_reader(&file)?;
    if owner != account && reader != Some(account) {
        return Err(denied());
    }
    check_credential(&file, owner, reader.unwrap_or(owner))?;
    for directory in ancestors {
        check_user_permissions(directory.file(), owner, false)?;
    }
    Ok(file)
}

pub(super) fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "credential access list is not private to its declared accounts",
    )
}

pub(super) fn posix_search(file: &File, account: u32) -> io::Result<bool> {
    let metadata = file.metadata()?;
    if account == 0 {
        return Ok(true);
    }
    let mode = metadata.mode();
    if metadata.uid() == account {
        return Ok(mode & 0o100 != 0);
    }
    // If unprivileged owner and other execute bits agree with group execute,
    // the result is independent of group membership.
    if (mode & 0o010 != 0) == (mode & 0o001 != 0) {
        return Ok(mode & 0o001 != 0);
    }
    let in_group = platform::in_group(account, metadata.gid())?;
    Ok(mode & if in_group { 0o010 } else { 0o001 } != 0)
}
