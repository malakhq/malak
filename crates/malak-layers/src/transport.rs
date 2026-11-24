use crate::{CreationId, Error, SenderOrigin};
use std::num::NonZeroUsize;

/// How a transport distinguishes peers.
#[derive(Clone, Copy, Debug)]
pub struct TransportInfo {
    /// Whether the transport can distinguish sources before proof.
    pub distinguishes_sources: bool,
    /// Whether the transport itself authenticates both ends.
    pub authenticates_ends: bool,
}

/// What the operating system or transport reports about a link's source.
///
/// This describes the link and does not itself authenticate a participant.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum Source {
    /// The calling user's numeric local account ID.
    LocalUser(u32),
    /// A VM launch as identified by its trusted host transport.
    VirtualMachine(CreationId),
    /// An identifier in the transport's own source namespace.
    Other(String),
}

/// A link and the facts known about its other end before proof.
pub struct Link<T> {
    /// The transport's handle to the link.
    pub io: T,
    /// The reported source, when one is available.
    pub source: Option<Source>,
    /// The reported local process ID, where the OS makes one available.
    pub process_id: Option<u32>,
    /// Sender facts from the accepting transport.
    pub sender: SenderOrigin,
}

/// Result of reading from a link into a buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Received {
    /// A nonempty prefix of the caller's buffer was filled.
    Bytes(NonZeroUsize),
    /// The link closed. Report this once for each link.
    Closed,
}

/// Carries ordered bytes and leaves authentication and framing to the core.
pub trait Transport: Send + Sync {
    /// The implementation's opaque link handle.
    type Connection: Send + Sync;

    /// State what the transport knows about the peer.
    fn describe(&self) -> TransportInfo;

    /// Accept the next link at a local address.
    fn accept(&self, address: &str) -> Result<Link<Self::Connection>, Error>;

    /// Open a link to the other end at an address.
    fn dial(&self, address: &str) -> Result<Link<Self::Connection>, Error>;

    /// Write an ordered prefix and return the number of bytes written.
    ///
    /// A nonempty input must make progress or return an error. An empty input closes the link.
    fn send(&self, link: &Self::Connection, bytes: &[u8]) -> Result<usize, Error>;

    /// Read the next bytes into a nonempty buffer, or report closure.
    fn receive(&self, link: &Self::Connection, output: &mut [u8]) -> Result<Received, Error>;
}
