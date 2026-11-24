use super::{LocalEntrances, owner_fields};
use crate::{BoardStatus, Error, grant::write_grant};
use bytes::Bytes;
use malak_layers::Durability;
use malak_proto::{Frame, encode_frame};
use minicbor::Encoder;

impl LocalEntrances {
    pub(super) fn operation_status(&self, request: &Frame) -> Result<Bytes, Error> {
        let fields = owner_fields(request)?;
        let id = fields.text("id")?.map(str::parse).transpose()?;
        let name = fields.text("channel")?.map(str::parse).transpose()?;
        if id.is_some() && name.is_some() {
            return Err(Error::refused("status takes at most one target"));
        }
        let snapshot = self.board.status(id.as_ref(), name.as_ref());
        let mechanism = self
            .settings
            .provisioning
            .as_ref()
            .map(|provision| provision.mechanism_name.as_str());
        let body = encode_status(&snapshot, mechanism);
        encode_frame(
            request.rid(),
            "OK",
            1,
            |output| {
                output
                    .str("run")
                    .unwrap()
                    .str(&snapshot.run.to_string())
                    .unwrap();
            },
            &body,
        )
        .map_err(|_| Error::unavailable("status reply does not fit"))
    }
}

fn encode_status(status: &BoardStatus, mechanism: Option<&str>) -> Vec<u8> {
    let mut output = Encoder::new(Vec::new());
    output
        .map(5 + u64::from(status.lead_session.is_some()))
        .unwrap();
    output
        .str("started_at_ms")
        .unwrap()
        .u64(status.started_at_ms)
        .unwrap();
    output.str("layers").unwrap().map(3).unwrap();
    output.str("entrance").unwrap().str("local").unwrap();
    output.str("mechanism").unwrap();
    text_or_null(&mut output, mechanism);
    let durability = match status.store.durability {
        Durability::Memory => "memory",
        Durability::Restart => "restart",
        Durability::Synced => "synced",
    };
    output
        .str("store_durability")
        .unwrap()
        .str(durability)
        .unwrap();
    output.str("repair").unwrap();
    text_or_null(&mut output, status.pending_repair.as_deref());
    if let Some(session) = &status.lead_session {
        output.str("lead_session").unwrap();
        session.write(&mut output);
    }
    output
        .str("participants")
        .unwrap()
        .array(status.participants.len() as u64)
        .unwrap();
    for participant in &status.participants {
        output
            .map(9 + u64::from(participant.claim.is_some()))
            .unwrap();
        output
            .str("id")
            .unwrap()
            .str(participant.id.as_str())
            .unwrap();
        output
            .str("name")
            .unwrap()
            .str(participant.name.as_str())
            .unwrap();
        output
            .str("revoked")
            .unwrap()
            .bool(participant.revoked)
            .unwrap();
        output.str("fingerprint").unwrap();
        text_or_null(&mut output, participant.fingerprint.as_deref());
        output
            .str("connections")
            .unwrap()
            .u64(participant.connections as u64)
            .unwrap();
        for (field, time) in [
            ("enrolled_at_ms", participant.enrolled_at_ms),
            ("grant_changed_at_ms", participant.grant_changed_at_ms),
            ("revoked_at_ms", participant.revoked_at_ms),
        ] {
            output.str(field).unwrap();
            if let Some(time) = time {
                output.u64(time).unwrap();
            } else {
                output.null().unwrap();
            }
        }
        output.str("grant").unwrap();
        write_grant(&mut output, &participant.grant);
        if let Some(claim) = &participant.claim {
            output.str("claim").unwrap();
            claim_status(&mut output, claim);
        }
    }
    output
        .str("channels")
        .unwrap()
        .array(status.channels.len() as u64)
        .unwrap();
    for channel in &status.channels {
        output.map(6).unwrap();
        output
            .str("name")
            .unwrap()
            .str(channel.name.as_str())
            .unwrap();
        output
            .str("creation")
            .unwrap()
            .str(&channel.creation.to_string())
            .unwrap();
        output.str("floor").unwrap().u64(channel.floor).unwrap();
        output.str("head").unwrap().u64(channel.head).unwrap();
        output
            .str("stopped")
            .unwrap()
            .bool(channel.stopped)
            .unwrap();
        output
            .str("readers")
            .unwrap()
            .map(channel.readers.len() as u64)
            .unwrap();
        for (id, position) in channel.readers.iter() {
            output.str(id.as_str()).unwrap().u64(*position).unwrap();
        }
    }
    output.into_writer()
}

fn text_or_null(output: &mut Encoder<Vec<u8>>, value: Option<&str>) {
    if let Some(value) = value {
        output.str(value).unwrap();
    } else {
        output.null().unwrap();
    }
}

fn claim_status(output: &mut Encoder<Vec<u8>>, claim: &crate::Claim) {
    output.map(6).unwrap();
    output
        .str("state")
        .unwrap()
        .str(claim.state.as_str())
        .unwrap();
    output.str("session").unwrap();
    session_status(output, &claim.session);
    output
        .str("issuer")
        .unwrap()
        .str(claim.issuer.as_str())
        .unwrap();
    output.str("refused").unwrap().u64(claim.refused).unwrap();
    output.str("last_refused_at_ms").unwrap();
    if let Some(at) = claim.last_refused_at_ms {
        output.u64(at).unwrap();
    } else {
        output.null().unwrap();
    }
    output
        .str("history")
        .unwrap()
        .array(claim.history.len() as u64)
        .unwrap();
    for event in &claim.history {
        use crate::ClaimEvent;
        let (kind, details) = match event {
            ClaimEvent::Issued { .. } => ("issued", 2),
            ClaimEvent::Refused { .. } => ("refused", 1),
            ClaimEvent::Ended { .. } => ("ended", 0),
            ClaimEvent::Released { .. } => ("released", 0),
        };
        output.map(2 + details).unwrap();
        output.str("event").unwrap().str(kind).unwrap();
        output.str("at_ms").unwrap().u64(event.at_ms()).unwrap();
        match event {
            ClaimEvent::Issued {
                session, issuer, ..
            } => {
                output.str("session").unwrap();
                session_status(output, session);
                output.str("issuer").unwrap().str(issuer.as_str()).unwrap();
            }
            ClaimEvent::Refused { peer, .. } => {
                output.str("peer").unwrap();
                let mut peer = peer.clone();
                peer.session = None;
                crate::sender::write(output, &peer, None, None);
            }
            ClaimEvent::Ended { .. } | ClaimEvent::Released { .. } => {}
        }
    }
}

fn session_status(output: &mut Encoder<Vec<u8>>, session: &malak_layers::ClaimedSession) {
    output.map(4).unwrap();
    output
        .str("pid")
        .unwrap()
        .u32(session.process.identity.pid)
        .unwrap();
    output
        .str("start_time")
        .unwrap()
        .u64(session.process.identity.start_time)
        .unwrap();
    output
        .str("program")
        .unwrap()
        .str(&session.process.program)
        .unwrap();
    output.str("folder").unwrap();
    text_or_null(output, session.folder.as_deref());
}
