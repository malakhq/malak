use crate::{
    Action, ChannelName, Error, Grant, Limits, ParticipantId, ParticipantName, reserved_names,
};
use malak_proto::Entrance;
use parking_lot::{Condvar, Mutex, MutexGuard};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod attached;

const RESERVED_PARTICIPANT_NAMES: &[&str] = &["lead", "stuck"];

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use attached::MAX_RECORD;
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(crate) use attached::fingerprint;
pub(crate) use attached::{Proof, Verified};

pub(crate) struct Authority {
    participants: HashMap<ParticipantId, Participant>,
    names: HashMap<ParticipantName, ParticipantId>,
    local_keys: Option<LocalKeys>,
    keys: HashMap<[u8; 32], ParticipantId>,
    retired_keys: HashSet<[u8; 32]>,
    latest_time: SystemTime,
}

struct LocalKeys {
    board: [u8; 32],
    owner: [u8; 32],
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct ParticipantView {
    pub(crate) times: ParticipantTimes,
    pub(crate) id: ParticipantId,
    pub(crate) name: ParticipantName,
    pub(crate) revoked: bool,
    pub(crate) grant: Grant,
    pub(crate) fingerprint: Option<String>,
    pub(crate) connections: std::sync::Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
pub(crate) struct ParticipantTimes {
    pub(crate) enrolled_at_ms: Option<u64>,
    pub(crate) grant_changed_at_ms: Option<u64>,
    pub(crate) revoked_at_ms: Option<u64>,
}

struct Participant {
    times: ParticipantTimes,
    name: ParticipantName,
    grant: Grant,
    revoked: bool,
    generation: u64,
    socket: Option<attached::Socket>,
    calls: VecDeque<Instant>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    connections: std::sync::Arc<AtomicUsize>,
}

impl Authority {
    pub(crate) fn new(lead: ParticipantId, enrolled_at_ms: Option<u64>) -> Self {
        use Action::{Ack, Create, Get, Read, Send};
        let mut grant = Grant::default();
        grant.allow(
            "**".parse().expect("lead grant pattern"),
            [Send, Read, Get, Ack, Create],
        );
        let mut authority = Self {
            participants: HashMap::new(),
            names: HashMap::new(),
            local_keys: None,
            keys: HashMap::new(),
            retired_keys: HashSet::new(),
            latest_time: UNIX_EPOCH,
        };
        authority.insert(
            lead,
            "lead".parse().expect("reserved lead label"),
            grant,
            enrolled_at_ms,
        );
        authority
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn status(&self) -> Vec<ParticipantView> {
        self.participants
            .iter()
            .map(|(id, participant)| ParticipantView {
                times: participant.times,
                id: id.clone(),
                name: participant.name.clone(),
                revoked: participant.revoked,
                grant: participant.grant.clone(),
                fingerprint: participant
                    .socket
                    .as_ref()
                    .and_then(|socket| socket.key.as_ref())
                    .map(attached::fingerprint),
                connections: participant.connections.clone(),
            })
            .collect()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn count_connection(&self, id: &ParticipantId) -> ConnectionCount {
        let count = self.participants[id].connections.clone();
        count.fetch_add(1, Ordering::AcqRel);
        ConnectionCount(count)
    }

    pub(crate) fn check_opening(
        &self,
        entrance: Entrance,
        key: &[u8; 32],
        registered_key_required: bool,
    ) -> Result<(), Error> {
        let Some(roles) = &self.local_keys else {
            return Err(Error::refused("local entrances are not configured"));
        };
        let accepted = match entrance {
            Entrance::Lead => self.keys.get(key).is_some_and(ParticipantId::is_lead),
            Entrance::Owner => key == &roles.owner,
            Entrance::General => {
                key != &roles.board
                    && key != &roles.owner
                    && !self.keys.get(key).is_some_and(ParticipantId::is_lead)
                    && !self.retired_keys.contains(key)
                    && (!registered_key_required
                        || self.keys.get(key).is_some_and(|id| self.active(id).is_ok()))
            }
        };
        if accepted {
            Ok(())
        } else {
            Err(Error::refused("key is not admissible at this entrance"))
        }
    }

    pub(crate) fn check_name(&self, name: &ParticipantName) -> Result<(), Error> {
        if self.names.contains_key(name) || name.as_str() == "lead" {
            return Err(Error::refused("participant name is in use"));
        }
        Ok(())
    }

    pub(crate) fn check_enrol_name(&self, name: &ParticipantName) -> Result<(), Error> {
        if RESERVED_PARTICIPANT_NAMES.contains(&name.as_str())
            || reserved_names::is_command(name.as_str())
        {
            return Err(Error::refused(format!(
                "participant name {name} is reserved"
            )));
        }
        self.check_name(name)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn ids(&self) -> HashSet<ParticipantId> {
        self.participants.keys().cloned().collect()
    }

    pub(crate) fn has_id(&self, id: &ParticipantId) -> bool {
        self.participants.contains_key(id)
    }

    pub(crate) fn insert(
        &mut self,
        id: ParticipantId,
        name: ParticipantName,
        grant: Grant,
        enrolled_at_ms: Option<u64>,
    ) {
        self.names.insert(name.clone(), id.clone());
        self.participants.insert(
            id,
            Participant {
                times: ParticipantTimes {
                    enrolled_at_ms,
                    grant_changed_at_ms: enrolled_at_ms,
                    revoked_at_ms: None,
                },
                name,
                grant,
                revoked: false,
                generation: 0,
                socket: None,
                calls: VecDeque::new(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                connections: std::sync::Arc::default(),
            },
        );
    }

    pub(crate) fn active(&self, id: &ParticipantId) -> Result<(), Error> {
        self.participants
            .get(id)
            .filter(|participant| !participant.revoked)
            .map(|_| ())
            .ok_or_else(|| Error::refused("participant is not active"))
    }

    pub(crate) fn valid(&mut self, id: &ParticipantId) -> Result<(), Error> {
        self.current(id).map(|_| ())
    }

    fn current(&mut self, id: &ParticipantId) -> Result<&mut Participant, Error> {
        let now = self.wall_time();
        self.participants
            .get_mut(id)
            .filter(|participant| !participant.revoked && !participant.grant.expired_at(now))
            .ok_or_else(|| Error::refused("participant is no longer valid"))
    }

    pub(crate) fn replace_grant(
        &mut self,
        id: &ParticipantId,
        grant: Grant,
        changed_at_ms: u64,
        save: impl FnOnce(&Grant, u64) -> Result<(), Error>,
    ) -> Result<(), Error> {
        self.active(id)?;
        let now = self.wall_time();
        let participant = self.participants.get_mut(id).expect("active participant");
        let generation = if participant.grant.expired_at(now) && !grant.expired_at(now) {
            participant.next_generation()?
        } else {
            participant.generation
        };
        save(&grant, generation)?;
        participant.generation = generation;
        participant.change_grant(grant, Some(changed_at_ms));
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restore_grant(
        &mut self,
        id: &ParticipantId,
        generation: u64,
        grant: Grant,
        changed_at_ms: Option<u64>,
    ) -> Result<(), Error> {
        self.active(id)?;
        let participant = self.participants.get_mut(id).expect("active participant");
        if generation < participant.generation || generation - participant.generation > 1 {
            return Err(Error::unavailable("invalid restored grant generation"));
        }
        participant.change_grant(grant, changed_at_ms);
        participant.generation = generation;
        Ok(())
    }

    pub(crate) fn revoke(
        &mut self,
        id: &ParticipantId,
        revoked_at_ms: Option<u64>,
        save: impl FnOnce() -> Result<(), Error>,
    ) -> Result<bool, Error> {
        if id.is_lead() {
            return Err(Error::refused("the lead cannot be revoked"));
        }
        let participant = self
            .participants
            .get_mut(id)
            .ok_or_else(|| Error::refused("unknown participant"))?;
        if participant.revoked {
            return Ok(false);
        }
        {
            let generation = participant.next_generation()?;
            save()?;
            participant.generation = generation;
            participant.revoked = true;
            participant.times.revoked_at_ms = revoked_at_ms;
            participant.calls.clear();
            self.names.remove(&participant.name);
            if let Some(key) = participant
                .socket
                .as_mut()
                .and_then(|socket| socket.key.take())
            {
                self.keys.remove(&key);
                self.retired_keys.insert(key);
            }
        }
        Ok(true)
    }

    pub(crate) fn start_call(
        &mut self,
        id: &ParticipantId,
        limits: &Limits,
    ) -> Result<ParticipantName, Error> {
        let now = Instant::now();
        let participant = self.current(id)?;
        let window = Duration::from_secs(60);
        while participant
            .calls
            .front()
            .is_some_and(|&call| now.duration_since(call) >= window)
        {
            participant.calls.pop_front();
        }

        let limit = if id.is_lead() {
            limits.lead_calls_per_minute
        } else {
            limits.calls_per_minute
        };
        if participant.calls.len() >= limit {
            return Err(Error::unavailable("participant call rate exceeded").at_limit());
        }
        participant.calls.push_back(now);
        Ok(participant.name.clone())
    }

    pub(crate) fn allow(
        &self,
        id: &ParticipantId,
        action: Action,
        channel: &ChannelName,
    ) -> Result<(), Error> {
        if self
            .participants
            .get(id)
            .is_some_and(|participant| participant.grant.covers(action, channel))
        {
            Ok(())
        } else {
            Err(Error::refused("grant does not allow the call"))
        }
    }

    fn wall_time(&mut self) -> SystemTime {
        self.latest_time = self.latest_time.max(SystemTime::now());
        self.latest_time
    }
}

impl Participant {
    fn change_grant(&mut self, grant: Grant, at_ms: Option<u64>) {
        if self.grant != grant {
            self.times.grant_changed_at_ms = at_ms;
        }
        self.grant = grant;
    }

    fn next_generation(&self) -> Result<u64, Error> {
        self.generation
            .checked_add(1)
            .ok_or_else(|| Error::unavailable("participant generation exhausted"))
    }
}

/// Owner saves register before taking the mutex, so new checks cannot overtake them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct ConnectionCount(std::sync::Arc<AtomicUsize>);

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for ConnectionCount {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct AuthorityLock<T> {
    state: Mutex<T>,
    saving: AtomicUsize,
    wait: Mutex<()>,
    saved: Condvar,
}

impl<T> AuthorityLock<T> {
    pub(crate) fn new(state: T) -> Self {
        Self {
            state: Mutex::new(state),
            saving: AtomicUsize::new(0),
            wait: Mutex::new(()),
            saved: Condvar::new(),
        }
    }

    pub(crate) fn check(&self) -> MutexGuard<'_, T> {
        loop {
            let state = self.state.lock();
            if self.saving.load(Ordering::SeqCst) == 0 {
                return state;
            }
            drop(state);
            let mut wait = self.wait.lock();
            while self.saving.load(Ordering::SeqCst) != 0 {
                self.saved.wait(&mut wait);
            }
        }
    }

    pub(crate) fn save(&self) -> MutexGuard<'_, T> {
        self.saving.fetch_add(1, Ordering::SeqCst);
        let state = self.state.lock();
        self.registered();
        state
    }

    pub(crate) fn save_until(&self, deadline: Instant) -> Result<MutexGuard<'_, T>, Error> {
        self.saving.fetch_add(1, Ordering::SeqCst);
        let state = self.state.try_lock_until(deadline);
        self.registered();
        state
            .filter(|_| Instant::now() < deadline)
            .ok_or_else(|| Error::unavailable("authority save could not obtain the lock in time"))
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn save_waiting(&self) -> bool {
        self.saving.load(Ordering::SeqCst) != 0
    }

    fn registered(&self) {
        let _wait = self.wait.lock();
        if self.saving.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.saved.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_out_save_does_not_leave_checks_waiting() {
        let lock = AuthorityLock::new(false);
        let first = lock.save();
        std::thread::scope(|tasks| {
            let rejected = tasks.spawn(|| {
                lock.save_until(Instant::now() + Duration::from_millis(20))
                    .is_err()
            });
            assert!(rejected.join().unwrap());
        });
        drop(first);

        assert!(!*lock.check());
    }
}
