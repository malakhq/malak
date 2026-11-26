//! Serve a board over a transport (the `unix` feature).
//!
//! A [`Server`] accepts links at the board's three entrances, gives each its
//! own thread, and drives it through the handshake, attachment and the four
//! verbs. A transport only moves bytes and a mechanism only proves identity;
//! the router is the one [`Owner`] controls.

use crate::{Error, Limits, Owner};
use malak_layers::{Mechanism, Store, Transport};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use malak_core::LocalEntrances;
pub use malak_core::LocalStartup;
pub use malak_proto::Entrance;

/// How often [`Server::run`] looks at its stop flag and board.
const POLL: Duration = Duration::from_millis(100);
/// How often [`Server::run`] calls its tick.
const TICK: Duration = Duration::from_secs(1);

/// A board's entrances, ready to listen on transports.
///
/// Create it with [`Server::create`], add one listener per entrance with
/// [`Server::listen`], call [`Server::ready`], then [`Server::run`].
pub struct Server {
    entrances: LocalEntrances,
    ready: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    faults: mpsc::Sender<Error>,
    fault: mpsc::Receiver<Error>,
}

/// Ends a [`Server::run`] from another thread.
#[derive(Clone)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    /// Make [`Server::run`] return `Ok`. Links already served keep running.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
}

impl Server {
    /// Start or reopen a durable local board, and provision its entrances.
    ///
    /// The lead's entrance admits `lead_uid`, and the owner door admits the
    /// account running the board. `mechanism` proves participants at the
    /// general entrance. The caller prepares the listener addresses in
    /// `startup` before it calls [`Server::listen`].
    pub fn create(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
        startup: LocalStartup,
        mechanism: Arc<dyn Mechanism>,
        lead_uid: u32,
    ) -> Result<(Owner, Self), Error> {
        let (owner, mut entrances) =
            Owner::create_local_for_account(store, limits, state, startup, mechanism, lead_uid)?;
        entrances.set_session_lookup(
            malak_transport_unix::session_at,
            malak_transport_unix::starting_session_at,
        )?;
        entrances.set_process_identity(malak_transport_unix::process_identity)?;
        let (faults, fault) = mpsc::channel();
        let server = Self {
            entrances,
            ready: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            faults,
            fault,
        };
        Ok((owner, server))
    }

    /// The board's entrances, for work that belongs to the command line.
    #[doc(hidden)]
    pub fn entrances(&self) -> &LocalEntrances {
        &self.entrances
    }

    /// A handle that ends [`Server::run`].
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(self.stop.clone())
    }

    /// Accept links at one entrance on its own thread.
    ///
    /// At most `capacity` links are served at a time; a link beyond that is
    /// dropped. Links accepted before [`Server::ready`] are dropped. The
    /// thread ends when `transport` fails to accept, and the failure ends
    /// [`Server::run`].
    pub fn listen<T>(
        &self,
        entrance: Entrance,
        transport: Arc<T>,
        address: String,
        capacity: usize,
    ) -> Result<(), Error>
    where
        T: Transport + 'static,
        T::Connection: 'static,
    {
        let places = Arc::new(AtomicUsize::new(capacity));
        let entrances = self.entrances.clone();
        let faults = self.faults.clone();
        let ready = self.ready.clone();
        thread::Builder::new()
            .name(format!("malak-{entrance:?}"))
            .spawn(move || {
                loop {
                    let link = match transport.accept(&address) {
                        Ok(link) => link,
                        Err(error) => {
                            let _ = faults.send(Error::unavailable(format!(
                                "{entrance:?} entrance listener failed ({error})"
                            )));
                            break;
                        }
                    };
                    if !ready.load(Ordering::Acquire) {
                        continue;
                    }
                    let Some(place) = Place::take(&places) else {
                        continue;
                    };
                    let transport = transport.clone();
                    let entrances = entrances.clone();
                    let _ = thread::Builder::new().spawn(move || {
                        let _place = place;
                        if let Ok(opening) = entrances.open(entrance, transport, link) {
                            match entrance {
                                Entrance::Owner => {
                                    let _ = opening.serve_owner();
                                }
                                Entrance::General | Entrance::Lead => {
                                    if let Ok(participant) = opening.attach_participant() {
                                        let _ = participant.serve();
                                    }
                                }
                            }
                        }
                    });
                }
            })
            .map(drop)
            .map_err(|error| Error::unavailable(format!("listener thread: {error}")))
    }

    /// Start serving the links the listeners accept.
    pub fn ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    /// Wait until the board is stopped, then return `Ok`.
    ///
    /// Returns `Ok` when the owner stops the board or a [`StopHandle`] is used,
    /// and an error when a listener fails.
    pub fn run(self) -> Result<(), Error> {
        self.run_with(|_| {})
    }

    /// Like [`Server::run`], and calls `tick` about once a second.
    #[doc(hidden)]
    pub fn run_with(self, mut tick: impl FnMut(&LocalEntrances)) -> Result<(), Error> {
        let mut next_tick = Instant::now() + TICK;
        loop {
            if Instant::now() >= next_tick {
                tick(&self.entrances);
                next_tick = Instant::now() + TICK;
            }
            match self.fault.recv_timeout(POLL) {
                Ok(error) => return Err(error),
                Err(mpsc::RecvTimeoutError::Timeout)
                    if self.entrances.stopped() || self.stop.load(Ordering::Acquire) =>
                {
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Error::unavailable("entrances stopped"));
                }
            }
        }
    }
}

/// One place in an entrance's capacity, returned when the link ends.
struct Place(Arc<AtomicUsize>);

impl Place {
    fn take(places: &Arc<AtomicUsize>) -> Option<Self> {
        places
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .ok()?;
        Some(Self(places.clone()))
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}
