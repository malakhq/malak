//! Start a worker and keep a host-owned handle for its lifetime.

#![cfg(any(target_os = "linux", target_os = "macos"))]

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod cgroup;
#[cfg(target_os = "linux")]
mod vm;

#[cfg(target_os = "linux")]
pub use cgroup::start_worker;
#[cfg(target_os = "linux")]
pub use vm::{VmStop, start_vm};

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

/// The starter's local files and worker credentials for one result report.
pub struct Report {
    /// The worker's own private handover.
    pub handover: PathBuf,
    /// A stable ID for retrying this report.
    pub id: String,
    /// An existing starter-private directory dedicated to this worker run.
    pub directory: PathBuf,
    /// Absolute path to Malak's `recipes/report.sh`.
    pub recipe: PathBuf,
}

impl Report {
    fn capture(&self, command: &mut Command) -> io::Result<()> {
        let output = self.private_file("out")?;
        let errors = self.private_file("err")?;
        command
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors));
        Ok(())
    }

    fn private_file(&self, name: &str) -> io::Result<File> {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.directory.join(name))
    }

    fn send(&self, status: ExitStatus) -> io::Result<()> {
        let code = status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
        let sent = Command::new(&self.recipe)
            .env("MALAK_WORKER_HANDOVER", &self.handover)
            .env("MALAK_REPORT_ID", &self.id)
            .arg(code.to_string())
            .arg(self.directory.join("out"))
            .status()?;
        if !sent.success() {
            return Err(io::Error::other(format!(
                "worker result report failed ({sent})"
            )));
        }
        Ok(())
    }
}

/// The starter's handle for a local worker and its pending result.
#[must_use = "keep the handle to stop the worker and report its result"]
pub struct Worker {
    child: Child,
    target: Target,
    report: Report,
    exit: Option<ExitStatus>,
    reported: bool,
}

#[cfg(target_os = "macos")]
type Target = nix::unistd::Pid;
#[cfg(target_os = "linux")]
type Target = cgroup::Cgroup;

impl Worker {
    fn new(child: Child, target: Target, report: Report) -> Self {
        Self {
            child,
            target,
            report,
            exit: None,
            reported: false,
        }
    }

    /// Observe an exit without blocking on a running worker. Completion
    /// stops remaining processes in the group and sends the worker's report.
    pub fn try_finish(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.exit.is_none() && !terminated(&self.child, false)? {
            return Ok(None);
        }
        self.stop().map(Some)
    }

    /// Wait for exit, finish the group, and send the worker's report.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if self.exit.is_none() {
            terminated(&self.child, true)?;
        }
        self.stop()
    }

    /// Stop the worker's process group or cgroup and send its report.
    ///
    /// If report delivery fails, call again to retry from the saved output.
    pub fn stop(&mut self) -> io::Result<ExitStatus> {
        let status = match self.exit {
            Some(status) => status,
            None => {
                self.stop_group()?;
                let status = self.child.wait()?;
                self.exit = Some(status);
                status
            }
        };
        if !self.reported {
            self.report.send(status)?;
            self.reported = true;
        }
        Ok(status)
    }

    #[cfg(target_os = "macos")]
    fn stop_group(&mut self) -> io::Result<()> {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, killpg};
        match killpg(self.target, Signal::SIGKILL) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(Errno::EPERM) if terminated(&self.child, false)? => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(target_os = "linux")]
    fn stop_group(&mut self) -> io::Result<()> {
        self.target.stop()
    }
}

/// Start a local worker in a new process group before its program runs.
///
/// On macOS, stopping a group covers the processes that stay in it.
#[cfg(target_os = "macos")]
pub fn start_worker(mut command: Command, report: Report) -> io::Result<Worker> {
    use std::os::unix::process::CommandExt;
    report.capture(&mut command)?;
    let child = command.process_group(0).spawn()?;
    let target = nix::unistd::Pid::from_raw(child.id() as i32);
    Ok(Worker::new(child, target, report))
}

#[allow(unsafe_code)]
fn terminated(child: &Child, wait: bool) -> io::Result<bool> {
    use nix::libc;
    let flags = libc::WEXITED | libc::WNOWAIT | if wait { 0 } else { libc::WNOHANG };
    loop {
        // SAFETY: a zeroed siginfo_t provides the writable output for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: this waits on the owned child and leaves it unreaped so its
        // process group cannot be reused before it is stopped.
        if unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, flags) } == 0 {
            // SAFETY: waitid either filled SIGCHLD data or left si_pid zero.
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
