use super::HandoverFile;
use crate::{Error, random};
use malak_fs::{
    AccountAccess, Directory, check_credential, check_user_permissions, open_credential,
    share_with, sync, trusted_parent, trusted_parent_for_account,
};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::fstatat;
use nix::unistd::{AccessFlags, faccessat, geteuid};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

type CredentialCheck<'a> = dyn Fn(&[u8]) -> bool + 'a;

const NEW: OFlag = OFlag::O_WRONLY.union(OFlag::O_CREAT).union(OFlag::O_EXCL);
const TEMPORARY_PREFIX: &str = ".malak-handover-";

/// Whether to create a handover or replace it atomically.
#[derive(Clone, Copy, Debug)]
pub enum HandoverWrite {
    /// Create a new file and refuse any existing destination.
    New,
    /// Write a complete sibling and atomically move it to the destination.
    Replace,
}

/// A checked handover destination pinned to the directory's descriptor.
pub struct HandoverTarget {
    folder: Directory,
    name: String,
    path: PathBuf,
    mode: HandoverWrite,
    owner: u32,
    reader: u32,
}

impl HandoverTarget {
    /// Recognize the name of an unpublished handover in its destination directory.
    pub fn manages_temporary(name: &str) -> bool {
        name.strip_prefix(TEMPORARY_PREFIX).is_some_and(|suffix| {
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
    }

    /// Check the path before an enrolment or key replacement is saved.
    ///
    /// The supplied UID is the recipient. For a separate account, the board
    /// checks its search access and installs a named read ACL before writing.
    pub fn prepare(path: impl AsRef<Path>, uid: u32, mode: HandoverWrite) -> Result<Self, Error> {
        Self::prepare_inner(path.as_ref(), uid, mode, None)
    }

    pub(crate) fn prepare_resume(
        path: &Path,
        uid: u32,
        accept: &CredentialCheck<'_>,
    ) -> Result<Self, Error> {
        Self::prepare_inner(path, uid, HandoverWrite::New, Some(accept))
    }

    fn prepare_inner(
        input: &Path,
        uid: u32,
        mode: HandoverWrite,
        accept: Option<&CredentialCheck<'_>>,
    ) -> Result<Self, Error> {
        let owner = geteuid().as_raw();
        let path = std::path::absolute(input).map_err(report)?;
        let (folder, name) = trusted_parent_for_account(&path, owner, uid).map_err(report)?;
        faccessat(
            &folder,
            ".",
            AccessFlags::W_OK | AccessFlags::X_OK,
            AtFlags::AT_EACCESS,
        )
        .map_err(|error| report(error.into()))?;
        let name = name
            .to_str()
            .ok_or_else(|| Error::refused("handover name must be UTF-8"))?;
        let mode = match fstatat(&folder, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Err(Errno::ENOENT) => mode,
            Ok(_) if accept.is_none() && matches!(mode, HandoverWrite::New) => {
                return Err(Error::refused("handover file already exists"));
            }
            Ok(_) => {
                let file = folder.open_file(name, OFlag::O_RDONLY).map_err(report)?;
                check_credential(&file, owner, uid).map_err(report)?;
                if let Some(accept) = accept {
                    let mut bytes = Zeroizing::new(Vec::new());
                    file.take(4097).read_to_end(&mut bytes).map_err(report)?;
                    if bytes.len() > 4096 || !accept(&bytes) {
                        return Err(Error::refused(
                            "existing credential does not match the board startup",
                        ));
                    }
                }
                HandoverWrite::Replace
            }
            Err(error) => return Err(report(error.into())),
        };
        Ok(Self {
            folder,
            name: name.into(),
            path,
            mode,
            owner,
            reader: uid,
        })
    }

    pub(crate) fn exists(&self) -> bool {
        matches!(self.mode, HandoverWrite::Replace)
    }

    pub(crate) fn confirm(&self) -> Result<(), Error> {
        self.folder.sync().map_err(report)
    }

    pub(crate) fn same_destination(&self, other: &Self) -> Result<bool, Error> {
        if self.name != other.name {
            return Ok(false);
        }
        let left = self.folder.file().metadata().map_err(report)?;
        let right = other.folder.file().metadata().map_err(report)?;
        Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
    }

    /// Save the handover after the board's authority save.
    ///
    /// A successful result means both file and directory reached stable storage.
    pub fn write(self, value: &HandoverFile) -> Result<PathBuf, Error> {
        self.save(&value.bytes())
    }

    pub(crate) fn save(self, bytes: &[u8]) -> Result<PathBuf, Error> {
        let temp = format!("{TEMPORARY_PREFIX}{:032x}", u128::from_be_bytes(random()?));
        self.write_new(&temp, bytes).map_err(report)?;
        #[cfg(test)]
        crate::enrol_kill::checkpoint(&self.path, "temporary");
        #[cfg(test)]
        crate::durable_kill::handover(&self.path, "temporary");
        let placed = match self.mode {
            HandoverWrite::New => self.folder.rename_to_new(&temp, &self.name),
            HandoverWrite::Replace => self.folder.rename(&temp, &self.name),
        };
        if let Err(error) = placed {
            let _ = self.folder.remove(&temp);
            return Err(report(error));
        }
        #[cfg(test)]
        crate::enrol_kill::checkpoint(&self.path, "published");
        #[cfg(test)]
        crate::durable_kill::handover(&self.path, "published");
        self.confirm()?;
        #[cfg(test)]
        crate::enrol_kill::checkpoint(&self.path, "directory");
        #[cfg(test)]
        crate::durable_kill::handover(&self.path, "directory");
        Ok(self.path)
    }

    fn write_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut file = self.folder.open_file(name, NEW)?;
        let permissions = if self.owner == self.reader {
            Ok(())
        } else {
            share_with(&file, self.reader, AccountAccess::Read)
        };
        let result = permissions
            .and_then(|()| check_credential(&file, self.owner, self.reader))
            .and_then(|()| file.write_all(bytes))
            .and_then(|()| sync(&file));
        if result.is_err() {
            let _ = self.folder.remove(name);
        }
        result
    }
}

pub(super) fn read(path: &Path) -> Result<HandoverFile, Error> {
    let absolute = std::path::absolute(path).map_err(report)?;
    let file = open_credential(&absolute, geteuid().as_raw()).map_err(report)?;
    HandoverFile::decode(&read_private_bytes(file)?)
}

pub(super) fn read_bytes(path: &Path) -> Result<Zeroizing<Vec<u8>>, Error> {
    let absolute = std::path::absolute(path).map_err(report)?;
    let uid = geteuid().as_raw();
    let (folder, name) = trusted_parent(&absolute, uid).map_err(report)?;
    let name = name
        .to_str()
        .ok_or_else(|| Error::refused("handover name must be UTF-8"))?;
    let file = folder.open_file(name, OFlag::O_RDONLY).map_err(report)?;
    check_user_permissions(&file, uid, true).map_err(report)?;
    read_private_bytes(file)
}

fn read_private_bytes(file: std::fs::File) -> Result<Zeroizing<Vec<u8>>, Error> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(4097).read_to_end(&mut bytes).map_err(report)?;
    Ok(bytes)
}

fn report(error: io::Error) -> Error {
    let message = format!("handover file: {error}");
    if error.raw_os_error() == Some(Errno::ELOOP as i32) {
        return Error::refused(message);
    }
    match error.kind() {
        io::ErrorKind::PermissionDenied
        | io::ErrorKind::AlreadyExists
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::NotADirectory
        | io::ErrorKind::NotFound => Error::refused(message),
        _ => Error::unavailable(message),
    }
}
