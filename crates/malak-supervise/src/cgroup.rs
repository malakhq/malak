use super::{Report, Worker};
use malak_fs::{check_user_permissions, trusted_parent};
use nix::libc;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Start a worker inside a fresh cgroup v2 subtree under a separate UID.
///
/// The parent must be controlled by the starter or root. The supplied worker
/// UID must have no ability to move its processes through the service manager
/// or another cgroup controller. The starter needs permission to switch UID
/// and GID. The child's initial supplementary groups are cleared.
pub fn start_worker(
    mut command: Command,
    report: Report,
    parent: &Path,
    uid: u32,
    gid: u32,
) -> io::Result<Worker> {
    report.capture(&mut command)?;
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| io::Error::other("could not make cgroup name"))?;
    let owner = nix::unistd::geteuid().as_raw();
    if uid == 0 || uid == owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "worker needs a separate unprivileged UID",
        ));
    }
    let (above, name) = trusted_parent(parent, owner)?;
    let parent_dir = above.child(name)?;
    check_user_permissions(parent_dir.file(), owner, false)?;
    if parent_dir.file().metadata()?.uid() == uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "worker controls the cgroup parent",
        ));
    }
    if nix::sys::statfs::fstatfs(&parent_dir)?.filesystem_type()
        != nix::sys::statfs::CGROUP2_SUPER_MAGIC
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a cgroup v2 parent is required",
        ));
    }
    let path = parent.join(format!("malak-{:032x}", u128::from_be_bytes(random)));
    fs::create_dir(&path)?;
    let group = match Cgroup::open(&path) {
        Ok(group) => group,
        Err(error) => {
            let _ = fs::remove_dir(&path);
            return Err(error);
        }
    };
    let procs = OpenOptions::new()
        .write(true)
        .open(path.join("cgroup.procs"))?;
    // SAFETY: the pre-exec closure makes only async-signal-safe syscalls on
    // the preopened descriptor and drops privileges before worker code runs.
    unsafe {
        command.pre_exec(move || enter(&procs, uid, gid));
    }
    let child = command.spawn()?;
    Ok(Worker::new(child, group, report))
}

fn enter(procs: &File, uid: u32, gid: u32) -> io::Result<()> {
    let bytes = b"0\n";
    // SAFETY: procs is a live cgroup.procs descriptor. The write moves this
    // trusted child, then privilege calls limit the identity before exec.
    unsafe {
        if libc::write(procs.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
            != bytes.len() as isize
            || libc::syscall(libc::SYS_setgroups, 0usize, std::ptr::null::<libc::gid_t>()) != 0
            || libc::syscall(libc::SYS_setresgid, gid, gid, gid) != 0
            || libc::syscall(libc::SYS_setresuid, uid, uid, uid) != 0
            || libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub(super) struct Cgroup {
    path: PathBuf,
    kill: File,
    events: File,
}

impl Cgroup {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.into(),
            kill: OpenOptions::new()
                .write(true)
                .open(path.join("cgroup.kill"))?,
            events: File::open(path.join("cgroup.events"))?,
        })
    }

    pub(super) fn stop(&mut self) -> io::Result<()> {
        self.kill.write_all(b"1")?;
        let until = Instant::now() + Duration::from_secs(10);
        let mut state = String::new();
        loop {
            state.clear();
            self.events.rewind()?;
            self.events.read_to_string(&mut state)?;
            if state.lines().any(|line| line == "populated 0") {
                return fs::remove_dir(&self.path);
            }
            if Instant::now() >= until {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "worker cgroup is still populated",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}
