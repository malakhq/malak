use super::{Receipt, invalid, participant};
use malak_fs::{Directory, check_user_permissions};
use nix::fcntl::OFlag;
use nix::unistd::geteuid;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{DirBuilder, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{self, Path};

pub(super) struct Pending {
    directory: Directory,
    name: String,
    _lock: File,
}

impl Pending {
    pub(super) fn lock(
        directory: &Path,
        handover: &Path,
        inbox: &str,
        session: &str,
    ) -> io::Result<Self> {
        if !directory.is_absolute() {
            return Err(invalid("MALAK_HOOK_STATE must be absolute"));
        }
        match DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
        let directory = Directory::open(directory)?;
        check_user_permissions(directory.file(), geteuid().as_raw(), true)
            .map_err(|_| invalid("MALAK_HOOK_STATE must name a private directory"))?;
        let key = session_key(handover, inbox, session)?;
        let lock = directory.open_file(&format!("{key}.lock"), OFlag::O_RDWR | OFlag::O_CREAT)?;
        lock.lock()?;
        Ok(Self {
            directory,
            name: format!("{key}.json"),
            _lock: lock,
        })
    }

    pub(super) fn load(&self) -> io::Result<Option<Receipt>> {
        match self.directory.open_file(&self.name, OFlag::O_RDONLY) {
            Ok(file) => Ok(Some(serde_json::from_reader(io::BufReader::new(file))?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn mark_ran(&self) -> io::Result<()> {
        match self.load()? {
            Some(receipt) if !receipt.ran => self.save(&Receipt {
                ran: true,
                ..receipt
            }),
            _ => Ok(()),
        }
    }

    pub(super) fn remove(&self) -> io::Result<()> {
        self.directory.remove(&self.name)
    }

    pub(super) fn save(&self, receipt: &Receipt) -> io::Result<()> {
        let nonce = participant::random_id().map_err(io::Error::other)?;
        let temporary = format!("{}.{}.tmp", self.name, nonce);
        let file = self
            .directory
            .open_file(&temporary, OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL)?;
        let result = (|| {
            serde_json::to_writer(&file, receipt)?;
            file.sync_all()?;
            self.directory.rename(&temporary, &self.name)
        })();
        if result.is_err() {
            let _ = self.directory.remove(&temporary);
        }
        result
    }
}

fn session_key(handover: &Path, inbox: &str, session: &str) -> io::Result<String> {
    let handover = if handover.as_os_str().is_empty() {
        Path::new(".")
    } else {
        handover
    };
    let handover = path::absolute(handover)?;
    let path = handover.as_os_str().as_bytes();
    let end = path
        .iter()
        .rposition(|byte| *byte != b'/')
        .map_or(path.len(), |last| last + 1);

    // Keep the original hook's key for pending receipts and concurrent upgrades.
    // The Python recipe used json.dumps with ASCII escaping and spaced commas.
    let mut digest = Sha256::new();
    digest.update(b"[");
    hash_json_string(&mut digest, &path[..end]);
    digest.update(b", ");
    hash_json_string(&mut digest, inbox.as_bytes());
    digest.update(b", ");
    hash_json_string(&mut digest, session.as_bytes());
    digest.update(b"]");
    let mut key = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(key, "{byte:02x}").unwrap();
    }
    Ok(key)
}

fn hash_json_string(digest: &mut Sha256, value: &[u8]) {
    digest.update(b"\"");
    for chunk in value.utf8_chunks() {
        let quoted = serde_json::to_string(chunk.valid()).expect("strings serialize as JSON");
        for unit in quoted[1..quoted.len() - 1].encode_utf16() {
            if unit < 0x7f {
                digest.update([unit as u8]);
            } else {
                digest.update(format!("\\u{unit:04x}"));
            }
        }
        // Python's OS environment decoding represents invalid bytes with surrogate escapes.
        for byte in chunk.invalid() {
            digest.update(format!("\\udc{byte:02x}"));
        }
    }
    digest.update(b"\"");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn session_keys_match_the_python_recipe() {
        let cases = [
            (
                Path::new("/safe//./handover/"),
                "session",
                "ce14fa8bf6f40aebe58feda2919530213a1cc45f9dbff299e1d500a2fa9232ca",
            ),
            (
                Path::new(OsStr::from_bytes(b"//safe/a\xc3\xb1\x80/")),
                "run🚀\n\x7f",
                "48b38e03b017afd8b58d054bbb898c95e46f4bd601e4c5aafa738ed5c99c7ab7",
            ),
        ];
        for (path, session, expected) in cases {
            assert_eq!(session_key(path, "to/worker", session).unwrap(), expected);
        }
    }
}
