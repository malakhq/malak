use super::{Board, Owner};
use crate::Error;
use crate::entrance_slots::{ActiveGate, Gate, Place};
use crate::link::{Segments, Wire};
use crate::workers::Workers;
use bytes::Bytes;
use malak_layers::{Link, Mechanism, Source, Transport};
use malak_proto::{Binding, Encrypted, Entrance, KeyPair, Responder, SegmentKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod attach;
mod calls;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod claims;
mod owner;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod provision;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use provision::LocalStartup;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;

pub use attach::LocalParticipant;
pub use owner::LocalOwner;

/// The board's local entrance keys and the accounts allowed at its private doors.
pub struct LocalConfig {
    /// The board key that clients pin in their handover.
    pub board_key: KeyPair,
    /// Public owner key. The private key stays with the owner.
    pub owner_key: [u8; 32],
    /// Public lead key. The private key stays with the lead.
    pub lead_key: [u8; 32],
    /// Local board account, required at the owner door.
    pub board_user: u32,
    /// Local lead account, required at the lead entrance.
    pub lead_user: u32,
}

/// Board-issued authentication for already accepted local links.
///
/// The caller supplies links accepted at directories that enforce the local
/// entrance policy. A transport layer cannot itself authenticate a board role.
#[derive(Clone)]
pub struct LocalEntrances {
    board: Arc<Board>,
    settings: Arc<Settings>,
}

type SessionLookup = fn(u32) -> Option<malak_layers::ClaimedSession>;

struct Settings {
    session_lookup: Option<SessionLookup>,
    starting_session_lookup: Option<SessionLookup>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    process_identity: Option<fn(u32) -> Option<malak_layers::ProcessIdentity>>,
    board_key: KeyPair,
    board_user: u32,
    owner_key: [u8; 32],
    lead_user: u32,
    mechanism: Arc<dyn Mechanism>,
    proves: malak_layers::Proves,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    provisioning: Option<provision::Provisioning>,
    general: LocalGate,
    lead: LocalGate,
    owner: LocalGate,
    general_attached: Arc<ActiveGate>,
    lead_attached: Arc<ActiveGate>,
}

struct LocalGate {
    gate: Arc<Gate>,
    timeout: Duration,
    workers: Workers,
}

impl LocalGate {
    fn new(capacity: usize, per_source: Option<usize>, rate: bool, seconds: u64) -> Self {
        Self {
            gate: Gate::new(capacity, per_source, rate),
            timeout: Duration::from_secs(seconds),
            workers: Workers::named("malak-opening"),
        }
    }
}

impl Settings {
    fn entrance(&self, entrance: Entrance) -> &LocalGate {
        match entrance {
            Entrance::General => &self.general,
            Entrance::Lead => &self.lead,
            Entrance::Owner => &self.owner,
        }
    }
}

/// A local link after the board has answered its Noise opening.
///
/// An opening confers no capability. The next protected frame still has to
/// authenticate and satisfy the protocol of the selected entrance.
pub struct LocalOpening<T: Transport> {
    wire: Arc<Wire<T>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    live: Arc<super::links::LiveLink>,
    protected: Option<Protected>,
    board: Arc<Board>,
    settings: Arc<Settings>,
    entrance: Entrance,
    place: Option<Place>,
    caller_key: [u8; 32],
    opened: Instant,
    deadline: Instant,
}

struct Protected {
    segments: Segments,
    encrypted: Encrypted,
    replied: Instant,
}

impl Owner {
    /// Configure the local entrances once, using the board's and owner's credentials.
    ///
    /// Use the returned capability only on trusted listeners for this board.
    pub fn local_entrances(
        &self,
        config: LocalConfig,
        general: Arc<dyn Mechanism>,
    ) -> Result<LocalEntrances, Error> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if self.board.store.journaled() {
            return Err(Error::not_supported(
                "durable local credentials require board startup provisioning",
            ));
        }
        self.configure_local(
            config,
            general,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            None,
        )
    }

    fn configure_local(
        &self,
        config: LocalConfig,
        general: Arc<dyn Mechanism>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] provisioning: Option<
            provision::Provisioning,
        >,
    ) -> Result<LocalEntrances, Error> {
        let mut table = self.board.table.save();
        table.authority.set_local_keys(
            *config.board_key.public(),
            config.owner_key,
            config.lead_key,
            config.lead_user,
        )?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.publish(&table);
        drop(table);
        Ok(self.local_settings(
            config,
            general,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            provisioning,
        ))
    }

    fn local_settings(
        &self,
        config: LocalConfig,
        general: Arc<dyn Mechanism>,
        #[cfg(any(target_os = "linux", target_os = "macos"))] provisioning: Option<
            provision::Provisioning,
        >,
    ) -> LocalEntrances {
        let proves = general.describe();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let _ = self.board.local_mechanism.set(general.clone());
        let settings = Arc::new(Settings {
            session_lookup: None,
            starting_session_lookup: None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            process_identity: None,
            board_key: config.board_key,
            board_user: config.board_user,
            owner_key: config.owner_key,
            lead_user: config.lead_user,
            mechanism: general,
            proves,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            provisioning,
            general: LocalGate::new(64, Some(8), true, 5),
            lead: LocalGate::new(16, None, true, 5),
            owner: LocalGate::new(8, None, false, 10),
            general_attached: ActiveGate::new(1024, 16),
            lead_attached: ActiveGate::new(16, 16),
        });
        LocalEntrances {
            board: self.board.clone(),
            settings,
        }
    }
}

impl LocalEntrances {
    /// Install the board host's process lookup before sharing the entrances.
    pub fn set_session_lookup(
        &mut self,
        lookup: SessionLookup,
        starting: SessionLookup,
    ) -> Result<(), Error> {
        let settings = Arc::get_mut(&mut self.settings)
            .ok_or_else(|| Error::refused("entrances already in use"))?;
        settings.session_lookup = Some(lookup);
        settings.starting_session_lookup = Some(starting);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn lookup_session(
        &self,
        pid: u32,
        launch: Option<&str>,
    ) -> Result<malak_layers::ClaimedSession, Error> {
        let lookup = if launch.is_some() {
            self.settings.starting_session_lookup
        } else {
            self.settings.session_lookup
        }
        .ok_or_else(|| Error::not_supported("local session binding is unavailable"))?;
        let mut session = lookup(pid)
            .ok_or_else(|| Error::refused(format!("session process {pid} is not running")))?;
        if let Some(program) = launch {
            if program.is_empty() || program.len() > 255 {
                return Err(Error::refused("invalid worker program"));
            }
            session.process.program = program.into();
        }
        Ok(session)
    }

    /// Install the board host's PID and start-time lookup before opening listeners.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn set_process_identity(
        &mut self,
        lookup: fn(u32) -> Option<malak_layers::ProcessIdentity>,
    ) -> Result<(), Error> {
        Arc::get_mut(&mut self.settings)
            .ok_or_else(|| Error::refused("entrances already in use"))?
            .process_identity = Some(lookup);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn check_session_running(
        &self,
        session: Option<&malak_layers::ClaimedSession>,
    ) -> Result<(), Error> {
        if let (Some(session), Some(lookup)) = (session, self.settings.process_identity) {
            let process = session.process.identity;
            if lookup(process.pid) != Some(process) {
                return Err(Error::refused(format!(
                    "session process {} is not running",
                    process.pid
                )));
            }
        }
        Ok(())
    }

    /// Whether an owner stop completed and released the board's state.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn stopped(&self) -> bool {
        self.board
            .stopped
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Read and filter a Noise opening and send its reply before the deadline.
    ///
    /// Call at acceptance, passing the listener's entrance and the transport's
    /// observed peer. On any refusal the link closes without a response.
    pub fn open<T: Transport + 'static>(
        &self,
        entrance: Entrance,
        transport: Arc<T>,
        link: Link<T::Connection>,
    ) -> Result<LocalOpening<T>, Error>
    where
        T::Connection: 'static,
    {
        self.board.store.running()?;
        let opened = Instant::now();
        let wire = Arc::new(Wire::new(transport, link));
        let source = match entrance {
            Entrance::General => None,
            Entrance::Lead => Some(self.settings.lead_user),
            Entrance::Owner => Some(self.settings.board_user),
        };
        if let Some(user) = source
            && wire.source() != Some(&Source::LocalUser(user))
        {
            return Err(Error::refused("source is outside the entrance's zone"));
        }

        let gate = self.settings.entrance(entrance);
        let place = gate.gate.reserve(wire.source(), opened)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let live = self.board.links.register(wire.clone())?;
        let deadline = opened + gate.timeout;
        let entries = self.clone();
        let link = wire.clone();
        let result = gate.workers.until(deadline, move || {
            let mut segments = Segments::new();
            let opening = segments.receive(&link, SegmentKind::Opening)?;
            let board_key = &entries.settings.board_key;
            let responder = Responder::read(
                Binding {
                    version: 1,
                    entrance,
                },
                board_key.private(),
                board_key.public(),
                &opening,
            )
            .map_err(|_| Error::refused("opening failed authentication"))?;
            let caller_key = *responder.caller_key();
            if entrance == Entrance::Owner {
                if caller_key != entries.settings.owner_key {
                    return Err(Error::refused("key is not admissible at this entrance"));
                }
            } else {
                entries.board.table.check().authority.check_opening(
                    entrance,
                    &caller_key,
                    entries.settings.proves.key_at_enrolment,
                )?;
            }
            if Instant::now() >= deadline {
                return Err(Error::refused("opening deadline reached"));
            }
            let (encrypted, reply) = responder
                .reply()
                .map_err(|_| Error::refused("opening failed authentication"))?;
            Segments::send(&link, SegmentKind::Reply, &reply)?;
            Ok((
                Protected {
                    segments,
                    encrypted,
                    replied: Instant::now(),
                },
                caller_key,
            ))
        });
        let (protected, caller_key) = match result {
            Ok(result) => result,
            Err(error) => {
                wire.close();
                return Err(error);
            }
        };

        Ok(LocalOpening {
            wire,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            live,
            protected: Some(protected),
            board: self.board.clone(),
            settings: self.settings.clone(),
            entrance,
            place: Some(place),
            caller_key,
            opened,
            deadline,
        })
    }
}

impl<T: Transport + 'static> LocalOpening<T>
where
    T::Connection: 'static,
{
    fn read_segment(&mut self) -> Result<Bytes, Error> {
        self.protect(|wire, state| state.segments.open(wire, &mut state.encrypted))
    }

    fn protect<R: Send + 'static>(
        &mut self,
        work: impl FnOnce(&Wire<T>, &mut Protected) -> Result<R, Error> + Send + 'static,
    ) -> Result<R, Error> {
        let deadline = self.deadline;
        self.protect_with(|| deadline, work)
    }

    fn protect_with<R: Send + 'static>(
        &mut self,
        deadline: impl Fn() -> Instant,
        work: impl FnOnce(&Wire<T>, &mut Protected) -> Result<R, Error> + Send + 'static,
    ) -> Result<R, Error> {
        let mut state = self
            .protected
            .take()
            .ok_or_else(|| Error::unavailable("link is closed"))?;
        let link = self.wire.clone();
        let result =
            self.settings
                .entrance(self.entrance)
                .workers
                .until_with(deadline, move || {
                    let result = work(&link, &mut state)?;
                    Ok((state, result))
                });
        match result {
            Ok((state, value)) => {
                self.protected = Some(state);
                Ok(value)
            }
            Err(error) => {
                self.wire.close();
                Err(error)
            }
        }
    }
}
