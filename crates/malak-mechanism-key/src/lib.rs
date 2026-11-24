//! Key enrols participants with a static key before they connect.

use malak_layers::{
    Action, ChannelName, Enrolment, Error, Grade, Handover, Identity, Mechanism, ParticipantId,
    Proves, Session, Step,
};
use malak_proto::KeyPair;

/// Proves possession of an enrolled key through the board's handshake.
///
/// The core checks the registered key and proof of possession at attach.
/// This mechanism supplies the key at enrolment and needs no challenge
/// rounds, retained state, expiry or call veto.
#[derive(Clone, Copy, Debug, Default)]
pub struct Key;

impl Mechanism for Key {
    fn describe(&self) -> Proves {
        Proves {
            highest_grade: Grade::Proven,
            has_veto: false,
            key_at_enrolment: true,
        }
    }

    fn enrol(&self, _: &ParticipantId, _: &Enrolment<'_>) -> Result<Handover, Error> {
        let pair =
            KeyPair::generate().map_err(|_| Error::unavailable("could not generate a Key pair"))?;
        Ok(Handover {
            private_key: Some(*pair.private()),
            public_key: Some(*pair.public()),
            contents: Vec::new(),
            record: None,
        })
    }

    fn step(&self, _: &Session<'_>, _: &[u8]) -> Result<Step, Error> {
        Ok(Step::Proven {
            ends: None,
            register_key: false,
            record: None,
        })
    }

    fn allow(&self, _: &Identity<'_>, _: Action, _: &ChannelName) -> Result<bool, Error> {
        Ok(true)
    }

    fn revoke(&self, _: &ParticipantId) -> Result<(), Error> {
        Ok(())
    }
}
