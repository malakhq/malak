use super::LocalOpening;
use crate::authority::{Proof, Verified};
use crate::entrance_slots::ActivePlace;
use crate::link::Segments;
use crate::{Error, Grade, ParticipantId};
use malak_layers::{Session, Side, Step, Transport};
use malak_proto::{
    AttachResponse, Attachment, Connection, ConnectionError, ConnectionLimits, Entrance, Input,
    Request, attach_id, attach_reply,
};
use std::time::Instant;

/// An attached participant link tied to a board, participant and authority generation.
pub struct LocalParticipant<T: Transport> {
    pub(super) identity: Verified,
    pub(super) attached: Option<Attached<T>>,
}

pub(super) struct Attached<T: Transport> {
    pub(super) link: LocalOpening<T>,
    pub(super) connection: Connection,
    pub(super) place: ActivePlace,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) count: crate::authority::ConnectionCount,
}

impl<T: Transport> LocalParticipant<T> {
    /// The ID the board authenticated on this link.
    pub fn id(&self) -> &ParticipantId {
        &self.identity.id
    }

    /// What the board established when this link attached.
    pub const fn grade(&self) -> Grade {
        self.identity.grade
    }

    /// Recheck authority and connection lifetime. Failure closes the link.
    pub fn check_valid(&mut self) -> Result<(), Error> {
        let attached = self.attached.as_mut().ok_or_else(closed)?;
        let authority = attached
            .link
            .board
            .table
            .check()
            .authority
            .validate_attached(&self.identity);
        let result = authority.and_then(|_| {
            attached
                .connection
                .tick(attached.link.opened.elapsed())
                .map_err(protocol_error)
        });
        if result.is_err() {
            self.attached = None;
        }
        result
    }
}

impl<T: Transport + 'static> LocalOpening<T>
where
    T::Connection: 'static,
{
    /// Authenticate and attach a participant or lead through its protected exchange.
    ///
    /// The owner door has no attachment. A refused proof closes without a reason.
    pub fn attach_participant(mut self) -> Result<LocalParticipant<T>, Error> {
        let gate = match self.entrance {
            Entrance::General => self.settings.general_attached.clone(),
            Entrance::Lead => self.settings.lead_attached.clone(),
            Entrance::Owner => return Err(closed()),
        };

        let mut limits = ConnectionLimits::default();
        limits.body_bytes = self.board.limits.body_bytes;
        limits.message_bytes = self.board.limits.outgoing_message_bytes;
        limits.open_data_requests = self.board.limits.open_data_requests;
        limits.reply_places = self.board.limits.closing_reply_places;
        let mut connection = Connection::new(limits).map_err(protocol_error)?;
        let binding = *self
            .protected
            .as_ref()
            .ok_or_else(closed)?
            .encrypted
            .handshake_hash();
        connection
            .handshake_reply_sent(&binding, self.opened.elapsed())
            .map_err(protocol_error)?;

        let mut request = self.attachment_request(&mut connection)?;
        let id = attach_id(&request.frame).map_err(|_| closed())?.parse()?;
        let attempt = self.board.table.check().authority.begin_attachment(
            self.entrance,
            id,
            self.caller_key,
            self.wire.source(),
        )?;

        let is_lead = self.entrance == Entrance::Lead;
        let highest_grade = if is_lead {
            Grade::Proven
        } else {
            self.settings.proves.highest_grade
        };
        for round in 1..=limits.attach_rounds {
            let answer = if is_lead {
                Step::Proven {
                    ends: None,
                    register_key: false,
                    record: None,
                }
            } else {
                let mechanism = self.settings.mechanism.clone();
                let source = self.wire.source().cloned();
                let id = attempt.id.clone();
                let key = attempt.key;
                let record = attempt.record.clone();
                let frame = request.frame;
                self.settings
                    .entrance(self.entrance)
                    .workers
                    .until(self.deadline, move || {
                        mechanism.step(
                            &Session {
                                side: Side::Board,
                                public_key: &key,
                                binding: &binding,
                                source: source.as_ref(),
                                claimed: &id,
                                record: record.as_deref(),
                                handover: None,
                            },
                            frame.body(),
                        )
                    })?
            };

            let ticket = request.ticket;
            match answer {
                Step::Denied => return Err(closed()),
                Step::More(payload)
                    if round < limits.attach_rounds && payload.len() <= limits.body_bytes =>
                {
                    self.write_attachment(ticket.rid(), AttachResponse::Step(&payload))?;
                    connection
                        .attachment_reply_sent(ticket, Attachment::More, self.opened.elapsed())
                        .map_err(protocol_error)?;
                    request = self.attachment_request(&mut connection)?;
                }
                Step::More(_) => return Err(closed()),
                Step::Proven {
                    ends,
                    register_key,
                    record,
                } => {
                    let board = self.board.clone();
                    let mut table = board.table.save_until(self.deadline)?;
                    if Instant::now() >= self.deadline {
                        return Err(closed());
                    }
                    let identity = table.authority.finish_attachment(
                        attempt,
                        Proof {
                            highest_grade,
                            ends,
                            register_key,
                            record,
                        },
                        |entry| board.store.save(entry),
                    )?;
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if register_key {
                        board.publish(&table);
                    }
                    let Some(place) = gate.reserve(&identity.id) else {
                        drop(table);
                        self.write_attachment(ticket.rid(), AttachResponse::Unavailable)?;
                        return Err(Error::unavailable("no attached place"));
                    };
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    let count = table.authority.count_connection(&identity.id);
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    self.live.identify(&identity.id);
                    drop(table);
                    board.table.check().authority.validate_attached(&identity)?;
                    self.wire.sender();
                    self.write_attachment(
                        ticket.rid(),
                        AttachResponse::Accepted(limits.silence.as_secs()),
                    )?;
                    connection
                        .attachment_reply_sent(ticket, Attachment::Accepted, self.opened.elapsed())
                        .map_err(protocol_error)?;
                    self.place.take();
                    return Ok(LocalParticipant {
                        identity,
                        attached: Some(Attached {
                            link: self,
                            connection,
                            place,
                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            count,
                        }),
                    });
                }
            }
        }
        Err(closed())
    }

    fn attachment_request(&mut self, connection: &mut Connection) -> Result<Request, Error> {
        loop {
            let mut plaintext = self.read_segment()?;
            match connection
                .receive(&mut plaintext, self.opened.elapsed())
                .map_err(protocol_error)?
            {
                Input::Partial => {}
                Input::Request(request) if plaintext.is_empty() => return Ok(request),
                _ => return Err(closed()),
            }
        }
    }

    fn write_attachment(&mut self, rid: u64, response: AttachResponse<'_>) -> Result<(), Error> {
        let payload = attach_reply(rid, response).map_err(|_| closed())?;
        self.protect(move |wire, state| {
            Segments::write_protected(wire, &mut state.encrypted, &payload)
        })
    }
}

fn protocol_error(_: ConnectionError) -> Error {
    closed()
}

fn closed() -> Error {
    Error::refused("participant link refused")
}
