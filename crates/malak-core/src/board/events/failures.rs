use super::{Event, EventKind, Events};
use crate::{Error, Outcome, ParticipantId};
use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Failures {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    pending: HashMap<ParticipantId, Calls>,
    closed: bool,
}

#[derive(Default)]
struct Calls {
    denied: Option<Event>,
    limited: Option<Event>,
}

impl Failures {
    pub(super) fn start(self: &Arc<Self>, events: Weak<Events>) -> Result<(), Error> {
        let failures = self.clone();
        std::thread::Builder::new()
            .name("malak-call-events".into())
            .spawn(move || failures.publish(&events))
            .map_err(|_| Error::unavailable("could not start the call event task"))?;
        Ok(())
    }

    pub(super) fn record(&self, mut event: Event, limited: bool) {
        let mut state = self.state.lock();
        let wake = state.pending.is_empty();
        let id = event
            .participant
            .as_ref()
            .expect("a failed call has a participant");
        let calls = state.pending.entry(id.clone()).or_default();
        if event.outcome == Some(Outcome::Refused) {
            let count = calls
                .denied
                .as_ref()
                .map_or(1, |event| event.count.saturating_add(1));
            let mut sample = event.clone();
            sample.count = count;
            calls.denied = Some(sample);
        }
        if limited {
            event.kind = EventKind::LimitHit;
            event.count = calls
                .limited
                .as_ref()
                .map_or(1, |event| event.count.saturating_add(1));
            calls.limited = Some(event);
        }
        if wake {
            self.changed.notify_one();
        }
    }

    pub(super) fn stop(&self) {
        self.state.lock().closed = true;
        self.changed.notify_one();
    }

    fn publish(&self, events: &Weak<Events>) {
        let mut state = self.state.lock();
        loop {
            while state.pending.is_empty() && !state.closed {
                self.changed.wait(&mut state);
            }
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until && !state.closed {
                self.changed.wait_until(&mut state, until);
            }
            if state.closed {
                break;
            }
            let pending = std::mem::take(&mut state.pending);
            drop(state);
            let Some(events) = events.upgrade() else {
                break;
            };
            for calls in pending.into_values() {
                for event in [calls.denied, calls.limited].into_iter().flatten() {
                    events.emit(event);
                }
            }
            drop(events);
            state = self.state.lock();
        }
    }
}
