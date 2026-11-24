//! Combine descriptor-bound volume identity with the host's continuity mark.

use data_encoding::HEXLOWER;
use malak_fs::trusted_parent;
use malak_layers::{StoreLocation, StorePlace};
use nix::errno::Errno;
use nix::fcntl::AtFlags;
use nix::poll::{PollFd, PollFlags, poll};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
use nix::sys::stat::{SFlag, fstatat};
use nix::unistd::geteuid;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

#[allow(unsafe_code)]
mod volume;

const PROTOCOL: &str = "malak-place-v1";
const MARK_BYTES: usize = 32;
const TIMEOUT: Duration = Duration::from_secs(1);

pub(super) fn location(directory: &File) -> io::Result<StoreLocation> {
    let filesystem_id = volume::identity(directory)?;
    let folder_number = directory.metadata()?.ino();
    Ok(StoreLocation {
        filesystem_id,
        folder_number,
    })
}

pub(super) fn establish(directory: &File, path: &Path) -> io::Result<StorePlace> {
    let volume = volume::uuid(directory)?;
    let metadata = directory.metadata()?;
    let device = metadata.dev();
    let inode = metadata.ino();

    let mut stream = connect_host(path)?;
    writeln!(
        stream,
        "{PROTOCOL} {} {device} {inode}",
        HEXLOWER.encode(&volume),
    )?;
    Ok(match read_mark(&stream)? {
        Some(mark) => {
            let mut place = Vec::with_capacity(2 + volume.len() + 8 + MARK_BYTES);
            place.extend([1, volume.len() as u8]);
            place.extend(volume);
            place.extend(inode.to_be_bytes());
            place.extend(mark);
            StorePlace::Established(place)
        }
        None => StorePlace::NotEstablished,
    })
}

fn connect_host(path: &Path) -> io::Result<UnixStream> {
    let owner = geteuid().as_raw();
    let (parent, leaf) = trusted_parent(path, owner)?;
    let entry = fstatat(&parent, leaf, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if SFlag::from_bits_truncate(entry.st_mode) & SFlag::S_IFMT != SFlag::S_IFSOCK
        || !matches_account(entry.st_uid, owner)
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(target_os = "linux")]
    let flags = SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK;
    #[cfg(target_os = "macos")]
    let flags = SockFlag::empty();
    let fd = socket(AddressFamily::Unix, SockType::Stream, flags, None)?;
    #[cfg(target_os = "macos")]
    {
        use nix::fcntl::{FcntlArg, FdFlag, fcntl};
        fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    // A saturated host socket must not keep the store's claim occupied.
    connect(stream.as_raw_fd(), &UnixAddr::new(path)?)?;
    if !matches_account(peer_uid(&stream)?, owner) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    Ok(stream)
}

fn read_mark(mut stream: &UnixStream) -> io::Result<Option<[u8; MARK_BYTES]>> {
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + TIMEOUT;
    let mut reply = [0; PROTOCOL.len() + 1 + MARK_BYTES * 2 + 1];
    let mut used = 0;
    let mut socket = [PollFd::new(stream.as_fd(), PollFlags::POLLIN)];
    while used < reply.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // Keep one deadline even if a host or a signal wakes us repeatedly.
        // Darwin can reject fractional socket receive timeouts.
        let millis = u16::try_from(left.as_millis().max(1)).expect("one-second deadline");
        match poll(&mut socket, millis) {
            Ok(0) | Err(Errno::EINTR) => continue,
            Ok(_) if Instant::now() < deadline => {}
            Ok(_) => return Err(io::ErrorKind::TimedOut.into()),
            Err(error) => return Err(error.into()),
        }
        let received = match stream.read(&mut reply[used..]) {
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        used += received;
        if received == 0 || reply[..used].contains(&b'\n') {
            break;
        }
    }
    Ok(std::str::from_utf8(&reply[..used]).ok().and_then(mark))
}

fn matches_account(uid: u32, owner: u32) -> bool {
    uid == 0 || uid == owner
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &impl AsFd) -> io::Result<u32> {
    use nix::sys::socket::{getsockopt, sockopt};
    Ok(getsockopt(stream, sockopt::PeerCredentials)?.uid())
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &impl AsFd) -> io::Result<u32> {
    Ok(nix::unistd::getpeereid(stream)?.0.as_raw())
}

fn mark(reply: &str) -> Option<[u8; MARK_BYTES]> {
    let encoded = reply
        .strip_prefix(PROTOCOL)?
        .strip_prefix(' ')?
        .strip_suffix('\n')?;
    let mark: [u8; MARK_BYTES] = HEXLOWER.decode(encoded.as_bytes()).ok()?.try_into().ok()?;
    (mark != [0; MARK_BYTES]).then_some(mark)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_complete_nonzero_host_mark_establishes_place() {
        let valid = format!("{PROTOCOL} {:064x}", 1);
        assert_eq!(mark(&format!("{valid}\n")).unwrap()[MARK_BYTES - 1], 1);
        assert_eq!(mark("not-established\n"), None);
        assert_eq!(mark(&valid), None);
        assert_eq!(mark(&format!("{PROTOCOL} {:064x}\n", 0)), None);
    }

    #[test]
    fn a_host_mark_can_arrive_in_separate_socket_reads() {
        let (client, mut host) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            for part in format!("{PROTOCOL} {:064x}\n", 7).as_bytes().chunks(8) {
                host.write_all(part).unwrap();
                std::thread::sleep(Duration::from_millis(3));
            }
        });

        let mark = read_mark(&client).unwrap().unwrap();

        server.join().unwrap();
        assert_eq!(mark[MARK_BYTES - 1], 7);
    }

    #[test]
    fn an_incomplete_host_mark_cannot_extend_the_deadline() {
        let (client, mut host) = UnixStream::pair().unwrap();
        host.write_all(b"malak-place-v1 ").unwrap();

        let error = read_mark(&client).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
