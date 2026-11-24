use crate::{Action, ChannelName, Error, Grade, ParticipantId, Source};
use std::time::SystemTime;
use zeroize::Zeroize;

/// What a mechanism can establish or veto.
#[derive(Clone, Copy, Debug)]
pub struct Proves {
    /// The maximum grade the mechanism can declare.
    pub highest_grade: Grade,
    /// Whether the core should ask for the optional call veto.
    pub has_veto: bool,
    /// Whether every enrolment registers a public key.
    pub key_at_enrolment: bool,
}

/// Information available to a mechanism preparing handover contents.
#[derive(Clone, Debug)]
pub struct Enrolment<'a> {
    /// The allowed source if the owner restricts where the key can be used.
    pub allowed_place: Option<&'a Source>,
    /// The intended local account, where supplied.
    pub account: Option<u32>,
}

/// Credentials and optional state returned by enrolment.
pub struct Handover {
    /// The private half of the caller's handshake key, when made at enrolment.
    pub private_key: Option<[u8; 32]>,
    /// The matching public key to register.
    pub public_key: Option<[u8; 32]>,
    /// Mechanism-specific bytes for the caller's handover file.
    pub contents: Vec<u8>,
    /// An optional board-held mechanism record of at most 4 KiB.
    pub record: Option<Vec<u8>>,
}

impl Drop for Handover {
    fn drop(&mut self) {
        self.private_key.zeroize();
        self.contents.zeroize();
        self.record.zeroize();
    }
}

/// Which side is driving the proof exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// The board side.
    Board,
    /// The participant side.
    Participant,
}

/// Facts from the core for one step of the mechanism exchange.
pub struct Session<'a> {
    /// The side making the call.
    pub side: Side,
    /// The public key used by the participant during the handshake.
    pub public_key: &'a [u8; 32],
    /// The final handshake hash, unique to this link.
    pub binding: &'a [u8; 32],
    /// The source reported by the transport, if known.
    pub source: Option<&'a Source>,
    /// The participant claimed by the caller.
    pub claimed: &'a ParticipantId,
    /// Mechanism bytes the board saved for the claimed participant.
    pub record: Option<&'a [u8]>,
    /// Mechanism bytes from the handover on the participant side.
    pub handover: Option<&'a [u8]>,
}

/// The mechanism's answer to one exchange step.
pub enum Step {
    /// Send these bytes to the other side and wait for another step.
    More(Vec<u8>),
    /// Accept the claimed identity, subject to the core's checks.
    Proven {
        /// The latest time this proof is valid, if limited.
        ends: Option<SystemTime>,
        /// Ask the board to register the negotiated public key.
        register_key: bool,
        /// Ask the board to save new mechanism state.
        record: Option<Vec<u8>>,
    },
    /// Refuse the claimed identity.
    Denied,
}

/// A validated caller's identity as seen by a mechanism veto.
#[derive(Clone, Debug)]
pub struct Identity<'a> {
    /// Core-selected participant identifier.
    pub participant: &'a ParticipantId,
    /// Core-selected identity grade.
    pub grade: Grade,
    /// The earliest of the grant and mechanism end times, where set.
    pub ends: Option<SystemTime>,
}

/// The pluggable proof of identity and optional additional veto.
///
/// The core alone decides the participant, its grade and its grant.
pub trait Mechanism: Send + Sync {
    /// State what the mechanism can prove before it is used.
    fn describe(&self) -> Proves;

    /// Prepare credentials and state for enrolment or key replacement.
    fn enrol(&self, who: &ParticipantId, hints: &Enrolment<'_>) -> Result<Handover, Error>;

    /// Run one proof exchange step for the claimed participant.
    fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, Error>;

    /// Veto a call the caller's grant would otherwise allow.
    fn allow(
        &self,
        who: &Identity<'_>,
        action: Action,
        channel: &ChannelName,
    ) -> Result<bool, Error>;

    /// Learn that the board already saved the participant's revocation.
    fn revoke(&self, who: &ParticipantId) -> Result<(), Error>;
}
