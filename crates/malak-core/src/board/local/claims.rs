use super::{LocalEntrances, LocalOpening};
use crate::authority::ClaimChange;
use crate::board::owner_save_deadline;
use crate::journal_entry::Entry;
use crate::{
    Claim, ClaimEvent, ClaimState, Error, Grade, ParticipantId, ProcessIdentity, SendRequest,
    epoch_millis, record_fields::RecordSender,
};
use malak_layers::{Event, EventKind, Outcome, SenderPeer, Transport};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

impl LocalEntrances {
    /// Expire dead sessions and publish any outstanding claim notices.
    ///
    /// A live local launcher can include the end notice in its own exit report.
    /// The runner supplies how it checks for that launcher.
    pub fn refresh_claims(
        &self,
        launcher_active: impl Fn(&ParticipantId) -> bool,
    ) -> Result<(), Error> {
        let Some(lookup) = self.settings.process_identity else {
            return Ok(());
        };
        for participant in self.board.status(None, None).participants {
            let Some(claim) = participant.claim else {
                continue;
            };
            if participant.revoked {
                continue;
            }
            let id = &participant.id;
            let identity = claim.session.process.identity;
            if claim.state == ClaimState::Bound && lookup(identity.pid) != Some(identity) {
                self.end_session(id, identity)?;
            }
            let current = self.board.table.check().authority.claim(id).cloned();
            if let Some(current) = current {
                if current.accepted && !current.accepted_notice {
                    self.post_claim_notice(id, &participant.name, &current, true)?;
                }
                if current.state == ClaimState::Ended
                    && !current.ended_notice
                    && !launcher_active(id)
                {
                    self.post_claim_notice(id, &participant.name, &current, false)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn release_claim(
        &self,
        id: &ParticipantId,
        peer: &SenderPeer,
        confirmation: Option<&str>,
    ) -> Result<(), Error> {
        let mut table = self.board.table.save_until(owner_save_deadline())?;
        let lead = ParticipantId::lead();
        let current_lead = !id.is_lead()
            && table
                .authority
                .claim(&lead)
                .is_some_and(|claim| claim.state == ClaimState::Bound)
            && table.authority.check_session(&lead, Some(peer)).is_ok();
        if !current_lead && confirmation != Some(id.as_str()) {
            return Err(Error::refused(
                "session release requires interactive owner confirmation",
            ));
        }
        let claim = table
            .authority
            .claim(id)
            .ok_or_else(|| Error::refused("the participant has no active session claim"))?;
        if claim.state != ClaimState::Bound {
            return Err(Error::refused(
                "the participant's session claim is no longer bound",
            ));
        }
        self.record_claim(
            &mut table,
            id,
            ClaimChange::Event(ClaimEvent::Released {
                at_ms: epoch_millis(SystemTime::now()),
            }),
        )?;
        let links = self.board.links.participant_sessions(id);
        drop(table);
        for link in links {
            link.close();
        }
        self.board.changes.notify();
        Ok(())
    }

    pub(super) fn end_session(
        &self,
        id: &ParticipantId,
        identity: ProcessIdentity,
    ) -> Result<(), Error> {
        let mut table = self.board.table.save_until(owner_save_deadline())?;
        if table.authority.claim(id).is_none_or(|claim| {
            claim.state != ClaimState::Bound || claim.session.process.identity != identity
        }) {
            return Ok(());
        }
        self.record_claim(
            &mut table,
            id,
            ClaimChange::Event(ClaimEvent::Ended {
                at_ms: epoch_millis(SystemTime::now()),
            }),
        )?;
        let links = self.board.links.participant_sessions(id);
        drop(table);
        for link in links {
            link.close();
        }
        self.board.changes.notify();
        Ok(())
    }

    pub(super) fn record_claim(
        &self,
        table: &mut crate::board::Table,
        id: &ParticipantId,
        change: ClaimChange,
    ) -> Result<(), Error> {
        self.board.store.save(Entry::Claim(id, &change))?;
        table.authority.restore_claim(id, change)?;
        self.board.publish(table);
        Ok(())
    }

    pub(super) fn report_worker_exit(
        &self,
        id: &ParticipantId,
        status: u8,
        path: &Path,
        trace: Option<&str>,
        peer: &Arc<SenderPeer>,
    ) -> Result<(crate::Receipt, String), Error> {
        if id.is_lead() || !path.is_absolute() {
            return Err(Error::refused("invalid worker exit report"));
        }
        let participant = self
            .board
            .status(Some(id), None)
            .participants
            .into_iter()
            .next()
            .filter(|person| !person.revoked)
            .ok_or_else(|| Error::refused("worker is not active"))?;
        let claim = participant
            .claim
            .ok_or_else(|| Error::refused("worker claim is missing"))?;
        let process = claim.session.process.identity;
        let lookup = self
            .settings
            .process_identity
            .ok_or_else(|| Error::not_supported("local session binding is unavailable"))?;
        if lookup(process.pid) == Some(process) || claim.state == ClaimState::Released {
            return Err(Error::refused("worker session has not ended"));
        }
        self.end_session(id, process)?;
        if claim.accepted && !claim.accepted_notice {
            self.post_claim_notice(id, &participant.name, &claim, true)?;
        }
        let file = std::fs::File::open(path)
            .map_err(|error| Error::refused(format!("cannot open worker result: {error}")))?;
        let mut captured = Vec::new();
        file.take(self.board.limits.body_bytes as u64 + 1)
            .read_to_end(&mut captured)
            .map_err(|error| Error::refused(format!("cannot read worker result: {error}")))?;
        let unread = self.board.unread(id).describe();
        let summary = if captured.is_empty() {
            format!("Worker exited without a result (exit status {status}){unread}.")
        } else {
            format!("Worker exit status: {status}{unread}")
        };
        let mut body = format!(
            "{summary}\n\n{}\n\n",
            crate::claim_notice::ended(&participant.name)
        )
        .into_bytes();
        body.extend(captured);
        let message = notice_id(id, &claim, false);
        let mut request = SendRequest::note("lead", &message, &body);
        request.kind = crate::Kind::Report;
        let headers = trace.map(|trace| [("trace", crate::HeaderValue::Text(trace))]);
        request.headers = headers.as_ref().map_or(&[], |value| value.as_slice());
        let sender = RecordSender {
            id,
            name: &participant.name,
            grade: Grade::Proven,
            peer: Some(peer),
            agent_session_id: None,
        };
        let content = request.validate(&self.board.limits, sender.grade)?;
        let receipt = self.board.persist_send(content, sender, |name| {
            let mut table = self.board.table.check();
            table.authority.valid(id)?;
            table.authority.allow(id, crate::Action::Send, name)?;
            table
                .channels
                .get(name)
                .cloned()
                .ok_or_else(|| Error::unavailable("the issuer's inbox is missing"))
        })?;
        self.mark_notice(id, &claim, false)?;
        Ok((receipt, message))
    }

    fn post_claim_notice(
        &self,
        id: &ParticipantId,
        name: &crate::ParticipantName,
        claim: &Claim,
        accepted: bool,
    ) -> Result<(), Error> {
        let session = &claim.session;
        let body = if accepted {
            format!(
                "{name} is on the board (PID {}, {}, {}).",
                session.process.identity.pid,
                session.process.program,
                session.folder.as_deref().unwrap_or("folder unavailable")
            )
        } else {
            crate::claim_notice::ended(name)
        };
        let channel = claim.issuer.inbox();
        let send_id = notice_id(id, claim, accepted);
        let owner_id = crate::upgrade_notice::SENDER_ID
            .parse()
            .expect("owner notice sender");
        let owner_name = "owner".parse().expect("owner notice name");
        // A launched worker's merged report and a standalone end notice use
        // the same sender and ID so a restart between the two saves cannot duplicate it.
        let (sender_id, sender_name) = if accepted || id.is_lead() {
            (&owner_id, &owner_name)
        } else {
            (id, name)
        };
        let sender = RecordSender {
            id: sender_id,
            name: sender_name,
            grade: Grade::Proven,
            peer: None,
            agent_session_id: None,
        };
        let content = SendRequest::note(channel.as_str(), &send_id, body.as_bytes())
            .validate(&self.board.limits, sender.grade)?;
        let result = self.board.persist_send(content, sender, |name| {
            self.board
                .table
                .check()
                .channels
                .get(name)
                .cloned()
                .ok_or_else(|| Error::unavailable("the issuer's inbox is missing"))
        });
        if accepted
            || result.as_ref().is_err_and(|error| {
                *error != Error::refused("message ID already names different content")
            })
        {
            result?;
        }
        self.mark_notice(id, claim, accepted)
    }

    fn mark_notice(&self, id: &ParticipantId, claim: &Claim, accepted: bool) -> Result<(), Error> {
        let mut table = self.board.table.save_until(owner_save_deadline())?;
        let save = table.authority.claim(id).is_some_and(|current| {
            current.issued_at_ms == claim.issued_at_ms
                && if accepted {
                    !current.accepted_notice
                } else {
                    !current.ended_notice
                }
        });
        if save {
            self.record_claim(
                &mut table,
                id,
                if accepted {
                    ClaimChange::AcceptedNotice
                } else {
                    ClaimChange::EndedNotice
                },
            )?;
        }
        Ok(())
    }
}

impl<T: Transport> LocalOpening<T> {
    pub(super) fn claim_entrances(&self) -> LocalEntrances {
        LocalEntrances {
            board: self.board.clone(),
            settings: self.settings.clone(),
        }
    }

    pub(super) fn refuse_claim(
        &self,
        table: &mut crate::board::Table,
        id: &ParticipantId,
        peer: &SenderPeer,
    ) -> Result<(), Error> {
        let change = ClaimChange::Event(ClaimEvent::Refused {
            at_ms: epoch_millis(SystemTime::now()),
            peer: peer.trace.clone(),
        });
        self.claim_entrances().record_claim(table, id, change)
    }

    pub(super) fn claim_binding_unavailable(&self, id: &ParticipantId, peer: &SenderPeer) {
        let mut event = Event::new(EventKind::Connected);
        event.operation = Some("attach");
        event.participant = Some(id.clone());
        event.reason = Some("local session binding unavailable".into());
        event.sender_trace = Some(peer.trace.clone());
        self.board.events.emit(event);
    }

    pub(super) fn claim_refusal_event(&self, id: &ParticipantId, peer: &SenderPeer) {
        let mut event = Event::new(EventKind::Denied);
        event.operation = Some("attach");
        event.participant = Some(id.clone());
        event.reason = Some("handover used from another session".into());
        event.outcome = Some(Outcome::Refused);
        event.sender_trace = Some(peer.trace.clone());
        self.board.events.emit(event);
    }
}

fn notice_id(id: &ParticipantId, claim: &Claim, accepted: bool) -> String {
    let suffix = if accepted { "accepted" } else { "ended" };
    format!("claim-{id}-{}-{suffix}", claim.issued_at_ms)
}
