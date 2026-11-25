use super::{Process, ProcessIdentity, interpreter_name};
use std::fs;
use std::path::Path;

#[cfg(target_os = "linux")]
pub(super) fn cwd(pid: u32) -> Option<Box<str>> {
    let path = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    Some(path.to_string_lossy().into())
}

#[cfg(target_os = "linux")]
pub(super) fn program(pid: u32) -> Option<Box<str>> {
    let name = fs::read(format!("/proc/{pid}/comm")).ok()?;
    let name = name.strip_suffix(b"\n").unwrap_or(&name);
    (!name.is_empty()).then(|| String::from_utf8_lossy(name).into())
}

#[cfg(target_os = "linux")]
pub(super) fn process(pid: u32) -> Option<Process> {
    process_in(Path::new("/proc"), pid)
}

fn process_in(procfs: &Path, pid: u32) -> Option<Process> {
    let stat = fs::read(procfs.join(format!("{pid}/stat"))).ok()?;
    let stat = String::from_utf8_lossy(&stat);
    // The parenthesized comm can itself contain a closing parenthesis or spaces.
    let (comm, fields) = stat.rsplit_once(')')?;
    let (_, comm) = comm.split_once('(')?;
    let mut fields = fields.split_ascii_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    let parent = fields.next()?.parse().ok()?; // Field 4.
    let start_time = fields.nth(17)?.parse().ok()?; // Field 22.
    Some(Process {
        identity: ProcessIdentity { pid, start_time },
        parent,
        program: (!comm.is_empty()).then(|| comm.into()),
    })
}

#[cfg(target_os = "linux")]
pub(super) fn runs_interpreter(pid: u32) -> bool {
    runs_interpreter_in(Path::new("/proc"), pid)
}

fn runs_interpreter_in(procfs: &Path, pid: u32) -> bool {
    // A direct shebang exec leaves the script name in comm. The executable
    // link identifies the interpreter without changing the displayed name.
    let Ok(exe) = fs::read_link(procfs.join(format!("{pid}/exe"))) else {
        return false;
    };
    exe.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(interpreter_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sender::walk;
    use malak_layers::{SenderPeer, SessionProcess};
    use malak_testkit::unix_socket::SocketDir;
    use std::os::unix::fs::symlink;

    #[test]
    fn shell_recipe_send_resolves_to_the_lead_up_session() {
        let procfs = procfs(&[
            (10, 1, "recipes-test", None),
            (11, 10, "malak", None),
            (12, 11, "python3", None),
            (13, 12, "malak", None),
            (14, 10, "malak", None),
            (15, 14, "ask", Some("/usr/bin/dash")),
            (16, 15, "ask", Some("/usr/bin/dash")),
            (17, 16, "malak", None),
        ]);

        assert_eq!(session(&procfs, 17), session(&procfs, 13));
    }

    #[test]
    fn python_shebang_recipe_resolves_to_its_agent_session() {
        let procfs = procfs(&[
            (10, 1, "codex", None),
            (11, 10, "recipe.py", Some("/usr/bin/python3.12")),
            (12, 11, "malak", None),
        ]);

        assert_eq!(session(&procfs, 12).identity.pid, 10);
    }

    #[test]
    fn shells_under_separate_agents_resolve_to_separate_sessions() {
        let procfs = procfs(&[
            (10, 1, "launcher", None),
            (11, 10, "codex", None),
            (12, 11, "ask", Some("/usr/bin/dash")),
            (13, 12, "malak", None),
            (14, 10, "codex", None),
            (15, 14, "ask", Some("/usr/bin/dash")),
            (16, 15, "malak", None),
        ]);

        let lead = session(&procfs, 13);
        let other = resolve(&procfs, 16);
        assert_eq!(lead.identity.pid, 11);
        assert_eq!(other.trace.session.unwrap().identity.pid, 14);
        assert!(!other.ancestry.contains(&lead.identity));
    }

    #[test]
    fn a_native_binary_named_ask_is_a_session_boundary() {
        let procfs = procfs(&[
            (10, 1, "codex", None),
            (11, 10, "ask", Some("/opt/bin/ask")),
            (12, 11, "malak", None),
        ]);

        assert_eq!(session(&procfs, 12).identity.pid, 11);
    }

    fn procfs(processes: &[(u32, u32, &str, Option<&str>)]) -> SocketDir {
        let procfs = SocketDir::new().unwrap();
        for &(pid, parent, name, executable) in processes {
            let entry = procfs.path().join(pid.to_string());
            fs::create_dir(&entry).unwrap();
            // Fill stat fields 5 through 21 before the start time in field 22.
            let start_time = u64::from(pid) * 100;
            let fields = "0 ".repeat(17);
            fs::write(
                entry.join("stat"),
                format!("{pid} ({name}) S {parent} {fields}{start_time}\n"),
            )
            .unwrap();
            if let Some(executable) = executable {
                symlink(executable, entry.join("exe")).unwrap();
            }
        }
        procfs
    }

    fn session(procfs: &SocketDir, pid: u32) -> SessionProcess {
        resolve(procfs, pid).trace.session.unwrap()
    }

    fn resolve(procfs: &SocketDir, pid: u32) -> SenderPeer {
        let mut peer = SenderPeer::transport("unix");
        walk(
            &mut peer,
            pid,
            |pid| process_in(procfs.path(), pid),
            |pid| runs_interpreter_in(procfs.path(), pid),
        );
        peer
    }
}
