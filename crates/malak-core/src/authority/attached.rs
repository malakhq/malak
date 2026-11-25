use super::{Authority, LocalKeys};
use crate::journal_entry::Entry;
use crate::{Error, Grade, ParticipantId};
use malak_layers::Source;
use malak_proto::Entrance;
use std::sync::Arc;
use std::time::SystemTime;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn fingerprint(key: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let digest: [u8; 32] = Sha256::digest(key).into();
    let [left, right] = digest.as_chunks::<16>().0 else {
        unreachable!("SHA-256 length")
    };
    format!(
        "{:032x}{:032x}",
        u128::from_be_bytes(*left),
        u128::from_be_bytes(*right)
    )
}

pub(crate) const MAX_RECORD: usize = 4096;

pub(super) struct Socket {
    pub(super) key: Option<[u8; 32]>,
    place: Option<Source>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    account: Option<u32>,
    may_receive_key: bool,
    record: Option<Arc<[u8]>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    rekey: Arc<parking_lot::Mutex<()>>,
}

pub(crate) struct Attempt {
    pub(crate) id: ParticipantId,
    pub(crate) key: [u8; 32],
    pub(crate) record: Option<Arc<[u8]>>,
    generation: u64,
}

pub(crate) struct Proof {
    pub(crate) highest_grade: Grade,
    pub(crate) ends: Option<SystemTime>,
    pub(crate) register_key: bool,
    pub(crate) record: Option<Vec<u8>>,
}

pub(crate) struct Verified {
    pub(crate) id: ParticipantId,
    pub(crate) grade: Grade,
    generation: u64,
    ends: Option<SystemTime>,
}

impl Authority {
    pub(crate) fn set_local_keys(
        &mut self,
        board: [u8; 32],
        owner: [u8; 32],
        lead: [u8; 32],
        lead_user: u32,
    ) -> Result<(), Error> {
        if self.local_keys.is_some() {
            return Err(Error::refused("local entrances are already configured"));
        }
        if board == owner || owner == lead || lead == board {
            return Err(Error::refused("local entrance keys must be distinct"));
        }
        self.local_keys = Some(LocalKeys { board, owner });
        let id = ParticipantId::lead();
        self.register_socket(
            &id,
            Some(lead),
            Some(Source::LocalUser(lead_user)),
            Some(lead_user),
            None,
        );
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn require_new_key(&self, key: &[u8; 32]) -> Result<(), Error> {
        let reserved = self
            .local_keys
            .as_ref()
            .is_some_and(|roles| key == &roles.board || key == &roles.owner);
        if reserved || self.retired_keys.contains(key) || self.keys.contains_key(key) {
            return Err(Error::refused("enrolment key is already on record"));
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn retire_key(&mut self, key: [u8; 32]) -> Result<(), Error> {
        self.require_new_key(&key)?;
        self.retired_keys.insert(key);
        Ok(())
    }

    pub(crate) fn register_socket(
        &mut self,
        id: &ParticipantId,
        key: Option<[u8; 32]>,
        place: Option<Source>,
        account: Option<u32>,
        record: Option<Vec<u8>>,
    ) {
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = account;
        self.participants
            .get_mut(id)
            .expect("participant exists")
            .socket = Some(Socket {
            key,
            place,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            account,
            may_receive_key: key.is_none(),
            record: record.map(Into::into),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            rekey: Arc::default(),
        });
        if let Some(key) = key {
            self.keys.insert(key, id.clone());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restore_socket_record(
        &mut self,
        id: &ParticipantId,
        generation: u64,
        record: Vec<u8>,
    ) -> Result<(), Error> {
        let participant = self
            .participants
            .get_mut(id)
            .filter(|participant| !participant.revoked && participant.generation == generation)
            .ok_or_else(refused)?;
        let socket = participant.socket.as_mut().ok_or_else(refused)?;
        if record.len() > MAX_RECORD {
            return Err(refused());
        }
        socket.record = Some(record.into());
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn rekey_gate(
        &self,
        id: &ParticipantId,
    ) -> Result<Arc<parking_lot::Mutex<()>>, Error> {
        Ok(self.keyed_socket(id)?.rekey.clone())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn rekey_target(
        &self,
        id: &ParticipantId,
        expected: &str,
    ) -> Result<(Option<u32>, Option<Source>), Error> {
        let socket = self.keyed_socket(id)?;
        if !socket.key.is_some_and(|key| fingerprint(&key) == expected) {
            return Err(Error::refused("the current key fingerprint does not match"));
        }
        Ok((socket.account, socket.place.clone()))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn keyed_socket(&self, id: &ParticipantId) -> Result<&Socket, Error> {
        self.active(id)?;
        self.participants[id]
            .socket
            .as_ref()
            .filter(|socket| socket.key.is_some())
            .ok_or_else(|| Error::refused("participant has no current key"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn replace_key(
        &mut self,
        id: &ParticipantId,
        expected: &str,
        key: [u8; 32],
        record: Option<Vec<u8>>,
        session: Option<malak_layers::ClaimedSession>,
        save: impl FnOnce(Entry<'_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.rekey_target(id, expected)?;
        self.require_new_key(&key)?;
        let generation = self.participants[id].next_generation()?;
        let issued_at_ms = crate::epoch_millis(SystemTime::now());
        save(Entry::Rekey {
            participant: id,
            generation,
            key,
            record: record.as_deref(),
            session: session.as_ref(),
            issued_at_ms,
        })?;
        self.install_key(id, generation, key, record)?;
        if let Some(session) = session {
            self.issue_session(id, session, issued_at_ms)?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restore_key(
        &mut self,
        id: &ParticipantId,
        generation: u64,
        key: [u8; 32],
        record: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        self.keyed_socket(id)?;
        self.require_new_key(&key)?;
        if self.participants[id].next_generation()? != generation
            || record
                .as_ref()
                .is_some_and(|record| record.len() > MAX_RECORD)
        {
            return Err(refused());
        }
        self.install_key(id, generation, key, record)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn install_key(
        &mut self,
        id: &ParticipantId,
        generation: u64,
        key: [u8; 32],
        record: Option<Vec<u8>>,
    ) -> Result<(), Error> {
        let participant = self.participants.get_mut(id).ok_or_else(refused)?;
        let socket = participant.socket.as_mut().ok_or_else(refused)?;
        let old = socket.key.replace(key).ok_or_else(refused)?;
        socket.record = record.map(Into::into);
        participant.generation = generation;
        self.keys.remove(&old);
        self.retired_keys.insert(old);
        self.keys.insert(key, id.clone());
        Ok(())
    }

    pub(crate) fn begin_attachment(
        &self,
        entrance: Entrance,
        id: ParticipantId,
        key: [u8; 32],
        source: Option<&Source>,
    ) -> Result<Attempt, Error> {
        if !matches!(
            (entrance, id.is_lead()),
            (Entrance::Lead, true) | (Entrance::General, false)
        ) {
            return Err(refused());
        }
        self.active(&id)?;
        let participant = &self.participants[&id];
        let socket = participant.socket.as_ref().ok_or_else(refused)?;
        self.check_key(&id, socket, &key)?;
        if let Some(place) = &socket.place
            && source != Some(place)
        {
            return Err(refused());
        }
        Ok(Attempt {
            id,
            key,
            record: socket.record.clone(),
            generation: participant.generation,
        })
    }

    pub(crate) fn finish_attachment(
        &mut self,
        attempt: Attempt,
        proof: Proof,
        save: impl FnOnce(Entry<'_>) -> Result<(), Error>,
    ) -> Result<Verified, Error> {
        let now = self.wall_time();
        self.check_generation(&attempt.id, attempt.generation, proof.ends, now)?;
        let socket = self.participants[&attempt.id]
            .socket
            .as_ref()
            .ok_or_else(refused)?;
        self.check_key(&attempt.id, socket, &attempt.key)?;
        if proof
            .record
            .as_ref()
            .is_some_and(|record| record.len() > MAX_RECORD)
            || (proof.register_key && !socket.may_receive_key)
        {
            return Err(refused());
        }

        let participant = &self.participants[&attempt.id];
        let generation = if proof.register_key {
            participant.next_generation()?
        } else {
            participant.generation
        };
        let updated_record = proof
            .record
            .filter(|record| Some(record.as_slice()) != socket.record.as_deref());
        let key = proof.register_key.then_some(attempt.key);
        if key.is_some() || updated_record.is_some() {
            save(Entry::SocketUpdate {
                participant: &attempt.id,
                generation,
                key,
                record: updated_record.as_deref(),
            })?;
        }
        let participant = self
            .participants
            .get_mut(&attempt.id)
            .expect("participant still exists");
        participant.generation = generation;
        let socket = participant.socket.as_mut().expect("checked socket record");
        if proof.register_key {
            socket.key = Some(attempt.key);
            socket.may_receive_key = false;
            self.keys.insert(attempt.key, attempt.id.clone());
        }
        if let Some(record) = updated_record {
            socket.record = Some(record.into());
        }

        let grade = if proof.highest_grade == Grade::Proven && socket.key == Some(attempt.key) {
            Grade::Proven
        } else {
            Grade::LinkOnly
        };
        Ok(Verified {
            id: attempt.id,
            grade,
            generation: participant.generation,
            ends: proof.ends,
        })
    }

    pub(crate) fn validate_attached(&mut self, verified: &Verified) -> Result<(), Error> {
        let now = self.wall_time();
        self.check_generation(&verified.id, verified.generation, verified.ends, now)
    }

    pub(crate) fn verified_end(&self, verified: &Verified) -> Option<SystemTime> {
        let grant_end = self
            .participants
            .get(&verified.id)
            .and_then(|participant| participant.grant.ends());
        match (grant_end, verified.ends) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (ends, None) | (None, ends) => ends,
        }
    }

    fn check_key(&self, id: &ParticipantId, socket: &Socket, key: &[u8; 32]) -> Result<(), Error> {
        let reserved = self
            .local_keys
            .as_ref()
            .is_some_and(|keys| &keys.board == key || &keys.owner == key);
        if reserved || self.retired_keys.contains(key) {
            return Err(refused());
        }
        let owner = self.keys.get(key);
        let fits = match socket.key {
            Some(expected) => expected == *key && owner == Some(id),
            None => socket.may_receive_key && owner.is_none(),
        };
        fits.then_some(()).ok_or_else(refused)
    }

    fn check_generation(
        &self,
        id: &ParticipantId,
        generation: u64,
        ends: Option<SystemTime>,
        now: SystemTime,
    ) -> Result<(), Error> {
        self.participants
            .get(id)
            .filter(|participant| {
                !participant.revoked
                    && participant.generation == generation
                    && !participant.grant.expired_at(now)
                    && ends.is_none_or(|end| now < end)
            })
            .map(|_| ())
            .ok_or_else(refused)
    }
}

fn refused() -> Error {
    Error::refused("attachment authority refused")
}

#[cfg(test)]
impl Authority {
    pub(crate) fn provision_fixture(
        &mut self,
        id: &ParticipantId,
        key: Option<[u8; 32]>,
        place: Option<Source>,
    ) {
        self.register_socket(id, key, place, None, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Grant;
    use std::time::UNIX_EPOCH;

    const KEY: [u8; 32] = [4; 32];
    const STRANGER: [u8; 32] = [5; 32];
    const LEAD_KEY: [u8; 32] = [3; 32];

    struct Fixture {
        authority: Authority,
        participant: ParticipantId,
    }

    impl Fixture {
        fn new(key: Option<[u8; 32]>) -> Self {
            let mut authority = Authority::new(ParticipantId::lead(), None);
            authority
                .set_local_keys([1; 32], [2; 32], LEAD_KEY, 1)
                .unwrap();
            let participant: ParticipantId = "member01".parse().unwrap();
            authority.insert(
                participant.clone(),
                "member".parse().unwrap(),
                Grant::default(),
                None,
            );
            authority.provision_fixture(&participant, key, Some(Source::LocalUser(9)));
            Self {
                authority,
                participant,
            }
        }

        fn proof(register_key: bool) -> Proof {
            Proof {
                highest_grade: Grade::Proven,
                ends: None,
                register_key,
                record: None,
            }
        }

        fn attempt(&self, key: [u8; 32]) -> Result<Attempt, Error> {
            self.authority.begin_attachment(
                Entrance::General,
                self.participant.clone(),
                key,
                Some(&Source::LocalUser(9)),
            )
        }
    }

    #[test]
    fn a_registered_key_admits_only_its_own_participant() {
        let sample = Fixture::new(Some(KEY));

        assert!(
            sample
                .authority
                .check_opening(Entrance::General, &KEY, true)
                .is_ok()
        );
        assert!(sample.attempt(STRANGER).is_err());
    }

    #[test]
    fn general_cannot_attach_as_lead_even_with_the_lead_key() {
        let sample = Fixture::new(None);

        assert!(
            sample
                .authority
                .begin_attachment(
                    Entrance::General,
                    ParticipantId::lead(),
                    LEAD_KEY,
                    Some(&Source::LocalUser(1)),
                )
                .is_err()
        );
    }

    #[test]
    fn a_key_on_record_cannot_be_registered_to_a_new_participant() {
        let sample = Fixture::new(None);

        assert!(sample.attempt(LEAD_KEY).is_err());
    }

    #[test]
    fn allowed_place_requires_a_matching_source() {
        let sample = Fixture::new(Some(KEY));

        assert!(
            sample
                .authority
                .begin_attachment(Entrance::General, sample.participant, KEY, None)
                .is_err()
        );
    }

    #[test]
    fn simultaneous_attempts_can_register_only_one_key() {
        let mut sample = Fixture::new(None);
        let first = sample.attempt(KEY).unwrap();
        let second = sample.attempt(STRANGER).unwrap();

        assert!(
            sample
                .authority
                .finish_attachment(first, Fixture::proof(true), |_| Ok(()))
                .is_ok()
        );
        assert!(
            sample
                .authority
                .finish_attachment(second, Fixture::proof(true), |_| Ok(()))
                .is_err()
        );
    }

    #[test]
    fn a_failed_mechanism_save_leaves_the_participant_unbound() {
        let mut sample = Fixture::new(None);
        let attempt = sample.attempt(KEY).unwrap();
        let mut proof = Fixture::proof(true);
        proof.record = Some(b"new credential".to_vec());

        assert!(
            sample
                .authority
                .finish_attachment(attempt, proof, |_| Err(Error::unavailable("save failed")))
                .is_err()
        );
        let next = sample.attempt(STRANGER).unwrap();
        assert!(next.record.is_none());
        assert!(
            sample
                .authority
                .finish_attachment(next, Fixture::proof(true), |_| Ok(()))
                .is_ok()
        );
    }

    #[test]
    fn a_mechanism_cannot_claim_proven_without_a_registered_key() {
        let mut sample = Fixture::new(None);
        let attempt = sample.attempt(KEY).unwrap();

        let verified = sample
            .authority
            .finish_attachment(attempt, Fixture::proof(false), |_| Ok(()))
            .unwrap();

        assert_eq!(verified.grade, Grade::LinkOnly);
    }

    #[test]
    fn a_mechanism_cannot_extend_an_expired_grant() {
        let mut sample = Fixture::new(Some(KEY));
        let attempt = sample.attempt(KEY).unwrap();
        let mut grant = Grant::default();
        grant.set_end(Some(UNIX_EPOCH));
        sample
            .authority
            .replace_grant(&sample.participant, grant, 0, |_, _| Ok(()))
            .unwrap();

        assert!(
            sample
                .authority
                .finish_attachment(attempt, Fixture::proof(false), |_| Ok(()))
                .is_err()
        );
    }

    #[test]
    fn an_in_flight_attempt_is_refused_after_revocation() {
        let mut sample = Fixture::new(Some(KEY));
        let attempt = sample.attempt(KEY).unwrap();
        sample
            .authority
            .revoke(&sample.participant, None, || Ok(()))
            .unwrap();

        assert!(
            sample
                .authority
                .finish_attachment(attempt, Fixture::proof(false), |_| Ok(()))
                .is_err()
        );
    }

    #[test]
    fn a_retired_key_is_silently_refused_at_the_opening() {
        let mut sample = Fixture::new(Some(KEY));
        sample
            .authority
            .revoke(&sample.participant, None, || Ok(()))
            .unwrap();

        assert!(
            sample
                .authority
                .check_opening(Entrance::General, &KEY, false)
                .is_err()
        );
    }
}
