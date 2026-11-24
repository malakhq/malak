use malak_transport_unix::vsock::Vmm;
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;

/// Host handle for the actual Firecracker process and its verified identity.
#[must_use = "keep the stop handle with the starter"]
pub struct VmStop {
    child: Child,
    identity: Arc<Vmm>,
}

/// Start Firecracker, or the foreground jailer that execs into Firecracker.
pub fn start_vm(mut command: Command, vmm_uid: u32) -> io::Result<VmStop> {
    let mut child = command.spawn()?;
    match Vmm::capture(&mut child, vmm_uid) {
        Ok(identity) => Ok(VmStop {
            child,
            identity: Arc::new(identity),
        }),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(io::Error::other(error))
        }
    }
}

impl VmStop {
    /// Evidence to pass to the host's verified vsock route.
    pub fn identity(&self) -> Arc<Vmm> {
        self.identity.clone()
    }

    /// Signal the saved pidfd and wait for the VMM's actual exit status.
    pub fn stop(&mut self) -> io::Result<ExitStatus> {
        self.identity.terminate().map_err(io::Error::other)?;
        self.wait()
    }

    /// Wait for the VMM exit through its host child handle.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Observe whether the VMM exited without blocking.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}
