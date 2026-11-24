#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::LocalEntrances;
use super::LocalOpening;
use crate::link::Segments;
use crate::{Error, Outcome};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use crate::{frame_fields::Fields, grant::read_grant};
use bytes::Bytes;
use malak_layers::Transport;
use malak_proto::{
    ConnectionError, Entrance, OwnerConnection, OwnerLimits, OwnerOperation, OwnerOutcome,
    OwnerRequest, OwnerTicket, owner_outcome,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use malak_proto::{Frame, encode_frame};
use parking_lot::Mutex;
use std::sync::Arc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod serving;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod status;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tail;

/// The protected request stream at the owner door.
///
/// Receiving a request authenticates its frame. Executing an owner operation
/// still requires the operation's shape checks and board save.
pub struct LocalOwner<T: Transport> {
    link: LocalOpening<T>,
    state: Option<State>,
}

struct State {
    protocol: OwnerConnection,
    plaintext: Bytes,
}

impl<T: Transport + 'static> LocalOpening<T>
where
    T::Connection: 'static,
{
    /// Begin the separate owner protocol after a successful owner opening.
    pub fn owner_requests(self) -> Result<LocalOwner<T>, Error> {
        if self.entrance != Entrance::Owner {
            return Err(Error::refused("owner entrance required"));
        }
        let protected = self.protected.as_ref().ok_or_else(closed)?;
        let protocol = OwnerConnection::new(
            OwnerLimits::default(),
            protected.encrypted.handshake_hash(),
            protected.replied.duration_since(self.opened),
        )
        .map_err(owner_protocol_error)?;
        self.wire.sender();
        Ok(LocalOwner {
            link: self,
            state: Some(State {
                protocol,
                plaintext: Bytes::new(),
            }),
        })
    }
}

impl<T: Transport + 'static> LocalOwner<T>
where
    T::Connection: 'static,
{
    /// Read an authenticated owner request. The socket closes on a framing error.
    pub fn next_request(&mut self) -> Result<OwnerRequest, Error> {
        self.link.board.store.running()?;
        let mut state = self.state.take().ok_or_else(closed)?;
        let opened = self.link.opened;
        let deadline = Arc::new(Mutex::new(opened + state.protocol.deadline()));
        let progress = deadline.clone();
        let result = self.link.protect_with(
            || *deadline.lock(),
            move |wire, protected| {
                let request = loop {
                    if let Some(request) = state
                        .protocol
                        .receive(&mut state.plaintext, opened.elapsed())
                        .map_err(owner_protocol_error)?
                    {
                        break request;
                    }
                    state.plaintext =
                        protected
                            .segments
                            .open_with(wire, &mut protected.encrypted, |at| {
                                state
                                    .protocol
                                    .peer_progress(at.duration_since(opened))
                                    .map_err(owner_protocol_error)?;
                                *progress.lock() = opened + state.protocol.deadline();
                                Ok(())
                            })?;
                };
                Ok((state, request))
            },
        );
        let (state, request) = result?;
        self.state = Some(state);
        Ok(request)
    }

    /// Send the outcome of an owner request with no operation-specific reply fields.
    pub fn reply(&mut self, ticket: OwnerTicket, outcome: Result<(), Error>) -> Result<(), Error> {
        let frame = outcome_frame(ticket, outcome)?;
        self.send_reply(ticket, frame)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    /// Execute one owner request and send its outcome.
    ///
    /// The enrolment reply includes the participant ID and handover path.
    /// An enrolment that was saved stands even if the reply cannot be sent.
    pub fn dispatch(&mut self, request: OwnerRequest) -> Result<(), Error> {
        self.state
            .as_ref()
            .ok_or_else(closed)?
            .protocol
            .check_reply(request.ticket)
            .map_err(owner_protocol_error)?;
        if request.operation == OwnerOperation::Tail {
            return self.tail(&request);
        }
        let operations = LocalEntrances {
            board: self.link.board.clone(),
            settings: self.link.settings.clone(),
        };
        let (result, stopping) = operations.answer(&request, self.link.wire.sender());
        let sent = result.and_then(|frame| self.send_reply(request.ticket, frame));
        if stopping {
            self.link.board.finish_stop();
        }
        sent
    }

    fn send_reply(&mut self, ticket: OwnerTicket, frame: Bytes) -> Result<(), Error> {
        let state = self.state.as_mut().ok_or_else(closed)?;
        state
            .protocol
            .check_reply(ticket)
            .map_err(owner_protocol_error)?;
        let deadline = self.link.opened + state.protocol.deadline();
        self.link.protect_with(
            || deadline,
            move |wire, protected| {
                Segments::write_protected(wire, &mut protected.encrypted, &frame)
            },
        )?;
        state
            .protocol
            .reply_sent(ticket, self.link.opened.elapsed())
            .map_err(owner_protocol_error)
    }

    /// Answer a ping or a request outside the owner protocol.
    ///
    /// Return false for operations that require board dispatch.
    pub fn reply_if_immediate(&mut self, request: &OwnerRequest) -> Result<bool, Error> {
        let Some(outcome) = immediate(request) else {
            return Ok(false);
        };
        self.reply(request.ticket, outcome)?;
        Ok(true)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl LocalEntrances {
    fn answer(
        &self,
        request: &OwnerRequest,
        sender: &Arc<malak_layers::SenderPeer>,
    ) -> (Result<Bytes, Error>, bool) {
        let ticket = request.ticket;
        let frame = &request.frame;
        let result = match request.operation {
            OwnerOperation::Status => self.operation_status(frame),
            OwnerOperation::Enrol => self.operation_enrol(frame),
            OwnerOperation::Rekey => self.operation_rekey(frame),
            OwnerOperation::Grant => self.operation_grant(frame).and_then(|()| ok(ticket)),
            OwnerOperation::Revoke => self.operation_revoke(frame).and_then(|()| ok(ticket)),
            OwnerOperation::WorkerExit => self.operation_worker_exit(frame, sender),
            OwnerOperation::Release => self
                .operation_release(frame, sender)
                .and_then(|()| ok(ticket)),
            OwnerOperation::ChannelCreate => self.operation_create(frame),
            OwnerOperation::ChannelRemove => self.operation_remove(frame),
            OwnerOperation::Trim => self.operation_trim(frame),
            OwnerOperation::UpgradeNotice => self
                .operation_upgrade_notice(frame, sender)
                .and_then(|()| ok(ticket)),
            OwnerOperation::Stop => owner_fields(frame)
                .and_then(|fields| fields.required("run")?.parse())
                .and_then(|run| self.board.begin_stop(run))
                .and_then(|()| ok(ticket)),
            OwnerOperation::Cancel => cancel_target(frame).and_then(|_| ok(ticket)),
            other => immediate(request)
                .unwrap_or_else(|| {
                    Err(Error::not_supported(format!(
                        "owner operation {other:?} is unavailable"
                    )))
                })
                .and_then(|()| ok(ticket)),
        };
        let stopping = request.operation == OwnerOperation::Stop && result.is_ok();
        self.record_operation(request, result.as_ref().err(), &sender.trace);
        (
            result.or_else(|error| outcome_frame(ticket, Err(error))),
            stopping,
        )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn record_operation(
        &self,
        request: &OwnerRequest,
        error: Option<&Error>,
        sender: &malak_layers::SenderTrace,
    ) {
        use super::super::OwnerTarget;

        let fields = Fields::new(&request.frame).ok();
        let text = |key| fields.as_ref().and_then(|fields| fields.required(key).ok());
        let target_name = || OwnerTarget::Channel(text("channel"), None);
        let id = || OwnerTarget::Participant(text("id"));
        let (operation, target) = match request.operation {
            OwnerOperation::Enrol => ("ENROL", OwnerTarget::Name(text("name"))),
            OwnerOperation::Grant => ("GRANT", id()),
            OwnerOperation::Revoke => ("REVOKE", id()),
            OwnerOperation::Release => ("RELEASE", id()),
            OwnerOperation::WorkerExit => ("WORKER_EXIT", id()),
            OwnerOperation::Rekey => ("REKEY", id()),
            OwnerOperation::Trim => {
                let record = fields.as_ref().and_then(|fields| {
                    let creation = fields.required("creation").ok()?.parse().ok()?;
                    let position = fields.number("position").ok()??.try_into().ok()?;
                    Some((creation, position))
                });
                ("TRIM", OwnerTarget::Channel(text("channel"), record))
            }
            OwnerOperation::ChannelCreate => ("CHANNEL_CREATE", target_name()),
            OwnerOperation::ChannelRemove => ("CHANNEL_REMOVE", target_name()),
            OwnerOperation::UpgradeNotice => {
                ("UPGRADE_NOTICE", OwnerTarget::Channel(Some("lead"), None))
            }
            OwnerOperation::Stop => (
                "STOP",
                OwnerTarget::Run(text("run").and_then(|run| run.parse().ok())),
            ),
            _ => return,
        };
        self.board
            .events
            .owner_from(operation, target, error, sender);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_upgrade_notice(
        &self,
        frame: &Frame,
        peer: &Arc<malak_layers::SenderPeer>,
    ) -> Result<(), Error> {
        let fields = owner_fields(frame)?;
        fields.only(&["run", "previous_build", "installed_build", "previous_entry"])?;
        let run = fields.required("run")?.parse()?;
        let previous = fields.required("previous_build")?;
        let installed = fields.required("installed_build")?;
        let previous_entry = fields.text("previous_entry")?;
        self.board
            .post_upgrade_notice(run, previous, installed, previous_entry, peer)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_enrol(&self, frame: &Frame) -> Result<Bytes, Error> {
        let fields = owner_fields(frame)?;
        let name = fields.required("name")?;
        let path = fields.required("path")?;
        if !std::path::Path::new(path).is_absolute() {
            return Err(Error::refused("enrolment path must be absolute"));
        }
        let grant = request_grant(&fields)?;
        let only_user = fields
            .number("only_user")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| Error::refused("invalid source account"))?;
        let account = fields
            .number("account")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| Error::refused("invalid handover account"))?
            .unwrap_or(self.settings.board_user);
        let session = self.session_field(&fields)?;
        let previous = session.as_ref().and_then(|_| {
            self.board
                .status(None, None)
                .participants
                .into_iter()
                .find(|person| {
                    !person.id.is_lead()
                        && !person.revoked
                        && person.name.as_str() == name
                        && person
                            .claim
                            .as_ref()
                            .is_some_and(|claim| claim.state != crate::ClaimState::Bound)
                })
        });
        let id = if let Some(previous) = previous {
            let fingerprint = previous
                .fingerprint
                .ok_or_else(|| Error::refused("participant has no current key"))?;
            self.rekey_local(&previous.id, &fingerprint, path.as_ref(), session)?;
            previous.id
        } else {
            self.enrol_at(
                name,
                grant,
                path.as_ref(),
                account,
                only_user.map(malak_layers::Source::LocalUser),
                session,
            )?
        };
        encode_frame(
            frame.rid(),
            "OK",
            2,
            |output| {
                output.str("id").unwrap().str(id.as_str()).unwrap();
                output.str("path").unwrap().str(path).unwrap();
            },
            &[],
        )
        .map_err(|_| Error::unavailable("enrolment reply does not fit"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_worker_exit(
        &self,
        frame: &Frame,
        sender: &Arc<malak_layers::SenderPeer>,
    ) -> Result<Bytes, Error> {
        let fields = owner_fields(frame)?;
        fields.only(&["id", "status", "path", "trace"])?;
        let id = fields.required("id")?.parse()?;
        let status = fields
            .required("status")?
            .parse::<u8>()
            .map_err(|_| Error::refused("invalid worker exit status"))?;
        let (receipt, message) = self.report_worker_exit(
            &id,
            status,
            fields.required("path")?.as_ref(),
            fields.text("trace")?,
            sender,
        )?;
        let lead = crate::board::Handle {
            board: self.board.clone(),
            id: crate::ParticipantId::lead(),
            proof: None,
        };
        let crate::GetReply::Message(record) = lead.get("lead", receipt.position.get())? else {
            return Err(Error::unavailable(
                "the worker's exit report could not be confirmed",
            ));
        };
        if record.id.as_str() != message || record.receipt.creation != receipt.creation {
            return Err(Error::unavailable(
                "the worker's exit report could not be confirmed",
            ));
        }
        super::calls::codec::exit_record(frame.rid(), &record, receipt.duplicate)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_release(
        &self,
        frame: &Frame,
        peer: &malak_layers::SenderPeer,
    ) -> Result<(), Error> {
        let fields = owner_fields(frame)?;
        fields.only(&["id", "confirmation"])?;
        let id = fields.required("id")?.parse()?;
        self.release_claim(&id, peer, fields.text("confirmation")?)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn session_field(
        &self,
        fields: &Fields<'_>,
    ) -> Result<Option<malak_layers::ClaimedSession>, Error> {
        let pid = fields
            .number("session")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| Error::refused("invalid session PID"))?;
        let launch = fields.text("launch_program")?;
        if launch.is_some() && pid.is_none() {
            return Err(Error::refused("worker launch requires a session PID"));
        }
        pid.map(|pid| self.lookup_session(pid, launch)).transpose()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_rekey(&self, frame: &Frame) -> Result<Bytes, Error> {
        let fields = owner_fields(frame)?;
        let id = fields.required("id")?.parse()?;
        let expected = fields.required("fingerprint")?;
        let path = fields.required("path")?;
        if !std::path::Path::new(path).is_absolute() {
            return Err(Error::refused("key replacement path must be absolute"));
        }
        let session = self.session_field(&fields)?;
        self.rekey_local(&id, expected, path.as_ref(), session)?;
        encode_frame(
            frame.rid(),
            "OK",
            1,
            |output| {
                output.str("path").unwrap().str(path).unwrap();
            },
            &[],
        )
        .map_err(|_| Error::unavailable("owner reply does not fit"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_grant(&self, frame: &Frame) -> Result<(), Error> {
        let fields = owner_fields(frame)?;
        let id = fields.required("id")?.parse()?;
        let grant = request_grant(&fields)?;
        let table = self
            .board
            .table
            .save_until(super::super::owner_save_deadline())?;
        self.board.save_grant(table, &id, grant)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_revoke(&self, frame: &Frame) -> Result<(), Error> {
        let fields = owner_fields(frame)?;
        let id = fields.required("id")?.parse()?;
        let table = self
            .board
            .table
            .save_until(super::super::owner_save_deadline())?;
        self.board.save_revoke(table, &id)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_create(&self, frame: &Frame) -> Result<Bytes, Error> {
        let fields = owner_fields(frame)?;
        let (creation, created) = self.board.create_channel(fields.required("channel")?)?;
        encode_frame(
            frame.rid(),
            "OK",
            2,
            |output| {
                output
                    .str("creation")
                    .unwrap()
                    .str(&creation.to_string())
                    .unwrap();
                output.str("created").unwrap().bool(created).unwrap();
            },
            &[],
        )
        .map_err(|_| Error::unavailable("owner reply does not fit"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_remove(&self, frame: &Frame) -> Result<Bytes, Error> {
        let fields = owner_fields(frame)?;
        let name = fields.required("channel")?;
        let creation = fields.required("creation")?.parse()?;
        let (removed, current) = self.board.remove_channel(name, creation)?;
        encode_frame(
            frame.rid(),
            "OK",
            1 + usize::from(current.is_some()),
            |output| {
                output.str("removed").unwrap().bool(removed).unwrap();
                if let Some(current) = current {
                    output
                        .str("creation")
                        .unwrap()
                        .str(&current.to_string())
                        .unwrap();
                }
            },
            &[],
        )
        .map_err(|_| Error::unavailable("owner reply does not fit"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn operation_trim(&self, frame: &Frame) -> Result<Bytes, Error> {
        let fields = owner_fields(frame)?;
        let name = fields.required("channel")?;
        let creation = fields.required("creation")?.parse()?;
        let position = fields
            .number("position")?
            .ok_or_else(|| Error::refused("missing trim position"))?;
        let floor = self.board.trim(name, creation, position)?;
        encode_frame(
            frame.rid(),
            "OK",
            1,
            |output| {
                output.str("floor").unwrap().u64(floor).unwrap();
            },
            &[],
        )
        .map_err(|_| Error::unavailable("owner reply does not fit"))
    }
}

fn immediate(request: &OwnerRequest) -> Option<Result<(), Error>> {
    Some(match request.operation {
        OwnerOperation::Ping if request.frame.body().is_empty() => Ok(()),
        OwnerOperation::Ping => Err(Error::refused("ping takes no body")),
        OwnerOperation::Unsupported => Err(Error::not_supported("unknown owner operation")),
        _ => return None,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn owner_fields(frame: &Frame) -> Result<Fields<'_>, Error> {
    if !frame.body().is_empty() {
        return Err(Error::refused("owner operation takes no body"));
    }
    Fields::new(frame)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn request_grant(fields: &Fields<'_>) -> Result<crate::Grant, Error> {
    read_grant(
        fields
            .get("grant")
            .ok_or_else(|| Error::refused("missing grant"))?,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cancel_target(frame: &Frame) -> Result<u64, Error> {
    owner_fields(frame)?
        .number("target")?
        .ok_or_else(|| Error::refused("cancel target is missing"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ok(ticket: OwnerTicket) -> Result<Bytes, Error> {
    outcome_frame(ticket, Ok(()))
}

fn outcome_frame(ticket: OwnerTicket, result: Result<(), Error>) -> Result<Bytes, Error> {
    let reason;
    let outcome = match result {
        Ok(()) => OwnerOutcome::Done,
        Err(error) => {
            reason = error.to_string();
            OwnerOutcome::Error {
                outcome: outcome_name(error.outcome()),
                reason: &reason,
            }
        }
    };
    owner_outcome(ticket.rid(), outcome).map_err(|_| Error::unavailable("owner reply does not fit"))
}

fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Done => "done",
        Outcome::Refused => "refused",
        Outcome::Unavailable => "unavailable",
        Outcome::NotSupported => "not_supported",
    }
}

fn owner_protocol_error(_: ConnectionError) -> Error {
    Error::refused("owner protocol rejected the frame or link")
}

fn closed() -> Error {
    Error::unavailable("owner link is closed")
}
