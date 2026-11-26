//! Own the process group of a directly launched test board.

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use std::io::{self, BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub(super) struct BoardChild {
    child: Child,
    exit: Option<ExitStatus>,
}

impl BoardChild {
    pub(super) fn spawn(command: &mut Command) -> Self {
        let child = command
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        Self { child, exit: None }
    }

    pub(super) fn wait_ready(&mut self) {
        let stdout = self.child.stdout.take().unwrap();
        let (send, ready) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(BufReader::new(stdout).lines().next().transpose());
        });
        let line = ready
            .recv_timeout(Duration::from_secs(60))
            .expect("board readiness deadline")
            .unwrap();
        assert_eq!(line.as_deref(), Some("ready"));
    }

    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn kill(&self) -> io::Result<()> {
        killpg(Pid::from_raw(self.id() as i32), Signal::SIGKILL).map_err(Into::into)
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(exit) = self.exit {
            return Ok(exit);
        }
        let exit = self.child.wait()?;
        let _ = self.kill();
        self.exit = Some(exit);
        Ok(exit)
    }
}

impl Drop for BoardChild {
    fn drop(&mut self) {
        if self.exit.is_none() {
            let _ = self.kill();
            let _ = self.child.wait();
        }
    }
}
