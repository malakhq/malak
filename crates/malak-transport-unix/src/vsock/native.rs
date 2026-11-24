use super::{Stream, accept_ready, io_error};
use malak_layers::Error;
use nix::libc::VMADDR_CID_ANY;
use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, VsockAddr, accept4, bind, connect, listen, socket,
};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub(super) struct Listener {
    port: u32,
    socket: OwnedFd,
}

impl Listener {
    pub(super) fn bind(port: u32) -> Result<Self, Error> {
        let socket = stream_socket()?;
        bind(socket.as_raw_fd(), &VsockAddr::new(VMADDR_CID_ANY, port))
            .and_then(|()| listen(&socket, Backlog::new(128)?))
            .map_err(|error| io_error("bind guest vsock listener", &error.into()))?;
        Ok(Self { port, socket })
    }

    pub(super) fn accept(&self, port: u32) -> Result<Stream, Error> {
        if self.port != port {
            return Err(Error::refused("address does not name the guest listener"));
        }
        accept_ready(
            "accept guest vsock",
            || accept4(self.socket.as_raw_fd(), SockFlag::SOCK_CLOEXEC).map_err(Into::into),
            |fd| {
                // SAFETY: accept4 returned a new descriptor owned by this caller.
                Ok(Stream::new(unsafe { OwnedFd::from_raw_fd(fd) }))
            },
        )
    }
}

pub(super) fn dial(cid: u32, port: u32) -> Result<Stream, Error> {
    let socket = stream_socket()?;
    connect(socket.as_raw_fd(), &VsockAddr::new(cid, port))
        .map_err(|error| io_error("connect guest vsock", &error.into()))?;
    Ok(Stream::new(socket))
}

fn stream_socket() -> Result<OwnedFd, Error> {
    socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|error| io_error("open guest vsock", &error.into()))
}
