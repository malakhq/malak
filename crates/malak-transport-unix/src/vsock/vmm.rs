use crate::stream::io_error;
use malak_layers::{CreationId, Error};
use nix::libc;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Child;

/// A host-owned identity for one foreground Firecracker launch.
///
/// The starter keeps a pidfd to the actual VMM. Each captured launch is
/// assigned a fresh identity which is independent of the PID and guest CID.
pub struct Vmm {
    id: CreationId,
    pid: u32,
    uid: u32,
    pidfd: OwnedFd,
}

impl Vmm {
    /// Capture the starter's unreaped Firecracker child and expected peer UID.
    ///
    /// Use Firecracker directly or the foreground jailer when it execs in
    /// place. The child must be reaped only by its starter.
    pub fn capture(child: &mut Child, uid: u32) -> Result<Self, Error> {
        if child
            .try_wait()
            .map_err(|error| io_error("inspect VMM child", &error))?
            .is_some()
        {
            return Err(Error::unavailable("VMM child has already exited"));
        }
        let pid = child.id();
        // SAFETY: pidfd_open takes two scalar arguments and returns a fresh
        // descriptor. While the starter holds the child unreaped, its PID
        // cannot be recycled.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
        if fd < 0 {
            return Err(io_error("pin VMM child", &std::io::Error::last_os_error()));
        }
        // SAFETY: The successful syscall returned an owned file descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let mut id = [0; 16];
        getrandom::fill(&mut id).map_err(|_| Error::unavailable("could not draw a VM ID"))?;
        let vmm = Self {
            id: CreationId::from_bytes(id),
            pid,
            uid,
            pidfd,
        };
        if !vmm.live()? {
            return Err(Error::unavailable("VMM child has already exited"));
        }
        Ok(vmm)
    }

    /// Identity to bind to this VM's participant key.
    pub const fn id(&self) -> CreationId {
        self.id
    }

    /// Send a fatal signal to this exact VMM process through its saved pidfd.
    pub fn terminate(&self) -> Result<(), Error> {
        // SAFETY: the descriptor is the live or exited VMM's pidfd. The
        // syscall uses the kernel-generated signal info for a null pointer.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0u32,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(io_error("stop VMM", &error));
            }
        }
        Ok(())
    }

    pub(super) fn matches(&self, uid: Option<u32>, pid: Option<u32>) -> Result<bool, Error> {
        Ok(uid == Some(self.uid) && pid == Some(self.pid) && self.live()?)
    }

    fn live(&self) -> Result<bool, Error> {
        let mut descriptor = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one initialized descriptor for the live pidfd.
        let ready = crate::stream::retry(|| match unsafe { libc::poll(&mut descriptor, 1, 0) } {
            -1 => Err(std::io::Error::last_os_error()),
            ready => Ok(ready),
        })
        .map_err(|error| io_error("check VMM process", &error))?;
        Ok(ready == 0)
    }
}
