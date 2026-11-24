use malak_layers::{Error, Received};
use nix::errno::Errno;
use nix::sys::socket::{MsgFlags, Shutdown, recv, send, setsockopt, shutdown, sockopt};
use std::io;
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};

const SOCKET_BUFFER: usize = 64 * 1024;

pub(crate) struct Stream {
    socket: OwnedFd,
    closed_reported: AtomicBool,
}

impl Stream {
    pub(crate) fn new(socket: impl Into<OwnedFd>) -> Self {
        Self {
            socket: socket.into(),
            closed_reported: AtomicBool::new(false),
        }
    }

    pub(crate) fn unix(socket: UnixStream) -> Result<Self, Error> {
        setsockopt(&socket, sockopt::SndBuf, &SOCKET_BUFFER)
            .and_then(|()| setsockopt(&socket, sockopt::RcvBuf, &SOCKET_BUFFER))
            .map_err(|error| io_error("set socket buffers", &error.into()))?;
        Ok(Self::new(socket))
    }

    pub(crate) fn send(&self, bytes: &[u8]) -> Result<usize, Error> {
        let fd = self.socket.as_raw_fd();
        if bytes.is_empty() {
            return match shutdown(fd, Shutdown::Both) {
                Ok(()) | Err(Errno::ENOTCONN) => Ok(0),
                Err(error) => Err(io_error("close link", &error.into())),
            };
        }
        match retry(|| {
            send(
                fd,
                bytes,
                MsgFlags::from_bits_retain(nix::libc::MSG_NOSIGNAL),
            )
            .map_err(Into::into)
        }) {
            Ok(0) => Err(Error::unavailable("link closed before writing bytes")),
            Ok(count) => Ok(count),
            Err(error) => Err(io_error("write link", &error)),
        }
    }

    pub(crate) fn receive(&self, output: &mut [u8]) -> Result<Received, Error> {
        if output.is_empty() {
            return Err(Error::refused("receive requires a nonempty buffer"));
        }
        if self.closed_reported.load(Ordering::Acquire) {
            return Err(Error::unavailable("link is closed"));
        }

        match self.read(output) {
            Ok(count) => NonZeroUsize::new(count)
                .map_or_else(|| self.closed(), |count| Ok(Received::Bytes(count))),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
                ) =>
            {
                self.closed()
            }
            Err(error) => Err(io_error("read link", &error)),
        }
    }

    #[cfg(target_os = "macos")]
    fn read(&self, output: &mut [u8]) -> io::Result<usize> {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::os::fd::AsFd;

        // Darwin can leave a blocking recv asleep after the peer shuts down.
        // Wait for readiness separately, then read without entering that wait.
        let fd = self.socket.as_raw_fd();
        let mut socket = [PollFd::new(self.socket.as_fd(), PollFlags::POLLIN)];
        loop {
            match retry(|| recv(fd, output, MsgFlags::MSG_DONTWAIT).map_err(Into::into)) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    retry(|| poll(&mut socket, PollTimeout::NONE).map_err(Into::into))?;
                }
                result => return result,
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn read(&self, output: &mut [u8]) -> io::Result<usize> {
        let fd = self.socket.as_raw_fd();
        retry(|| recv(fd, output, MsgFlags::empty()).map_err(Into::into))
    }

    fn closed(&self) -> Result<Received, Error> {
        if self.closed_reported.swap(true, Ordering::AcqRel) {
            Err(Error::unavailable("link is closed"))
        } else {
            Ok(Received::Closed)
        }
    }
}

pub(crate) fn retry<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    loop {
        match operation() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

pub(crate) fn io_error(operation: &str, error: &io::Error) -> Error {
    Error::unavailable(format!("{operation}: {error}"))
}

pub(crate) fn accept_ready<S, L>(
    operation: &str,
    mut accept: impl FnMut() -> io::Result<S>,
    mut prepare: impl FnMut(S) -> Result<L, Error>,
) -> Result<L, Error> {
    loop {
        let socket = match retry(&mut accept) {
            Ok(socket) => socket,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
                ) =>
            {
                continue;
            }
            Err(error) => return Err(io_error(operation, &error)),
        };
        // Preparation owns the accepted socket and drops it on failure.
        if let Ok(link) = prepare(socket) {
            return Ok(link);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_setup_failure_discards_only_the_accepted_socket() {
        let (first, _first_peer) = UnixStream::pair().unwrap();
        let (second, _second_peer) = UnixStream::pair().unwrap();
        let next = second.as_raw_fd();
        let mut sockets = [first, second].into_iter();

        let accepted = accept_ready(
            "accept link",
            || Ok(sockets.next().expect("a socket is ready")),
            |socket| {
                if socket.as_raw_fd() == next {
                    Stream::unix(socket)
                } else {
                    Err(io_error("set socket buffers", &Errno::EINVAL.into()))
                }
            },
        )
        .unwrap();

        assert_eq!(accepted.socket.as_raw_fd(), next);
    }

    #[test]
    fn invalid_argument_from_the_listener_is_reported() {
        let error = accept_ready(
            "accept link",
            || Err::<(), _>(Errno::EINVAL.into()),
            |()| -> Result<(), Error> { unreachable!("the listener accepted no socket") },
        )
        .unwrap_err();

        assert_eq!(error, io_error("accept link", &Errno::EINVAL.into()));
    }

    #[test]
    fn a_connection_aborted_during_accept_is_skipped() {
        let mut sockets = [Err(Errno::ECONNABORTED.into()), Ok(())].into_iter();

        assert!(accept_ready("accept link", || sockets.next().unwrap(), Ok).is_ok());
    }
}
