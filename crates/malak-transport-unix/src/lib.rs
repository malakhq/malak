//! Unix-domain and Firecracker vsock links.

#![cfg(any(target_os = "linux", target_os = "macos"))]

mod sender;
mod stream;
pub mod vsock;

/// Resolve the calling process's session using the Unix sender-trace walk.
pub fn local_session_process() -> Option<malak_layers::SessionProcess> {
    sender::local_session_process()
}

/// Read a running process's identity without walking its ancestry.
pub fn process_identity(pid: u32) -> Option<malak_layers::ProcessIdentity> {
    sender::process_identity(pid)
}

/// Resolve a running process to the session that would own its child commands.
pub fn session_at(pid: u32) -> Option<malak_layers::ClaimedSession> {
    sender::session_at(pid, false)
}

/// Identify a live process before it replaces itself with a worker command.
pub fn starting_session_at(pid: u32) -> Option<malak_layers::ClaimedSession> {
    sender::session_at(pid, true)
}

use malak_layers::{Error, Link, Received, SenderOrigin, Source, Transport, TransportInfo};
use nix::sys::socket::sockopt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use stream::{Stream, accept_ready, io_error};

/// A byte transport with an optional Unix listener.
#[derive(Default)]
pub struct UnixTransport {
    listener: Option<(PathBuf, UnixListener)>,
}

/// A connected bidirectional Unix byte stream.
pub struct UnixLink(Stream);

impl UnixTransport {
    /// Bind a socket at a configured `unix:` address and start listening.
    ///
    /// The caller supplies the socket's directory and enforces the entrance's
    /// filesystem policy. Existing paths are left in place.
    pub fn bind(address: &str) -> Result<Self, Error> {
        let path = unix_path(address)?;
        let listener =
            UnixListener::bind(path).map_err(|error| io_error("bind Unix listener", &error))?;
        Ok(Self {
            listener: Some((path.to_path_buf(), listener)),
        })
    }
}

impl Transport for UnixTransport {
    type Connection = UnixLink;

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            distinguishes_sources: true,
            authenticates_ends: false,
        }
    }

    fn accept(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
        let path = unix_path(address)?;
        let (_, listener) = self
            .listener
            .as_ref()
            .filter(|(bound, _)| bound == path)
            .ok_or_else(|| Error::unavailable("Unix listener is not bound at that address"))?;
        accept_ready(
            "accept Unix link",
            || listener.accept().map(|(socket, _)| socket),
            |socket| {
                let peer = peer(&socket).require_uid()?;
                let pid = peer.pid.ok().flatten();
                let sender = SenderOrigin::deferred(move || sender::resolve(pid));
                connected(socket, peer, sender)
            },
        )
    }

    fn dial(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
        let stream = UnixStream::connect(unix_path(address)?)
            .map_err(|error| io_error("connect Unix link", &error))?;
        let peer = peer(&stream);
        connected(stream, peer, SenderOrigin::transport("unix"))
    }

    fn send(&self, link: &Self::Connection, bytes: &[u8]) -> Result<usize, Error> {
        link.0.send(bytes)
    }

    fn receive(&self, link: &Self::Connection, output: &mut [u8]) -> Result<Received, Error> {
        link.0.receive(output)
    }
}

fn connected(
    socket: UnixStream,
    peer: Peer,
    sender: SenderOrigin,
) -> Result<Link<UnixLink>, Error> {
    Ok(Link {
        io: UnixLink(Stream::unix(socket)?),
        source: peer.uid.ok().map(Source::LocalUser),
        process_id: peer.pid.ok().flatten(),
        sender,
    })
}

struct Peer {
    uid: nix::Result<u32>,
    pid: nix::Result<Option<u32>>,
}

impl Peer {
    fn require_uid(self) -> Result<Self, Error> {
        self.uid
            .map_err(|error| io_error("read Unix peer credentials", &error.into()))?;
        Ok(self)
    }
}

#[cfg(target_os = "linux")]
fn peer(socket: &UnixStream) -> Peer {
    use nix::sys::socket::getsockopt;

    match getsockopt(socket, sockopt::PeerCredentials) {
        Ok(credentials) => Peer {
            uid: Ok(credentials.uid()),
            pid: Ok(credentials.pid().try_into().ok()),
        },
        Err(error) => Peer {
            uid: Err(error),
            pid: Err(error),
        },
    }
}

#[cfg(target_os = "macos")]
fn peer(socket: &UnixStream) -> Peer {
    use nix::sys::socket::getsockopt;

    Peer {
        uid: nix::unistd::getpeereid(socket).map(|(uid, _)| uid.as_raw()),
        pid: getsockopt(socket, sockopt::LocalPeerPid).map(|pid| pid.try_into().ok()),
    }
}

fn unix_path(address: &str) -> Result<&Path, Error> {
    address
        .strip_prefix("unix:")
        .filter(|path| !path.is_empty())
        .map(Path::new)
        .ok_or_else(|| Error::refused("invalid Unix address"))
}
