use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

pub(super) fn identity(directory: &File) -> io::Result<Vec<u8>> {
    if let Ok(uuid) = uuid(directory) {
        let mut id = Vec::with_capacity(uuid.len() + 1);
        id.push(1);
        id.extend(uuid);
        return Ok(id);
    }
    let id = nix::sys::statfs::fstatfs(directory)?.filesystem_id();
    // SAFETY: On Linux and macOS libc represents fsid_t as two C int values.
    // Every bit pattern is valid for the resulting array.
    let words = unsafe { std::mem::transmute::<nix::libc::fsid_t, [i32; 2]>(id) };
    if words == [0, 0] {
        return Err(io::ErrorKind::Unsupported.into());
    }
    let mut id = Vec::with_capacity(9);
    id.push(2);
    id.extend(words.into_iter().flat_map(i32::to_be_bytes));
    Ok(id)
}

#[cfg(target_os = "macos")]
pub(super) fn uuid(directory: &File) -> io::Result<Vec<u8>> {
    use nix::libc;

    let mut attrs = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // A four-byte native-endian length precedes the requested 16-byte UUID.
    let mut output = [0_u32; 5];
    // SAFETY: The descriptor belongs to the open store directory, attrs is
    // initialized, and the output buffer is valid for the supplied size.
    if unsafe {
        libc::fgetattrlist(
            directory.as_raw_fd(),
            (&raw mut attrs).cast(),
            output.as_mut_ptr().cast(),
            size_of_val(&output),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if output[0] != size_of_val(&output) as u32 || output[1..] == [0; 4] {
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(output[1..]
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .collect())
}

#[cfg(target_os = "linux")]
pub(super) fn uuid(directory: &File) -> io::Result<Vec<u8>> {
    #[repr(C)]
    struct FsUuid {
        length: u8,
        uuid: [u8; 16],
    }
    // FS_IOC_GETFSUUID uses the public fsuuid2 layout in linux/fs.h.
    nix::ioctl_read!(read_uuid, 0x15, 0, FsUuid);
    let mut output = FsUuid {
        length: 0,
        uuid: [0; 16],
    };
    // SAFETY: The descriptor is live and output matches the kernel's result type.
    unsafe { read_uuid(directory.as_raw_fd(), &mut output) }?;
    let uuid = output
        .uuid
        .get(..usize::from(output.length))
        .ok_or(io::ErrorKind::InvalidData)?;
    if uuid.iter().all(|byte| *byte == 0) {
        return Err(io::ErrorKind::Unsupported.into());
    }
    Ok(uuid.to_vec())
}
