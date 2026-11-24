use crate::{Error, ParticipantId};
use malak_layers::Source;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ONE: u64 = 1_000_000_000;
const BURST: u64 = 40 * ONE;
const REFILL: u64 = 20;
const REFILL_TIME: Duration = Duration::from_secs(2);

pub(crate) struct Gate {
    capacity: usize,
    per_source: Option<usize>,
    rate: bool,
    state: Mutex<State>,
}

struct State {
    open: usize,
    sources: HashMap<Source, Bucket>,
    last_sweep: Instant,
}

struct Bucket {
    open: usize,
    credit: u64,
    updated: Instant,
}

impl Gate {
    pub(crate) fn new(capacity: usize, per_source: Option<usize>, rate: bool) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            per_source,
            rate,
            state: Mutex::new(State {
                open: 0,
                sources: HashMap::new(),
                last_sweep: Instant::now(),
            }),
        })
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        source: Option<&Source>,
        now: Instant,
    ) -> Result<Place, Error> {
        let mut state = self.state.lock();
        if now.saturating_duration_since(state.last_sweep) >= REFILL_TIME {
            state.sources.retain(|_, bucket| {
                bucket.open > 0 || now.saturating_duration_since(bucket.updated) < REFILL_TIME
            });
            state.last_sweep = now;
        }

        let at_capacity = state.open >= self.capacity;
        let source = source.filter(|_| self.rate);
        if let Some(source) = source {
            let bucket = state.sources.entry(source.clone()).or_insert(Bucket {
                open: 0,
                credit: BURST,
                updated: now,
            });
            let nanos = now.saturating_duration_since(bucket.updated).as_nanos();
            let elapsed = nanos.min(REFILL_TIME.as_nanos()) as u64;
            bucket.credit = (bucket.credit + elapsed * REFILL).min(BURST);
            bucket.updated = bucket.updated.max(now);
            if bucket.credit < ONE {
                return Err(Error::unavailable("source connection rate exceeded"));
            }
            bucket.credit -= ONE;
            if self.per_source.is_some_and(|limit| bucket.open >= limit) {
                return Err(Error::unavailable("source has no connection places left"));
            }
            if !at_capacity {
                bucket.open += 1;
            }
        }
        if at_capacity {
            return Err(Error::unavailable("entrance has no connection places left"));
        }
        state.open += 1;
        Ok(Place {
            gate: self.clone(),
            source: source.cloned(),
        })
    }
}

pub(crate) struct Place {
    gate: Arc<Gate>,
    source: Option<Source>,
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock();
        state.open -= 1;
        if let Some(source) = &self.source {
            state.sources.get_mut(source).expect("open source").open -= 1;
        }
    }
}

pub(crate) struct ActiveGate {
    capacity: usize,
    per_participant: usize,
    state: Mutex<Active>,
}

#[derive(Default)]
struct Active {
    open: usize,
    participants: HashMap<ParticipantId, usize>,
}

impl ActiveGate {
    pub(crate) fn new(capacity: usize, per_participant: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity,
            per_participant,
            state: Mutex::default(),
        })
    }

    pub(crate) fn reserve(self: &Arc<Self>, id: &ParticipantId) -> Option<ActivePlace> {
        let mut state = self.state.lock();
        if state.open >= self.capacity {
            return None;
        }
        if state.participants.get(id).copied().unwrap_or(0) >= self.per_participant {
            return None;
        }
        *state.participants.entry(id.clone()).or_default() += 1;
        state.open += 1;
        Some(ActivePlace {
            gate: self.clone(),
            id: id.clone(),
        })
    }
}

pub(crate) struct ActivePlace {
    gate: Arc<ActiveGate>,
    id: ParticipantId,
}

impl Drop for ActivePlace {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock();
        let count = state
            .participants
            .get_mut(&self.id)
            .expect("reserved participant");
        *count -= 1;
        if *count == 0 {
            state.participants.remove(&self.id);
        }
        state.open -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_opening_place_returns_when_a_link_closes() {
        let gate = Gate::new(1, None, false);
        let now = Instant::now();
        let opened = gate.reserve(None, now).unwrap();

        assert!(gate.reserve(None, now).is_err());
        drop(opened);

        assert!(gate.reserve(None, now).is_ok());
    }

    #[test]
    fn one_source_cannot_take_other_sources_opening_places() {
        let gate = Gate::new(2, Some(1), true);
        let now = Instant::now();
        let one = Source::LocalUser(1);
        let other = Source::LocalUser(2);
        let _opened = gate.reserve(Some(&one), now).unwrap();

        assert!(gate.reserve(Some(&one), now).is_err());
        assert!(gate.reserve(Some(&other), now).is_ok());
    }

    #[test]
    fn a_source_can_burst_and_then_must_wait_for_credit() {
        let gate = Gate::new(1, None, true);
        let now = Instant::now();
        let source = Source::LocalUser(1);
        for _ in 0..40 {
            gate.reserve(Some(&source), now).unwrap();
        }

        assert!(gate.reserve(Some(&source), now).is_err());
        assert!(
            gate.reserve(Some(&source), now + Duration::from_millis(49))
                .is_err()
        );
        assert!(
            gate.reserve(Some(&source), now + Duration::from_millis(50))
                .is_ok()
        );
    }

    #[test]
    fn an_attached_participant_cannot_take_another_participants_place() {
        let gate = ActiveGate::new(2, 1);
        let first = "member01".parse().unwrap();
        let second = "member02".parse().unwrap();
        let place = gate.reserve(&first).unwrap();

        assert!(gate.reserve(&first).is_none());
        assert!(gate.reserve(&second).is_some());
        drop(place);
        assert!(gate.reserve(&first).is_some());
    }
}
