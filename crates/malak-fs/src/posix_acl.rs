use super::{AccountAccess, denied, posix_search};
use nix::unistd::{Gid, Uid, User, getgrouplist};
use rustix::fs::{XattrFlags, fgetxattr, fsetxattr};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;

const ATTRIBUTE: &str = "system.posix_acl_access";
const VERSION: [u8; 4] = 2_u32.to_le_bytes();
const OWNER: u16 = 1;
const NAMED_USER: u16 = 2;
const GROUP: u16 = 4;
const NAMED_GROUP: u16 = 8;
const MASK: u16 = 16;
const OTHER: u16 = 32;
const UNDEFINED: u32 = u32::MAX;
const READ: u16 = 4;
const SEARCH: u16 = 1;

#[derive(PartialEq)]
struct Entry(u16, u16, u32);

fn canonical(owner: u16, user: u32, access: u16) -> [Entry; 5] {
    [
        Entry(OWNER, owner, UNDEFINED),
        Entry(NAMED_USER, access, user),
        Entry(GROUP, 0, UNDEFINED),
        Entry(MASK, access, UNDEFINED),
        Entry(OTHER, 0, UNDEFINED),
    ]
}

pub(super) fn share_with(file: &File, user: u32, access: AccountAccess) -> io::Result<()> {
    let owner = ((file.metadata()?.mode() >> 6) & 7) as u16;
    let access = match access {
        AccountAccess::Read => READ,
        AccountAccess::Search => SEARCH,
    };
    let mut encoded = Vec::with_capacity(44);
    encoded.extend(VERSION);
    for Entry(tag, permission, id) in canonical(owner, user, access) {
        encoded.extend(tag.to_le_bytes());
        encoded.extend(permission.to_le_bytes());
        encoded.extend(id.to_le_bytes());
    }
    Ok(fsetxattr(file, ATTRIBUTE, &encoded, XattrFlags::empty())?)
}

fn read(file: &File) -> io::Result<Option<Vec<Entry>>> {
    let mut bytes = vec![0; 64 * 1024];
    let size = match fgetxattr(file, ATTRIBUTE, bytes.as_mut_slice()) {
        Ok(size) => size,
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let payload = bytes[..size]
        .strip_prefix(&VERSION)
        .ok_or(io::ErrorKind::InvalidData)?;
    let (entries, tail) = payload.as_chunks::<8>();
    if !tail.is_empty() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(Some(
        entries
            .iter()
            .map(|entry| {
                Entry(
                    u16::from_le_bytes([entry[0], entry[1]]),
                    u16::from_le_bytes([entry[2], entry[3]]),
                    u32::from_le_bytes(entry[4..].try_into().unwrap()),
                )
            })
            .collect(),
    ))
}

pub(super) fn sole_reader(file: &File) -> io::Result<Option<u32>> {
    let Some(entries) = read(file)? else {
        return Ok(None);
    };
    match entries.as_slice() {
        [Entry(OWNER, owner, _), Entry(NAMED_USER, READ, reader), ..]
            if entries == canonical(*owner, *reader, READ) && owner & READ != 0 =>
        {
            Ok(Some(*reader))
        }
        _ => Err(denied()),
    }
}

pub(super) fn can_traverse(file: &File, account: u32) -> io::Result<bool> {
    let metadata = file.metadata()?;
    if account == 0 || metadata.uid() == account {
        return posix_search(file, account);
    }
    let Some(entries) = read(file)? else {
        return posix_search(file, account);
    };
    let mask = entries
        .iter()
        .find(|entry| entry.0 == MASK)
        .map_or(7, |entry| entry.1);
    if let Some(Entry(_, rights, _)) = entries
        .iter()
        .find(|entry| entry.0 == NAMED_USER && entry.2 == account)
    {
        return Ok(rights & mask & SEARCH != 0);
    }
    let groups = groups(account)?;
    let mut union = None;
    for Entry(tag, rights, id) in &entries {
        let group = match *tag {
            GROUP => metadata.gid(),
            NAMED_GROUP => *id,
            _ => continue,
        };
        if groups.contains(&Gid::from_raw(group)) {
            union = Some(union.unwrap_or(0) | rights);
        }
    }
    Ok(match union {
        Some(rights) => rights & mask & SEARCH != 0,
        None => entries
            .iter()
            .any(|entry| entry.0 == OTHER && entry.1 & SEARCH != 0),
    })
}

pub(super) fn in_group(account: u32, group: u32) -> io::Result<bool> {
    Ok(groups(account)?.contains(&Gid::from_raw(group)))
}

fn groups(account: u32) -> io::Result<Vec<Gid>> {
    let user = User::from_uid(Uid::from_raw(account))?.ok_or(io::ErrorKind::NotFound)?;
    let name = CString::new(user.name).map_err(|_| io::ErrorKind::InvalidData)?;
    Ok(getgrouplist(&name, user.gid)?)
}
