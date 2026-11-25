use malak_layers::ClaimedSession;
use minicbor::Encoder;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use {
    malak_layers::{ProcessIdentity, SessionProcess},
    minicbor::{Decoder, data::Type},
};

pub(super) fn write(output: &mut Encoder<Vec<u8>>, session: &ClaimedSession) {
    output.array(4).unwrap();
    output.u32(session.process.identity.pid).unwrap();
    output.u64(session.process.identity.start_time).unwrap();
    output.str(&session.process.program).unwrap();
    if let Some(folder) = &session.folder {
        output.str(folder).unwrap();
    } else {
        output.null().unwrap();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn read(input: &mut Decoder<'_>) -> Option<ClaimedSession> {
    if input.array().ok()?? != 4 {
        return None;
    }
    let pid = input.u32().ok()?;
    let start_time = input.u64().ok()?;
    let program = input.str().ok()?.into();
    let folder = if input.datatype().ok()? == Type::Null {
        input.null().ok()?;
        None
    } else {
        Some(input.str().ok()?.into())
    };
    Some(ClaimedSession {
        process: SessionProcess {
            identity: ProcessIdentity { pid, start_time },
            program,
        },
        folder,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn write_change(output: &mut Encoder<Vec<u8>>, change: &crate::authority::ClaimChange) {
    use crate::{ClaimEvent, authority::ClaimChange};
    match change {
        ClaimChange::Event(ClaimEvent::Issued {
            at_ms,
            session,
            issuer,
        }) => {
            output.array(4).unwrap().u8(0).unwrap().u64(*at_ms).unwrap();
            write(output, session);
            output.str(issuer.as_str()).unwrap();
        }
        ClaimChange::Event(ClaimEvent::Refused { at_ms, peer }) => {
            output.array(3).unwrap().u8(1).unwrap().u64(*at_ms).unwrap();
            crate::sender::write(output, peer, None, None);
        }
        ClaimChange::Event(ClaimEvent::Ended { at_ms }) => {
            output.array(2).unwrap().u8(2).unwrap().u64(*at_ms).unwrap();
        }
        ClaimChange::Event(ClaimEvent::Released { at_ms }) => {
            output.array(2).unwrap().u8(3).unwrap().u64(*at_ms).unwrap();
        }
        ClaimChange::Accepted => {
            output.array(1).unwrap().u8(4).unwrap();
        }
        ClaimChange::AcceptedNotice => {
            output.array(1).unwrap().u8(5).unwrap();
        }
        ClaimChange::EndedNotice => {
            output.array(1).unwrap().u8(6).unwrap();
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn read_change(input: &mut Decoder<'_>) -> Option<crate::authority::ClaimChange> {
    use crate::{ClaimEvent, authority::ClaimChange};
    let length = input.array().ok()??;
    let tag = input.u8().ok()?;
    Some(match (tag, length) {
        (0, 4) => ClaimChange::Event(ClaimEvent::Issued {
            at_ms: input.u64().ok()?,
            session: read(input)?,
            issuer: input.str().ok()?.parse().ok()?,
        }),
        (1, 3) => ClaimChange::Event(ClaimEvent::Refused {
            at_ms: input.u64().ok()?,
            peer: crate::sender::read(input).ok()?.0,
        }),
        (2, 2) => ClaimChange::Event(ClaimEvent::Ended {
            at_ms: input.u64().ok()?,
        }),
        (3, 2) => ClaimChange::Event(ClaimEvent::Released {
            at_ms: input.u64().ok()?,
        }),
        (4, 1) => ClaimChange::Accepted,
        (5, 1) => ClaimChange::AcceptedNotice,
        (6, 1) => ClaimChange::EndedNotice,
        _ => return None,
    })
}
