use crate::link::Wire;
use crate::{Error, ParticipantId};
use malak_layers::Transport;
use parking_lot::Mutex;
use std::sync::{Arc, OnceLock, Weak};

#[derive(Default)]
pub(super) struct Links(Mutex<State>);

#[derive(Default)]
struct State {
    closed: bool,
    links: Vec<Weak<LiveLink>>,
}

pub(super) struct LiveLink {
    wire: Arc<dyn CloseLink>,
    participant: OnceLock<ParticipantId>,
    local_process: bool,
}

trait CloseLink: Send + Sync {
    fn close(&self);
}

impl<T: Transport> CloseLink for Wire<T> {
    fn close(&self) {
        self.close();
    }
}

impl Links {
    pub(super) fn register<T: Transport + 'static>(
        &self,
        wire: Arc<Wire<T>>,
    ) -> Result<Arc<LiveLink>, Error> {
        let mut state = self.0.lock();
        if state.closed {
            return Err(Error::unavailable("board has stopped"));
        }
        state.links.retain(|link| link.strong_count() != 0);
        let live = Arc::new(LiveLink {
            local_process: wire.process_id().is_some(),
            wire,
            participant: OnceLock::new(),
        });
        state.links.push(Arc::downgrade(&live));
        Ok(live)
    }

    pub(super) fn participant(&self, id: &ParticipantId) -> Vec<Arc<LiveLink>> {
        self.participant_links(id, false)
    }

    pub(super) fn participant_sessions(&self, id: &ParticipantId) -> Vec<Arc<LiveLink>> {
        self.participant_links(id, true)
    }

    fn participant_links(&self, id: &ParticipantId, local_only: bool) -> Vec<Arc<LiveLink>> {
        self.0
            .lock()
            .links
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|link| {
                link.participant.get() == Some(id) && (!local_only || link.local_process)
            })
            .collect()
    }

    pub(super) fn close_all(&self) {
        let pending = {
            let mut state = self.0.lock();
            state.closed = true;
            std::mem::take(&mut state.links)
        };
        for link in pending.into_iter().filter_map(|link| link.upgrade()) {
            link.close();
        }
    }
}

impl LiveLink {
    pub(super) fn identify(&self, participant: &ParticipantId) {
        self.participant
            .set(participant.clone())
            .expect("one identity per link");
    }

    pub(super) fn close(&self) {
        self.wire.close();
    }
}
