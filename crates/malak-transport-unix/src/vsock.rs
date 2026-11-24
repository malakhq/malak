//! Firecracker stream transport for host Unix routes and Linux guests.

mod preamble;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod native;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod vmm;

#[cfg(target_os = "linux")]
pub use vmm::Vmm;

use crate::stream::{Stream, accept_ready, io_error};
use malak_layers::{Error, Link, Received, Transport, TransportInfo};
use std::ffi::OsString;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// A Firecracker host route or native guest vsock transport.
///
/// Transport addresses are `vsock:HOST:PORT`. On a guest, use CID 2
/// for the host. On the host, the VM name selects a configured Unix route.
#[derive(Default)]
pub struct VsockTransport {
    host: Option<Host>,
    #[cfg(target_os = "linux")]
    guest: Option<native::Listener>,
}

struct Host {
    name: String,
    port: u32,
    uds: PathBuf,
    listener: UnixListener,
    #[cfg(target_os = "linux")]
    vmm: Option<std::sync::Arc<Vmm>>,
}

/// A connected Firecracker byte stream.
pub struct VsockLink(Stream);

impl VsockTransport {
    /// Listen for one VM's guest-to-host connections at `uds_path_PORT`.
    ///
    /// Supply the host-path equivalent of the VM's configured `uds_path`
    /// and an unused socket pathname in an operator-controlled directory.
    /// The caller sets filesystem access and removes the listener pathname.
    pub fn bind_firecracker(guest: &str, uds_path: &Path, port: u32) -> Result<Self, Error> {
        if !valid_guest(guest) || !uds_path.is_absolute() || !valid_port(port) {
            return Err(Error::refused("invalid Firecracker route"));
        }
        let mut path = OsString::from(uds_path);
        path.push(format!("_{port}"));
        let listener =
            UnixListener::bind(path).map_err(|error| io_error("bind Firecracker route", &error))?;
        Ok(Self {
            host: Some(Host {
                name: guest.into(),
                port,
                uds: uds_path.into(),
                listener,
                #[cfg(target_os = "linux")]
                vmm: None,
            }),
            #[cfg(target_os = "linux")]
            guest: None,
        })
    }

    /// Bind a host route and require the pinned VMM as its peer.
    ///
    /// Use the VM's ID as the address's VM component on the host. The
    /// guest connects to `vsock:2:PORT`.
    #[cfg(target_os = "linux")]
    pub fn bind_verified(
        vmm: std::sync::Arc<Vmm>,
        uds_path: &Path,
        port: u32,
    ) -> Result<Self, Error> {
        let mut transport = Self::bind_firecracker(&vmm.id().to_string(), uds_path, port)?;
        transport.host.as_mut().expect("host route").vmm = Some(vmm);
        Ok(transport)
    }

    /// Open a native listener in a Linux guest at the supplied port.
    #[cfg(target_os = "linux")]
    pub fn bind_guest(port: u32) -> Result<Self, Error> {
        if !valid_port(port) {
            return Err(Error::refused("invalid vsock port"));
        }
        Ok(Self {
            guest: Some(native::Listener::bind(port)?),
            ..Self::default()
        })
    }
}

impl Transport for VsockTransport {
    type Connection = VsockLink;

    fn describe(&self) -> TransportInfo {
        TransportInfo {
            #[cfg(target_os = "linux")]
            distinguishes_sources: self.host.as_ref().is_some_and(|host| host.vmm.is_some()),
            #[cfg(not(target_os = "linux"))]
            distinguishes_sources: false,
            authenticates_ends: false,
        }
    }

    fn accept(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
        let (guest, port) = parse(address)?;
        if let Some(host) = &self.host {
            if host.name != guest || host.port != port {
                return Err(Error::refused(
                    "address does not name this Firecracker listener",
                ));
            }
            return accept_ready(
                "accept Firecracker link",
                || host.listener.accept().map(|(socket, _)| socket),
                |socket| {
                    let peer = crate::peer(&socket).require_uid()?;
                    peer.pid
                        .map_err(|error| io_error("read Unix peer credentials", &error.into()))?;
                    let evidence = host.evidence(peer)?;
                    host_link(socket, evidence)
                },
            );
        }
        #[cfg(target_os = "linux")]
        if let Some(listener) = &self.guest {
            return listener.accept(port).map(link);
        }
        Err(Error::not_supported("vsock listener is not configured"))
    }

    fn dial(&self, address: &str) -> Result<Link<Self::Connection>, Error> {
        let (guest, port) = parse(address)?;
        if let Some(host) = &self.host {
            if host.name != guest {
                return Err(Error::refused(
                    "address does not name this Firecracker route",
                ));
            }
            let socket = UnixStream::connect(&host.uds)
                .map_err(|error| io_error("connect to Firecracker VMM", &error))?;
            let evidence = host.evidence(crate::peer(&socket))?;
            preamble::connect(&socket, port)?;
            return host_link(socket, evidence);
        }
        #[cfg(target_os = "linux")]
        {
            let cid = guest
                .parse()
                .map_err(|_| Error::refused("vsock requires a numeric guest CID"))?;
            native::dial(cid, port).map(link)
        }
        #[cfg(not(target_os = "linux"))]
        Err(Error::not_supported("native guest vsock requires Linux"))
    }

    fn send(&self, link: &Self::Connection, bytes: &[u8]) -> Result<usize, Error> {
        link.0.send(bytes)
    }

    fn receive(&self, link: &Self::Connection, output: &mut [u8]) -> Result<Received, Error> {
        link.0.receive(output)
    }
}

impl Host {
    fn evidence(&self, peer: crate::Peer) -> Result<Evidence, Error> {
        let pid = peer.pid.ok().flatten();
        #[cfg(target_os = "linux")]
        if let Some(vmm) = &self.vmm {
            if vmm.matches(peer.uid.ok(), pid)? {
                return Ok(Evidence {
                    pid,
                    source: Some(malak_layers::Source::VirtualMachine(vmm.id())),
                });
            }
            return Err(Error::refused(
                "Firecracker peer does not match the live VMM",
            ));
        }
        Ok(Evidence { pid, source: None })
    }
}

#[derive(Default)]
struct Evidence {
    pid: Option<u32>,
    source: Option<malak_layers::Source>,
}

fn host_link(socket: UnixStream, evidence: Evidence) -> Result<Link<VsockLink>, Error> {
    let mut link = link(Stream::unix(socket)?);
    link.process_id = evidence.pid;
    link.source = evidence.source;
    Ok(link)
}

fn link(stream: Stream) -> Link<VsockLink> {
    Link {
        io: VsockLink(stream),
        source: None,
        process_id: None,
        sender: malak_layers::SenderOrigin::transport("vsock"),
    }
}

fn parse(address: &str) -> Result<(&str, u32), Error> {
    let invalid = || Error::refused("invalid vsock address");
    let (guest, port) = address
        .strip_prefix("vsock:")
        .and_then(|address| address.split_once(':'))
        .ok_or_else(invalid)?;
    let port = port.parse().map_err(|_| invalid())?;
    if !valid_guest(guest) || !valid_port(port) {
        return Err(invalid());
    }
    Ok((guest, port))
}

fn valid_guest(guest: &str) -> bool {
    !guest.is_empty()
        && guest.len() <= 128
        && guest
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_port(port: u32) -> bool {
    port != 0 && port != u32::MAX
}

#[cfg(test)]
mod tests;
