use malak_layers::{ClaimedSession, ParticipantId, SenderTrace};
use std::collections::VecDeque;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(any(target_os = "linux", target_os = "macos"))]
const HISTORY_LIMIT: usize = 64;

/// The current state of a session-bound handover.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimState {
    /// The session can use its handover.
    Bound,
    /// The issued session exited.
    Ended,
    /// An authorized caller released the claim.
    Released,
}

impl ClaimState {
    /// The stable status label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Ended => "ended",
            Self::Released => "released",
        }
    }
}

/// One of the newest changes to a participant's claim.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ClaimEvent {
    /// The board issued a handover to a session.
    Issued {
        /// Time on the board host.
        at_ms: u64,
        /// The selected session.
        session: ClaimedSession,
        /// The participant that receives the claim notices.
        issuer: ParticipantId,
    },
    /// A different session presented the handover.
    Refused {
        /// Time on the board host.
        at_ms: u64,
        /// The direct peer as observed by the board.
        peer: SenderTrace,
    },
    /// The issued session exited.
    Ended {
        /// Time on the board host.
        at_ms: u64,
    },
    /// An authorized caller released the claim.
    Released {
        /// Time on the board host.
        at_ms: u64,
    },
}

impl ClaimEvent {
    /// Time on the board host in Unix epoch milliseconds.
    pub const fn at_ms(&self) -> u64 {
        match self {
            Self::Issued { at_ms, .. }
            | Self::Refused { at_ms, .. }
            | Self::Ended { at_ms }
            | Self::Released { at_ms } => *at_ms,
        }
    }
}

/// The current claim, along with its most recent history.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Claim {
    /// State of the last-issued handover.
    pub state: ClaimState,
    /// Session the board last issued the handover to.
    pub session: ClaimedSession,
    /// Recipient of the claim notices.
    pub issuer: ParticipantId,
    /// Total refused attempts for the current handover.
    pub refused: u64,
    /// Time of the last refused attempt.
    pub last_refused_at_ms: Option<u64>,
    /// The newest 64 events, oldest first.
    pub history: VecDeque<ClaimEvent>,
    pub(crate) accepted: bool,
    pub(crate) accepted_notice: bool,
    pub(crate) ended_notice: bool,
    pub(crate) issued_at_ms: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
pub(crate) enum ClaimChange {
    Event(ClaimEvent),
    Accepted,
    AcceptedNotice,
    EndedNotice,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Claim {
    pub(super) fn apply(claim: &mut Option<Arc<Self>>, change: ClaimChange) -> Option<()> {
        if let ClaimChange::Event(ClaimEvent::Issued {
            at_ms,
            session,
            issuer,
        }) = change
        {
            let history = claim
                .take()
                .map_or_else(VecDeque::new, |claim| Arc::unwrap_or_clone(claim).history);
            let mut issued = Self {
                state: ClaimState::Bound,
                session,
                issuer,
                refused: 0,
                last_refused_at_ms: None,
                history,
                accepted: false,
                accepted_notice: false,
                ended_notice: false,
                issued_at_ms: at_ms,
            };
            issued.remember(ClaimEvent::Issued {
                at_ms,
                session: issued.session.clone(),
                issuer: issued.issuer.clone(),
            });
            *claim = Some(Arc::new(issued));
            return Some(());
        }
        let claim = Arc::make_mut(claim.as_mut()?);
        match change {
            ClaimChange::Accepted if claim.state == ClaimState::Bound && !claim.accepted => {
                claim.accepted = true;
            }
            ClaimChange::AcceptedNotice if claim.accepted && !claim.accepted_notice => {
                claim.accepted_notice = true;
            }
            ClaimChange::EndedNotice if claim.state == ClaimState::Ended && !claim.ended_notice => {
                claim.ended_notice = true;
            }
            ClaimChange::Event(event) => {
                match &event {
                    ClaimEvent::Refused { at_ms, .. } => {
                        claim.refused = claim.refused.saturating_add(1);
                        claim.last_refused_at_ms = Some(*at_ms);
                    }
                    ClaimEvent::Ended { .. } if claim.state == ClaimState::Bound => {
                        claim.state = ClaimState::Ended;
                    }
                    ClaimEvent::Released { .. } if claim.state == ClaimState::Bound => {
                        claim.state = ClaimState::Released;
                    }
                    _ => return None,
                }
                claim.remember(event);
            }
            _ => return None,
        }
        Some(())
    }

    fn remember(&mut self, event: ClaimEvent) {
        if self.history.len() == HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(event);
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use malak_layers::{ProcessIdentity, SessionProcess};

    fn issued() -> Option<Arc<Claim>> {
        let mut claim = None;
        Claim::apply(
            &mut claim,
            ClaimChange::Event(ClaimEvent::Issued {
                at_ms: 1,
                session: ClaimedSession {
                    process: SessionProcess {
                        identity: ProcessIdentity {
                            pid: 20,
                            start_time: 30,
                        },
                        program: "codex".into(),
                    },
                    folder: None,
                },
                issuer: ParticipantId::lead(),
            }),
        )
        .unwrap();
        claim
    }

    #[test]
    fn history_keeps_the_newest_64_events() {
        let mut claim = issued();
        for at_ms in 2..=100 {
            Claim::apply(
                &mut claim,
                ClaimChange::Event(ClaimEvent::Refused {
                    at_ms,
                    peer: SenderTrace::transport("unix"),
                }),
            )
            .unwrap();
        }
        let claim = claim.unwrap();
        assert_eq!(claim.refused, 99);
        assert_eq!(claim.history.len(), 64);
        assert_eq!(claim.history.front().unwrap().at_ms(), 37);
        assert_eq!(claim.history.back().unwrap().at_ms(), 100);
    }

    #[test]
    fn an_equal_pid_with_a_different_start_time_is_refused() {
        let id = ParticipantId::lead();
        let mut table = super::super::Authority::new(id.clone(), None);
        let session = issued().unwrap().session.clone();
        table.issue_session(&id, session.clone(), 1).unwrap();
        let mut peer = malak_layers::SenderPeer::transport("unix");
        let mut replacement = session.process;
        replacement.identity.start_time += 1;
        peer.ancestry.push(replacement.identity);
        peer.trace.session = Some(replacement);

        assert!(table.check_session(&id, Some(&peer)).is_err());
    }
}
