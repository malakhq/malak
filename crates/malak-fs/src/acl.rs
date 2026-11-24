//! Descriptor-bound extended ACL inspection on Darwin.

use super::shared::{AccountAccess, denied, posix_search};
use nix::errno::Errno;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::ptr::{NonNull, null_mut};

// Darwin definitions in sys/acl.h. ACL descriptors are owned by libc.
const ACL_TYPE_EXTENDED: c_int = 0x100;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_EXTENDED_ALLOW: c_int = 1;
const ACL_EXTENDED_DENY: c_int = 2;
const READ: u64 = 1 << 1;
const SEARCH: u64 = 1 << 3;
const UID: c_int = 0;
const ONLY_INHERIT: c_int = 1 << 8;
// Write data, delete, append, delete child, write attributes or xattrs,
// change permissions, and change owner.
const ALTER: u64 =
    (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13);

unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
    fn acl_free(acl: *mut c_void) -> c_int;
    fn acl_get_entry(acl: *mut c_void, id: c_int, entry: *mut *mut c_void) -> c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
    fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> c_int;
    fn acl_init(count: c_int) -> *mut c_void;
    fn acl_create_entry(acl: *mut *mut c_void, entry: *mut *mut c_void) -> c_int;
    fn acl_set_tag_type(entry: *mut c_void, tag: c_int) -> c_int;
    fn acl_set_qualifier(entry: *mut c_void, uuid: *const u8) -> c_int;
    fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
    fn acl_set_permset_mask_np(entry: *mut c_void, mask: u64) -> c_int;
    fn acl_set_fd_np(fd: c_int, acl: *mut c_void, acl_type: c_int) -> c_int;
    fn acl_get_flagset_np(entry: *mut c_void, flags: *mut *mut c_void) -> c_int;
    fn acl_get_flag_np(flags: *mut c_void, flag: c_int) -> c_int;
    fn mbr_gid_to_uuid(gid: u32, uuid: *mut u8) -> c_int;
    fn mbr_uid_to_uuid(uid: u32, uuid: *mut u8) -> c_int;
    fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, id_type: *mut c_int) -> c_int;
    fn mbr_check_membership(user: *const u8, group: *const u8, member: *mut c_int) -> c_int;
}

struct Acl(NonNull<c_void>);

impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: This is the owned result of acl_get_fd_np and is freed once.
        unsafe { acl_free(self.0.as_ptr()) };
    }
}

pub(super) fn check(file: &File, private: bool) -> io::Result<()> {
    entries(file, |_, tag, rights| {
        if tag != ACL_EXTENDED_DENY && (private || rights & ALTER != 0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "extended ACL grants access",
            ));
        }
        Ok(())
    })
}

pub(super) fn share_with(file: &File, account: u32, access: AccountAccess) -> io::Result<()> {
    if file.metadata()?.mode() & 0o077 != 0 {
        return Err(denied());
    }
    let uuid = user_uuid(account)?;
    let rights = match access {
        AccountAccess::Read => READ,
        AccountAccess::Search => SEARCH,
    };
    // SAFETY: The nonnull result is an allocated ACL owned by this function.
    let mut raw = NonNull::new(unsafe { acl_init(1) })
        .ok_or_else(io::Error::last_os_error)?
        .as_ptr();
    let mut entry = null_mut();
    // SAFETY: raw points to the owned ACL and may be replaced by acl_create_entry.
    let created = unsafe { acl_create_entry(&mut raw, &mut entry) };
    let acl = Acl(NonNull::new(raw).expect("allocated ACL"));
    if created != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: entry belongs to the live ACL. The qualifier is a 16-byte UUID.
    if unsafe {
        acl_set_tag_type(entry, ACL_EXTENDED_ALLOW) != 0
            || acl_set_qualifier(entry, uuid.as_ptr()) != 0
            || acl_set_permset_mask_np(entry, rights) != 0
            || acl_set_fd_np(file.as_raw_fd(), acl.0.as_ptr(), ACL_TYPE_EXTENDED) != 0
    } {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn sole_reader(file: &File) -> io::Result<Option<u32>> {
    let mut reader = None;
    entries(file, |entry, tag, rights| {
        if tag == ACL_EXTENDED_DENY {
            return Ok(());
        }
        if tag != ACL_EXTENDED_ALLOW || rights != READ || reader.is_some() || inherit_only(entry)? {
            return Err(denied());
        }
        let subject = qualifier(entry)?;
        let (uid, kind) = identity(&subject)?;
        if kind != UID || user_uuid(uid)? != subject {
            return Err(denied());
        }
        reader = Some(uid);
        Ok(())
    })?;
    let mode = file.metadata()?.mode();
    if reader.is_some() && (mode & 0o077 != 0 || mode & 0o400 == 0) {
        return Err(denied());
    }
    Ok(reader)
}

pub(super) fn can_traverse(file: &File, account: u32) -> io::Result<bool> {
    if account == 0 {
        return Ok(true);
    }
    let mut result = None;
    let mut user = None;
    entries(file, |entry, tag, rights| {
        if result.is_some() || rights & SEARCH == 0 || inherit_only(entry)? {
            return Ok(());
        }
        let subject = qualifier(entry)?;
        let who = match user {
            Some(user) => user,
            None => {
                let uuid = user_uuid(account)?;
                user = Some(uuid);
                uuid
            }
        };
        let (_, kind) = identity(&subject)?;
        if subject == who || (kind != UID && is_member(&who, &subject)?) {
            result = Some(tag == ACL_EXTENDED_ALLOW);
        }
        Ok(())
    })?;
    match result {
        Some(allowed) => Ok(allowed),
        None => posix_search(file, account),
    }
}

pub(super) fn in_group(uid: u32, gid: u32) -> io::Result<bool> {
    let user = user_uuid(uid)?;
    let mut group = [0; 16];
    // SAFETY: mbr_gid_to_uuid writes into a 16-byte group UUID buffer.
    system(unsafe { mbr_gid_to_uuid(gid, group.as_mut_ptr()) })?;
    is_member(&user, &group)
}

fn is_member(user: &[u8; 16], group: &[u8; 16]) -> io::Result<bool> {
    let mut member = 0;
    // SAFETY: both UUIDs and the integer output remain live for the call.
    system(unsafe { mbr_check_membership(user.as_ptr(), group.as_ptr(), &mut member) })?;
    Ok(member != 0)
}

fn inherit_only(entry: *mut c_void) -> io::Result<bool> {
    let mut flags = null_mut();
    // SAFETY: entry belongs to a live ACL, from which flags are borrowed.
    if unsafe { acl_get_flagset_np(entry, &mut flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    match unsafe { acl_get_flag_np(flags, ONLY_INHERIT) } {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(io::Error::last_os_error()),
    }
}

fn user_uuid(uid: u32) -> io::Result<[u8; 16]> {
    let mut result = [0; 16];
    // SAFETY: mbr_uid_to_uuid writes exactly the supplied 16-byte UUID.
    system(unsafe { mbr_uid_to_uuid(uid, result.as_mut_ptr()) })?;
    Ok(result)
}

fn identity(uuid: &[u8; 16]) -> io::Result<(u32, c_int)> {
    let (mut id, mut kind) = (0, 0);
    // SAFETY: the input is a UUID and the outputs match the C parameter types.
    system(unsafe { mbr_uuid_to_id(uuid.as_ptr(), &mut id, &mut kind) })?;
    Ok((id, kind))
}

fn system(code: c_int) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

fn qualifier(entry: *mut c_void) -> io::Result<[u8; 16]> {
    // SAFETY: entry was borrowed from a live extended ACL. The returned
    // qualifier is a UUID buffer that the caller frees using acl_free.
    let uuid =
        NonNull::new(unsafe { acl_get_qualifier(entry) }).ok_or_else(io::Error::last_os_error)?;
    let bytes = unsafe { uuid.as_ptr().cast::<[u8; 16]>().read() };
    unsafe { acl_free(uuid.as_ptr()) };
    Ok(bytes)
}

fn entries(
    file: &File,
    mut visit: impl FnMut(*mut c_void, c_int, u64) -> io::Result<()>,
) -> io::Result<()> {
    // SAFETY: The borrowed File keeps the descriptor live. A nonnull result
    // owns an ACL that the operating system requires us to free.
    let Some(acl) =
        NonNull::new(unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) }).map(Acl)
    else {
        return match Errno::last() {
            Errno::ENOENT => Ok(()),
            error => Err(error.into()),
        };
    };

    let mut index = ACL_FIRST_ENTRY;
    loop {
        let mut entry = null_mut();
        // SAFETY: The ACL is live, and entry is a writable result slot. An
        // entry returned on success is borrowed for the life of the ACL.
        let status = unsafe { acl_get_entry(acl.0.as_ptr(), index, &mut entry) };
        if status != 0 {
            return match Errno::last() {
                Errno::EINVAL => Ok(()),
                error => Err(error.into()),
            };
        }
        let mut tag = 0;
        let mut rights = 0;
        // SAFETY: entry comes from the successful call above. Both output
        // arguments have the types and storage sizes declared by Darwin.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0
            || unsafe { acl_get_permset_mask_np(entry, &mut rights) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        visit(entry, tag, rights)?;
        index = ACL_NEXT_ENTRY;
    }
}
