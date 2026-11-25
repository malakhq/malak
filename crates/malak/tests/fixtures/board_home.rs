//! Own a private test home and any boards started from it.

use malak_testkit::unix_socket::SocketDir;
use std::path::Path;
use std::process::Command;

pub(super) struct BoardHome(SocketDir);

impl BoardHome {
    pub(super) fn new() -> Self {
        Self::under(None)
    }

    pub(super) fn under(home: Option<&Path>) -> Self {
        let mut watcher = Command::new("python3");
        watcher.arg("-B").arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/board_processes.py"
        ));
        Self(SocketDir::with_cleanup(home, &mut watcher).expect("start the test board watcher"))
    }

    pub(super) fn path(&self) -> &Path {
        self.0.path()
    }
}
