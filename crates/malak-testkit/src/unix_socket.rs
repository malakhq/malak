//! Private fixtures for Unix sockets and files subject to ancestor checks.

use malak_fs::{Directory, check_user_permissions, trusted_parent};
use nix::unistd::{User, geteuid};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A private per-test directory that removes its contents when dropped.
pub struct SocketDir(PathBuf);

impl SocketDir {
    /// Create a fixture under the current account's home from the OS account database.
    ///
    /// The resolved home and its ancestors must satisfy the same write-access
    /// check as handover paths. The fixture fits a Unix socket address even
    /// with the CLI's listener subdirectories.
    pub fn new() -> io::Result<Self> {
        let uid = geteuid();
        let home = User::from_uid(uid)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "test account has no home"))?
            .dir
            .canonicalize()?;
        Self::under(&home)
    }

    /// Create a private fixture in the short test-only folder of a trusted directory.
    pub fn under(home: &Path) -> io::Result<Self> {
        let uid = geteuid().as_raw();
        let parent = home.join(".m");
        loop {
            trusted_parent(&parent, uid)?;
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{:x}-{id:x}", std::process::id()));
            SocketAddr::from_pathname(path.join("d/o/s"))?;
            match create_fixture(&parent, &path, uid) {
                Ok(()) => {
                    let fixture = Self(path);
                    let directory = Directory::open(fixture.path())?;
                    check_user_permissions(directory.file(), uid, true)?;
                    return Ok(fixture);
                }
                // Another fixture may remove the empty parent, or another
                // process may already own this leaf.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::AlreadyExists
                    ) => {}
                Err(error) => {
                    let _ = fs::remove_dir(&parent);
                    return Err(error);
                }
            }
        }
    }

    /// Return the private directory used by the fixture.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Return the address of the test's Unix socket.
    pub fn address(&self) -> String {
        format!("unix:{}/s", self.0.display())
    }
}

fn create_fixture(parent: &Path, path: &Path, uid: u32) -> io::Result<()> {
    match DirBuilder::new().mode(0o711).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let directory = Directory::open(parent)?;
    check_user_permissions(directory.file(), uid, false)?;
    let mode = directory.file().metadata()?.permissions().mode();
    // The separate-account tests grant access to individual fixtures. Their
    // parent needs search permission even when the suite runs with umask 077.
    if mode & 0o777 != 0o711 {
        directory
            .file()
            .set_permissions(fs::Permissions::from_mode(0o711))?;
    }
    DirBuilder::new().mode(0o700).create(path)
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
        if let Some(parent) = self.0.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_socket_fixture_uses_a_private_directory_and_cleans_up() {
        let socket = SocketDir::new().unwrap();
        let path = socket.path().to_owned();
        let second = SocketDir::new().unwrap();

        assert_ne!(socket.address(), second.address());
        trusted_parent(&path.join("s"), geteuid().as_raw()).unwrap();
        SocketAddr::from_pathname(path.join("d/o/s")).unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o700);
        drop(socket);

        assert!(!path.exists());
    }

    #[test]
    fn a_socket_fixture_cleans_up_its_contents_and_parent_during_unwind() {
        let root = SocketDir::new().unwrap();
        let parent = root.path().join(".m");
        let unwind = std::panic::catch_unwind(|| {
            let fixture = SocketDir::under(root.path()).unwrap();
            fs::write(fixture.path().join("fixture-data"), b"test").unwrap();
            std::panic::panic_any(fixture.path().to_owned());
        });

        let path = *unwind.unwrap_err().downcast::<PathBuf>().unwrap();
        assert_eq!(path.parent(), Some(parent.as_path()));
        assert!(!parent.exists());
    }
}
