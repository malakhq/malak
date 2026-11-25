use crate::link::{Segments, Wire};
use crate::workers::Workers;
use crate::{Error, HandoverFile, OwnerCredential};
use crate::{Grant, ParticipantId, frame_fields::Fields, grant::write_grant};
use bytes::Bytes;
use malak_layers::{Mechanism, Session, Side, Step, Transport};
use malak_proto::{
    Binding, ConnectionLimits, Encrypted, Entrance, Frame, FrameDecoder, Initiator, KeyPair,
    SegmentKind, attach_request, attach_step, encode_frame,
};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The client side of a protected local board connection.
///
/// A program can submit a framed request, then read its protected replies.
/// Each call supplies the amount of time it is willing to wait.
pub struct LocalClient<T: Transport> {
    wire: Arc<Wire<T>>,
    state: Option<State>,
    workers: Workers,
    expires: Instant,
}

struct State {
    segments: Segments,
    encrypted: Encrypted,
    frames: FrameDecoder,
    buffered: Bytes,
}

impl<T: Transport + 'static> LocalClient<T>
where
    T::Connection: 'static,
{
    /// Connect to the owner door with credentials from the state folder.
    pub fn owner(transport: Arc<T>, credential: OwnerCredential) -> Result<Self, Error> {
        let address = credential.address().to_owned();
        let binding = Binding {
            version: 1,
            entrance: Entrance::Owner,
        };
        Self::open(transport, address, Duration::from_secs(10), move |wire| {
            State::handshake(
                wire,
                binding,
                credential.board_key(),
                credential.private_key(),
            )
        })
    }

    /// Connect and complete participant or lead attachment with the supplied handover.
    ///
    /// Supply the handover's configured mechanism for a general participant.
    /// The lead's entrance has no mechanism.
    pub fn participant(
        transport: Arc<T>,
        handover: HandoverFile,
        mechanism: Option<Arc<dyn Mechanism>>,
    ) -> Result<Self, Error> {
        Self::participant_with_wait(
            transport,
            handover,
            mechanism,
            ConnectionLimits::default().attach_time,
        )
    }

    /// Connect using a handover file that may be replaced while the caller runs.
    ///
    /// Select the configured mechanism for each fresh handover, or return
    /// `None` for the lead. An attachment failure triggers an immediate retry
    /// followed by increasing delays. Every attempt rereads the file. The
    /// library reports `unavailable` if it cannot attach within 30 seconds.
    pub fn participant_from_file(
        transport: Arc<T>,
        path: impl AsRef<Path>,
        select: impl Fn(&HandoverFile) -> Result<Option<Arc<dyn Mechanism>>, Error>,
    ) -> Result<Self, Error> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let path = Arc::new(path.as_ref().to_owned());
        let files = Workers::named("malak-handover");
        let load = || {
            let path = path.clone();
            files.until(deadline, move || HandoverFile::read(path.as_path()))
        };
        let mut initial = Some(load()?);
        let mut delay = Duration::ZERO;
        let attach_time = ConnectionLimits::default().attach_time;
        while Instant::now() < deadline {
            if let Some(handover) = initial.take().or_else(|| load().ok()) {
                let mechanism = select(&handover)?;
                let left = deadline.saturating_duration_since(Instant::now());
                if !left.is_zero() {
                    match Self::participant_with_wait(
                        transport.clone(),
                        handover,
                        mechanism,
                        left.min(attach_time),
                    ) {
                        Ok(client) => return Ok(client),
                        Err(error) if error == attachment_closed() => return Err(error),
                        Err(_) => {}
                    }
                }
            }
            std::thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
            delay = (delay * 2).clamp(Duration::from_millis(50), Duration::from_secs(2));
        }
        Err(Error::unavailable("could not attach"))
    }

    /// Connect and attach within the supplied time, capped at the attachment limit.
    pub fn participant_with_wait(
        transport: Arc<T>,
        handover: HandoverFile,
        mechanism: Option<Arc<dyn Mechanism>>,
        wait: Duration,
    ) -> Result<Self, Error> {
        if (handover.binding().entrance == Entrance::Lead) != mechanism.is_none() {
            return Err(Error::refused("wrong mechanism for the local entrance"));
        }
        let address = handover.address().to_owned();
        let wait = wait.min(ConnectionLimits::default().attach_time);
        Self::open(transport, address, wait, move |wire| {
            let mut state = State::handshake(
                wire,
                handover.binding(),
                handover.board_key(),
                handover.private_key(),
            )?;
            state.attach(wire, &handover, mechanism.as_deref())?;
            Ok(state)
        })
    }

    /// Request a durable enrolment through an owner connection.
    ///
    /// The path is absolute on the board host. Only the board writes the
    /// handover file. The reply contains its path and the new participant ID.
    pub fn enrol(
        &mut self,
        name: &str,
        grant: &Grant,
        path: &str,
        only_user: Option<u32>,
    ) -> Result<(ParticipantId, String), Error> {
        self.enrol_for_account(name, grant, path, None, only_user)
    }

    /// Enrol a participant and share its handover with a specific local account.
    ///
    /// Omit the account to use the board's UID. The allowed source is optional
    /// and independent of the account that can read the file.
    pub fn enrol_for_account(
        &mut self,
        name: &str,
        grant: &Grant,
        path: &str,
        account: Option<u32>,
        only_user: Option<u32>,
    ) -> Result<(ParticipantId, String), Error> {
        self.enrol_with_session(name, grant, path, account, only_user, None)
    }

    /// Enrol with an optional PID resolved to a session on the board host.
    pub fn enrol_with_session(
        &mut self,
        name: &str,
        grant: &Grant,
        path: &str,
        account: Option<u32>,
        only_user: Option<u32>,
        session: Option<u32>,
    ) -> Result<(ParticipantId, String), Error> {
        self.enrol_with_binding(
            name,
            grant,
            path,
            account,
            only_user,
            session.map(|pid| (pid, None)),
        )
    }

    /// Issue a worker handover while its process waits to start the command.
    pub fn enrol_starting_session(
        &mut self,
        name: &str,
        grant: &Grant,
        path: &str,
        account: Option<u32>,
        only_user: Option<u32>,
        launch: (u32, &str),
    ) -> Result<(ParticipantId, String), Error> {
        self.enrol_with_binding(
            name,
            grant,
            path,
            account,
            only_user,
            Some((launch.0, Some(launch.1))),
        )
    }

    fn enrol_with_binding(
        &mut self,
        name: &str,
        grant: &Grant,
        path: &str,
        account: Option<u32>,
        only_user: Option<u32>,
        session: Option<(u32, Option<&str>)>,
    ) -> Result<(ParticipantId, String), Error> {
        let launch = session.and_then(|(_, launch)| launch);
        let rid = 1;
        let request = encode_frame(
            rid,
            "ENROL",
            3 + usize::from(only_user.is_some())
                + usize::from(account.is_some())
                + usize::from(session.is_some())
                + usize::from(launch.is_some()),
            |output| {
                output.str("name").unwrap().str(name).unwrap();
                output.str("path").unwrap().str(path).unwrap();
                output.str("grant").unwrap();
                write_grant(output, grant);
                if let Some(uid) = account {
                    output.str("account").unwrap().u32(uid).unwrap();
                }
                if let Some(uid) = only_user {
                    output.str("only_user").unwrap().u32(uid).unwrap();
                }
                if let Some((pid, _)) = session {
                    output.str("session").unwrap().u32(pid).unwrap();
                }
                if let Some(program) = launch {
                    output.str("launch_program").unwrap().str(program).unwrap();
                }
            },
            &[],
        )
        .map_err(|_| Error::refused("invalid enrolment request"))?;
        let reply = self.owner_call(request, rid)?;
        let fields = Fields::new(&reply)?;
        Ok((
            fields.required("id")?.parse()?,
            fields.required("path")?.into(),
        ))
    }

    /// Replace the owner's entire grant for an active participant.
    pub fn replace_grant(&mut self, id: &ParticipantId, grant: &Grant) -> Result<(), Error> {
        let rid = 1;
        let request = encode_frame(
            rid,
            "GRANT",
            2,
            |output| {
                output.str("id").unwrap().str(id.as_str()).unwrap();
                output.str("grant").unwrap();
                write_grant(output, grant);
            },
            &[],
        )
        .map_err(|_| Error::refused("invalid grant request"))?;
        self.owner_call(request, rid).map(|_| ())
    }

    /// Replace a key when its current fingerprint matches the request.
    ///
    /// Returns the path of the replacement handover on the board host.
    pub fn rekey(
        &mut self,
        id: &ParticipantId,
        fingerprint: &str,
        path: &str,
    ) -> Result<String, Error> {
        self.rekey_session(id, fingerprint, path, None)
    }

    /// Replace a key and optionally bind the replacement to a local session.
    pub fn rekey_session(
        &mut self,
        id: &ParticipantId,
        fingerprint: &str,
        path: &str,
        session: Option<u32>,
    ) -> Result<String, Error> {
        let rid = 1;
        let frame = encode_frame(
            rid,
            "REKEY",
            3 + usize::from(session.is_some()),
            |output| {
                output.str("id").unwrap().str(id.as_str()).unwrap();
                output.str("fingerprint").unwrap().str(fingerprint).unwrap();
                output.str("path").unwrap().str(path).unwrap();
                if let Some(pid) = session {
                    output.str("session").unwrap().u32(pid).unwrap();
                }
            },
            &[],
        )
        .map_err(|_| Error::refused("invalid key replacement request"))?;
        let reply = self.owner_call(frame, rid)?;
        Ok(Fields::new(&reply)?.required("path")?.into())
    }

    /// Send a single owner request and read its closing outcome.
    ///
    /// Use this with a frame carrying the supplied request number.
    /// A streaming request such as TAIL uses the frame methods instead.
    pub fn owner_call(&mut self, request: Bytes, rid: u64) -> Result<Frame, Error> {
        self.send_frame(request, Duration::from_secs(5))?;
        let reply = self.read_frame(Duration::from_secs(60))?;
        Self::owner_reply(reply, rid)
    }

    /// Interpret an authenticated closing reply from the owner door.
    pub fn owner_reply(reply: Frame, rid: u64) -> Result<Frame, Error> {
        if reply.rid() != rid {
            return Err(Error::unavailable("invalid owner reply"));
        }
        match reply.op() {
            "OK" => Ok(reply),
            "ERR" if reply.body().is_empty() => {
                let fields = Fields::new(&reply)?;
                let reason = fields.required("reason")?.to_owned();
                match fields.required("outcome")? {
                    "refused" => Err(Error::refused(reason)),
                    "not_supported" => Err(Error::not_supported(reason)),
                    "unavailable" => Err(Error::unavailable(reason)),
                    _ => Err(Error::unavailable("invalid owner outcome")),
                }
            }
            _ => Err(Error::unavailable("invalid owner reply")),
        }
    }

    /// Send one complete request frame on this connection.
    pub fn send_frame(&mut self, frame: Bytes, wait: Duration) -> Result<(), Error> {
        self.drive(wait, move |wire, state| state.send(wire, &frame))
    }

    /// Read the next complete reply, preserving any subsequent plaintext.
    pub fn read_frame(&mut self, wait: Duration) -> Result<Frame, Error> {
        self.drive(wait, |wire, state| state.read(wire))
    }

    fn open(
        transport: Arc<T>,
        address: String,
        wait: Duration,
        connect: impl FnOnce(&Wire<T>) -> Result<State, Error> + Send + 'static,
    ) -> Result<Self, Error> {
        let opened = Instant::now();
        let deadline = opened + wait;
        let workers = Workers::named("malak-client");
        let remote = transport.clone();
        let link = workers.until(deadline, move || remote.dial(&address))?;
        let wire = Arc::new(Wire::new(transport, link));
        let work = wire.clone();
        match workers.until(deadline, move || connect(&work)) {
            Ok(state) => Ok(Self {
                wire,
                state: Some(state),
                workers,
                expires: opened + ConnectionLimits::default().lifetime,
            }),
            Err(error) => {
                wire.close();
                Err(error)
            }
        }
    }

    fn drive<R: Send + 'static>(
        &mut self,
        wait: Duration,
        work: impl FnOnce(&Wire<T>, &mut State) -> Result<R, Error> + Send + 'static,
    ) -> Result<R, Error> {
        let mut state = self
            .state
            .take()
            .ok_or_else(|| Error::unavailable("link is closed"))?;
        let deadline = Instant::now()
            .checked_add(wait)
            .map_or(self.expires, |at| at.min(self.expires));
        if Instant::now() >= deadline {
            self.wire.close();
            return Err(Error::unavailable("the link deadline passed"));
        }
        let wire = self.wire.clone();
        match self.workers.until(deadline, move || {
            let value = work(&wire, &mut state)?;
            Ok((state, value))
        }) {
            Ok((state, value)) => {
                self.state = Some(state);
                Ok(value)
            }
            Err(error) => {
                self.wire.close();
                Err(error)
            }
        }
    }
}

impl State {
    fn handshake<T: Transport>(
        wire: &Wire<T>,
        binding: Binding,
        pinned: &[u8; 32],
        private: &[u8; 32],
    ) -> Result<Self, Error> {
        let (opening, request) =
            Initiator::open(binding, private, pinned).map_err(|_| handshake_error())?;
        let mut segments = Segments::new();
        Segments::send(wire, SegmentKind::Opening, &request)?;
        let reply = segments.receive(wire, SegmentKind::Reply)?;
        let encrypted = opening.finish(&reply).map_err(|_| handshake_error())?;
        let frames = if binding.entrance == Entrance::Owner {
            FrameDecoder::with_frame_limit((isize::MAX as usize).min(u32::MAX as usize))
        } else {
            FrameDecoder::new(ConnectionLimits::default().body_bytes, None)
        }
        .map_err(|_| Error::unavailable("invalid client frame limit"))?;
        Ok(Self {
            segments,
            encrypted,
            frames,
            buffered: Bytes::new(),
        })
    }

    fn attach<T: Transport>(
        &mut self,
        wire: &Wire<T>,
        handover: &HandoverFile,
        mechanism: Option<&dyn Mechanism>,
    ) -> Result<(), Error> {
        let public_key =
            KeyPair::public_for(handover.private_key()).map_err(|_| handshake_error())?;
        let binding = *self.encrypted.handshake_hash();
        let session = Session {
            side: Side::Participant,
            public_key: &public_key,
            binding: &binding,
            source: None,
            claimed: handover.id(),
            record: None,
            handover: Some(handover.mechanism_data()),
        };
        let mut outgoing = answer(mechanism, &session, &[])?;
        for rid in 1..=ConnectionLimits::default().attach_rounds as u64 {
            let request = if rid == 1 {
                attach_request(rid, handover.id().as_str(), &outgoing)
            } else {
                attach_step(rid, &outgoing)
            }
            .map_err(|_| Error::refused("invalid proof request"))?;
            self.send(wire, &request)?;
            let reply = self.read(wire).map_err(|error| {
                if handover.address().starts_with("unix:")
                    && error == Error::unavailable("link closed while reading a segment")
                {
                    attachment_closed()
                } else {
                    error
                }
            })?;
            if reply.rid() != rid {
                return Err(handshake_error());
            }
            match reply.op() {
                "OK" if reply.body().is_empty() => return Ok(()),
                "ERR" if reply.body().is_empty() => {
                    return Err(Error::unavailable("no attached place"));
                }
                "STEP" if mechanism.is_some() => {
                    outgoing = answer(mechanism, &session, reply.body())?;
                }
                _ => return Err(handshake_error()),
            }
        }
        Err(handshake_error())
    }

    fn send<T: Transport>(&mut self, wire: &Wire<T>, frame: &[u8]) -> Result<(), Error> {
        Segments::write_protected(wire, &mut self.encrypted, frame)
    }

    fn read<T: Transport>(&mut self, wire: &Wire<T>) -> Result<Frame, Error> {
        loop {
            if let Some(frame) = self
                .frames
                .pull(&mut self.buffered)
                .map_err(|_| Error::unavailable("invalid board reply"))?
            {
                return Ok(frame);
            }
            self.buffered = self.segments.open(wire, &mut self.encrypted)?;
        }
    }
}

fn answer(
    mechanism: Option<&dyn Mechanism>,
    session: &Session<'_>,
    input: &[u8],
) -> Result<Vec<u8>, Error> {
    let Some(mechanism) = mechanism else {
        return Ok(Vec::new());
    };
    match mechanism
        .step(session, input)
        .map_err(|_| handshake_error())?
    {
        Step::More(bytes) => Ok(bytes),
        Step::Proven { .. } => Ok(Vec::new()),
        Step::Denied => Err(handshake_error()),
    }
}

fn attachment_closed() -> Error {
    Error::refused(
        "the board closed the attachment. If this handover is in use by another session, re-issue it with malak enrol NAME --session PID, malak recipe worker-start NAME --again, or malak recipe lead-up",
    )
}

fn handshake_error() -> Error {
    Error::refused("board proof did not complete")
}
