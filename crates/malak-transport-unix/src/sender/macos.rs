use super::{Process, ProcessIdentity};
use nix::libc;
use std::mem::MaybeUninit;

pub(super) fn cwd(pid: u32) -> Option<Box<str>> {
    // SAFETY: This flavor writes a proc_vnodepathinfo.
    let info = unsafe { pid_info::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO) }?;
    let bytes = info.pvi_cdir.vip_path.as_flattened();
    text(bytes)
}

pub(super) fn program(pid: u32) -> Option<Box<str>> {
    let mut name = [0; 32];
    // SAFETY: libproc receives a writable buffer of the specified size.
    let read = unsafe {
        libc::proc_name(
            pid.try_into().ok()?,
            name.as_mut_ptr().cast(),
            name.len() as u32,
        )
    };
    (read > 0).then(|| text(&name)).flatten()
}

pub(super) fn process(pid: u32) -> Option<Process> {
    // SAFETY: This flavor writes a proc_bsdinfo.
    let info = unsafe { pid_info::<libc::proc_bsdinfo>(pid, libc::PROC_PIDTBSDINFO) }?;
    let start_time = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
    Some(Process {
        identity: ProcessIdentity { pid, start_time },
        parent: info.pbi_ppid,
        program: text(&info.pbi_name).or_else(|| text(&info.pbi_comm)),
    })
}

// T must be the libc result struct for this flavor and accept all bit patterns.
unsafe fn pid_info<T>(pid: u32, flavor: i32) -> Option<T> {
    let mut info = MaybeUninit::<T>::zeroed();
    let size = size_of::<T>() as i32;
    // SAFETY: The caller selects the matching flavor and libc struct.
    let read = unsafe {
        libc::proc_pidinfo(
            pid.try_into().ok()?,
            flavor,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    // SAFETY: A full result initialized the struct, which accepts all bit patterns.
    (read == size).then(|| unsafe { info.assume_init() })
}

fn text(bytes: &[libc::c_char]) -> Option<Box<str>> {
    let bytes: Vec<_> = bytes
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into())
}
