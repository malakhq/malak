//! Private fixtures for Unix sockets and files subject to ancestor checks.

use malak_fs::{Directory, check_user_permissions, trusted_parent};
use nix::unistd::{User, geteuid};
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

// The fixture's stdin closes even when its test process cannot run Drop.
const REMOVE_HOME: &str = r#"
    IFS= read -r unused
    /bin/rm -rf "$1" || exit
    /bin/rmdir "${1%/*}" 2>/dev/null || :
"#;

/// A private per-test directory that removes its contents when dropped.
pub struct SocketDir {
    path: PathBuf,
    watcher: Option<Child>,
}

impl SocketDir {
    /// Create a fixture under the current account's home from the OS account database.
    ///
    /// The resolved home and its ancestors must satisfy the same write-access
    /// check as handover paths. The fixture fits a Unix socket address even
    /// with the CLI's listener subdirectories.
    pub fn new() -> io::Result<Self> {
        Self::with_cleanup(None, &mut cleanup_command())
    }

    /// Create a private fixture in the short test-only folder of a trusted directory.
    pub fn under(home: &Path) -> io::Result<Self> {
        Self::with_cleanup(Some(home), &mut cleanup_command())
    }

    /// Create a fixture with a custom cleanup command for out-of-process tests.
    ///
    /// The fixture path is appended to the command. The command runs in its own
    /// group and must wait for EOF on stdin before cleaning up the fixture. Use
    /// the account home when `home` is omitted.
    pub fn with_cleanup(home: Option<&Path>, watcher: &mut Command) -> io::Result<Self> {
        let account_home;
        let home = if let Some(home) = home {
            home
        } else {
            account_home = User::from_uid(geteuid())?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "test account has no home"))?
                .dir
                .canonicalize()?;
            &account_home
        };
        let uid = geteuid().as_raw();
        let parent = home.join(".m");
        loop {
            trusted_parent(&parent, uid)?;
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{:x}-{id:x}", std::process::id()));
            SocketAddr::from_pathname(path.join("d/o/s"))?;
            match create_fixture(&parent, &path, uid) {
                Ok(()) => {
                    let mut fixture = Self {
                        path,
                        watcher: None,
                    };
                    let directory = Directory::open(fixture.path())?;
                    check_user_permissions(directory.file(), uid, true)?;
                    fixture.watcher = Some(
                        watcher
                            .arg(fixture.path())
                            .stdin(Stdio::piped())
                            .process_group(0)
                            .spawn()?,
                    );
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
        &self.path
    }

    /// Return the address of the test's Unix socket.
    pub fn address(&self) -> String {
        format!("unix:{}/s", self.path.display())
    }
}

fn cleanup_command() -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", REMOVE_HOME, "malak-test-home"]);
    command
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
        if let Some(watcher) = &mut self.watcher {
            drop(watcher.stdin.take());
            match watcher.wait() {
                Ok(status) if status.success() => {}
                result => eprintln!(
                    "test fixture cleanup failed for {:?}: {result:?}",
                    self.path
                ),
            }
        }
        let _ = fs::remove_dir_all(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{kill_after_checkpoint, pause_at_checkpoint};
    use std::time::{Duration, Instant};

    const INTERRUPT_CHECKPOINT: &str = "MALAK_TEST_SOCKET_DIR_CHECKPOINT";

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

    #[test]
    fn a_socket_fixture_cleans_up_when_the_test_process_is_killed() {
        let scratch = SocketDir::new().unwrap();
        let checkpoint = scratch.path().join("checkpoint");
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "unix_socket::tests::interruption_child"])
            .env(INTERRUPT_CHECKPOINT, &checkpoint);
        kill_after_checkpoint(&mut child, &checkpoint);

        let deadline = Instant::now() + Duration::from_secs(5);
        while scratch.path().join(".m").exists() {
            assert!(Instant::now() < deadline, "the child fixture survived");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn interruption_child() {
        if let Some(checkpoint) = std::env::var_os(INTERRUPT_CHECKPOINT) {
            let checkpoint = Path::new(&checkpoint);
            let fixture = SocketDir::under(checkpoint.parent().unwrap()).unwrap();
            fs::write(fixture.path().join("fixture-data"), b"test").unwrap();
            pause_at_checkpoint(checkpoint);
        }
    }
}
