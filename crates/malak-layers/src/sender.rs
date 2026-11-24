use std::sync::{Arc, OnceLock};

/// The OS identity of a process during its lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    /// Local process ID.
    pub pid: u32,
    /// OS start time, in clock ticks since boot on Linux or epoch microseconds on macOS.
    pub start_time: u64,
}

/// A process selected as the source of a sending session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionProcess {
    /// PID and start time, used together to recognize the process.
    pub identity: ProcessIdentity,
    /// OS-reported program name.
    pub program: Box<str>,
}

/// Sender details observed by the board's transport, separate from authentication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderTrace {
    /// Transport kind, such as `unix`, `vsock` or `in_process`.
    pub transport: Box<str>,
    /// Direct peer PID, if available.
    pub pid: Option<u32>,
    /// Direct peer program name, if available.
    pub program: Option<Box<str>>,
    /// Direct peer working folder, if available.
    pub cwd: Option<Box<str>>,
    /// OS process selected as the session, if available.
    pub session: Option<SessionProcess>,
}

impl SenderTrace {
    /// Describe only the transport kind.
    pub fn transport(kind: &str) -> Self {
        Self {
            transport: kind.into(),
            pid: None,
            program: None,
            cwd: None,
            session: None,
        }
    }
}

/// Sender facts for a connection, including evidence of process reparenting.
pub struct SenderPeer {
    /// The provenance that accompanies a stored message.
    pub trace: SenderTrace,
    /// Process identities observed below the session process, including the peer.
    pub ancestry: Vec<ProcessIdentity>,
}

impl SenderPeer {
    /// Describe a connection for which only the transport kind is known.
    pub fn transport(kind: &str) -> Self {
        Self {
            trace: SenderTrace::transport(kind),
            ancestry: Vec::new(),
        }
    }
}

/// A transport's sender observation, resolved once when the connection attaches.
pub struct SenderOrigin {
    lookup: Option<Box<dyn Fn() -> SenderPeer + Send + Sync>>,
    observed: OnceLock<Arc<SenderPeer>>,
}

impl SenderOrigin {
    /// Record the transport kind without looking up any process details.
    pub fn transport(kind: &str) -> Self {
        Self {
            lookup: None,
            observed: OnceLock::from(Arc::new(SenderPeer::transport(kind))),
        }
    }

    /// Defer an OS lookup until the board authenticates the connection.
    pub fn deferred(lookup: impl Fn() -> SenderPeer + Send + Sync + 'static) -> Self {
        Self {
            lookup: Some(Box::new(lookup)),
            observed: OnceLock::new(),
        }
    }

    /// Read or collect this connection's sender facts.
    pub fn observe(&self) -> &Arc<SenderPeer> {
        self.observed
            .get_or_init(|| Arc::new(self.lookup.as_ref().expect("deferred sender lookup")()))
    }
}
