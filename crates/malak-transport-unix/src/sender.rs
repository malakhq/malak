use malak_layers::{ProcessIdentity, SenderPeer, SessionProcess};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(target_os = "macos")]
use macos as os;

const MAX_ANCESTORS: usize = 64;

struct Process {
    identity: ProcessIdentity,
    parent: u32,
    program: Option<Box<str>>,
}

impl Process {
    fn session(self) -> Option<SessionProcess> {
        Some(SessionProcess {
            identity: self.identity,
            program: self.program?,
        })
    }
}

pub(super) fn resolve(pid: Option<u32>) -> SenderPeer {
    let mut peer = SenderPeer::transport("unix");
    peer.trace.pid = pid.filter(|&pid| pid > 0);
    if let Some(pid) = peer.trace.pid {
        peer.trace.program = os::program(pid);
        peer.trace.cwd = os::cwd(pid).filter(|cwd| cwd.len() <= 4096);
        walk(&mut peer, pid, os::process);
    }
    peer
}

pub(super) fn local_session_process() -> Option<SessionProcess> {
    let mut peer = SenderPeer::transport("unix");
    walk(&mut peer, std::process::id(), os::process);
    peer.trace.session
}

fn walk(peer: &mut SenderPeer, pid: u32, read: impl Fn(u32) -> Option<Process>) {
    let Some(mut process) = read(pid) else { return };
    for _ in 0..MAX_ANCESTORS {
        peer.ancestry.push(process.identity);
        if process.parent == 1 {
            peer.trace.session = process.session();
            return;
        }
        let Some(parent) = read(process.parent) else {
            return;
        };
        if parent.identity.start_time > process.identity.start_time
            || peer.ancestry.contains(&parent.identity)
        {
            return;
        }
        match parent.program.as_deref() {
            Some(name) if wrapper(name) => process = parent,
            Some(_) => {
                peer.trace.session = parent.session();
                return;
            }
            None => return,
        }
    }
}

fn wrapper(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash" | "zsh" | "dash" | "fish" | "env" | "timeout" | "nohup" | "malak"
    ) || name.starts_with("python")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_current_process() {
        let pid = std::process::id();
        let peer = resolve(Some(pid));

        assert_eq!(peer.trace.pid, Some(pid));
        assert!(peer.trace.program.is_some());
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert_eq!(
            std::path::Path::new(peer.trace.cwd.as_deref().unwrap())
                .canonicalize()
                .unwrap(),
            cwd
        );
        assert_eq!(peer.ancestry[0].pid, pid);
        assert!(peer.ancestry[0].start_time > 0);
    }

    #[test]
    fn chooses_the_first_non_wrapper_ancestor() {
        let mut peer = SenderPeer::transport("unix");

        walk(&mut peer, 4, |pid| match pid {
            4 => Some(process(pid, 3, "malak")),
            3 => Some(process(pid, 2, "zsh")),
            2 => Some(process(pid, 1, "claude")),
            _ => None,
        });

        assert_eq!(peer.trace.session.unwrap().identity.pid, 2);
    }

    #[test]
    fn keeps_the_detached_worker_as_the_session() {
        let mut peer = SenderPeer::transport("unix");

        walk(&mut peer, 4, |pid| match pid {
            4 => Some(process(pid, 3, "malak")),
            3 => Some(process(pid, 1, "python3")),
            _ => None,
        });

        assert_eq!(peer.trace.session.unwrap().identity.pid, 3);
    }

    fn process(pid: u32, parent: u32, name: &str) -> Process {
        Process {
            identity: ProcessIdentity {
                pid,
                start_time: u64::from(pid),
            },
            parent,
            program: Some(name.into()),
        }
    }
}
