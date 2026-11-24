use super::{Process, ProcessIdentity};
use std::fs;

pub(super) fn cwd(pid: u32) -> Option<Box<str>> {
    let path = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    Some(path.to_string_lossy().into())
}

pub(super) fn program(pid: u32) -> Option<Box<str>> {
    let name = fs::read(format!("/proc/{pid}/comm")).ok()?;
    let name = name.strip_suffix(b"\n").unwrap_or(&name);
    (!name.is_empty()).then(|| String::from_utf8_lossy(name).into())
}

pub(super) fn process(pid: u32) -> Option<Process> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let stat = String::from_utf8_lossy(&stat);
    // The parenthesized comm can itself contain a closing parenthesis or spaces.
    let (comm, fields) = stat.rsplit_once(')')?;
    let (_, comm) = comm.split_once('(')?;
    let mut fields = fields.split_ascii_whitespace().skip(1); // Skip state (field 3).
    let parent = fields.next()?.parse().ok()?; // Field 4.
    let start_time = fields.nth(17)?.parse().ok()?; // Field 22.
    Some(Process {
        identity: ProcessIdentity { pid, start_time },
        parent,
        program: (!comm.is_empty()).then(|| comm.into()),
    })
}
