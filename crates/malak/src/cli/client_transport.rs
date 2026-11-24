use super::board_version::BoardVersion;
use malak_core::LocalClient;
use malak_layers::{Error, Link, Outcome, Received, Source, Transport, TransportInfo};
use malak_transport_unix::{
    UnixLink, UnixTransport,
    vsock::{VsockLink, VsockTransport},
};
use nix::unistd::geteuid;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub(super) const LINK_DROPPED: &str =
    "the link dropped and could not reconnect before the wait ended";
pub(super) const LINK_HINT: &str =
    "Check whether the board stopped or is restarting, then retry when it is running.";

pub(super) fn link_failed(error: &Error) -> bool {
    if error.outcome() != Outcome::Unavailable {
        return false;
    }
    let reason = error.to_string();
    matches!(
        reason.as_str(),
        "board has stopped"
            | "link is closed"
            | "link closed while reading a segment"
            | "link stopped while writing a segment"
            | "link closed before writing bytes"
            | "the link deadline passed"
            | LINK_DROPPED
    ) || [
        "no board is answering at ",
        "read link: ",
        "write link: ",
        "connect guest vsock: ",
        "connect to Firecracker VMM: ",
    ]
    .iter()
    .any(|prefix| reason.starts_with(prefix))
}

// Keep this distinction at link I/O boundaries. A board can use the same
// worker timeout wording for a store call in an authenticated ERR reply.
pub(super) fn link_io_error(error: Error) -> Error {
    if error == Error::unavailable("worker did not answer in time") {
        Error::unavailable("the link deadline passed")
    } else {
        error
    }
}

pub(super) fn reconnect<T>(
    deadline: Instant,
    mut attempt: impl FnMut(Duration) -> Result<T, Error>,
) -> Result<T, Error> {
    let mut delay = Duration::ZERO;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::unavailable(LINK_DROPPED));
        }
        match attempt(remaining) {
            Err(error) if link_failed(&error) => {}
            result => return result,
        }
        delay = (delay * 2).clamp(Duration::from_millis(50), Duration::from_secs(2));
        std::thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// Select the participant's transport from its private handover address.
#[derive(Default)]
pub(super) struct ClientTransport {
    unix: UnixTransport,
    vsock: VsockTransport,
    local_peer: Mutex<Option<(String, u32)>>,
    unix_link: Mutex<Weak<UnixLink>>,
}

pub(super) enum ClientLink {
    Unix(Arc<UnixLink>),
    Vsock(VsockLink),
}

impl ClientTransport {
    pub(super) fn connect(
        open: impl FnOnce(Arc<Self>) -> Result<LocalClient<Self>, Error>,
    ) -> Result<(LocalClient<Self>, BoardVersion), Error> {
        let (client, version, _) = Self::connect_quiet(open)?;
        version.warn();
        Ok((client, version))
    }

    pub(super) fn connect_quiet(
        open: impl FnOnce(Arc<Self>) -> Result<LocalClient<Self>, Error>,
    ) -> Result<(LocalClient<Self>, BoardVersion, Arc<Self>), Error> {
        let transport = Arc::new(Self::default());
        let client = open(transport.clone())?;
        let peer = transport.local_peer.lock().unwrap().take();
        let version = peer.map_or(BoardVersion::Unavailable, |(address, pid)| {
            BoardVersion::read(&address, geteuid().as_raw(), pid)
        });
        Ok((client, version, transport))
    }

    pub(super) fn close_unix_link(&self) {
        if let Some(link) = self.unix_link.lock().unwrap().upgrade() {
            let _ = self.unix.send(&link, &[]);
        }
    }
}

impl Transport for ClientTransport {
    type Connection = ClientLink;

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            distinguishes_sources: false,
            authenticates_ends: false,
        }
    }

    fn accept(&self, _address: &str) -> Result<Link<ClientLink>, Error> {
        Err(Error::not_supported(
            "the participant client does not listen",
        ))
    }

    fn dial(&self, address: &str) -> Result<Link<ClientLink>, Error> {
        if address.starts_with("vsock:") {
            *self.local_peer.lock().unwrap() = None;
            self.vsock
                .dial(address)
                .map(|link| map_link(link, ClientLink::Vsock))
        } else {
            let link = self
                .unix
                .dial(address)
                .map_err(|_| not_answering(address))?;
            let uid = geteuid().as_raw();
            *self.local_peer.lock().unwrap() = link
                .process_id
                .filter(|_| link.source == Some(Source::LocalUser(uid)))
                .map(|pid| (address.to_owned(), pid));
            Ok(map_link(link, |link| {
                let link = Arc::new(link);
                *self.unix_link.lock().unwrap() = Arc::downgrade(&link);
                ClientLink::Unix(link)
            }))
        }
    }

    fn send(&self, link: &ClientLink, bytes: &[u8]) -> Result<usize, Error> {
        match link {
            ClientLink::Unix(link) => self.unix.send(link, bytes),
            ClientLink::Vsock(link) => self.vsock.send(link, bytes),
        }
    }

    fn receive(&self, link: &ClientLink, output: &mut [u8]) -> Result<Received, Error> {
        match link {
            ClientLink::Unix(link) => self.unix.receive(link, output),
            ClientLink::Vsock(link) => self.vsock.receive(link, output),
        }
    }
}

fn not_answering(address: &str) -> Error {
    let place = address.strip_prefix("unix:").unwrap_or(address);
    Error::unavailable(format!(
        "no board is answering at {place}; {}",
        super::config::START_HINT
    ))
}

fn map_link<T>(link: Link<T>, convert: impl FnOnce(T) -> ClientLink) -> Link<ClientLink> {
    Link {
        io: convert(link.io),
        source: link.source,
        process_id: link.process_id,
        sender: link.sender,
    }
}
