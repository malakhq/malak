use crate::{ParticipantId, Position};
use arc_swap::ArcSwap;
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct ChannelActivity {
    pub(crate) readers: ArcSwap<BTreeMap<ParticipantId, u64>>,
}

impl ChannelActivity {
    pub(crate) fn acknowledge(&self, id: &ParticipantId, position: Position) {
        let mut readers = BTreeMap::clone(&self.readers.load());
        readers.insert(id.clone(), position.get());
        self.readers.store(readers.into());
    }

    pub(crate) fn forget_reader(&self, id: &ParticipantId) {
        let current = self.readers.load();
        if current.contains_key(id) {
            let mut readers = BTreeMap::clone(&current);
            readers.remove(id);
            self.readers.store(readers.into());
        }
    }
}
