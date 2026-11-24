use super::{Board, Owner, Table, owner_save_deadline};
use crate::channel::{Channel, LockedHistory};
use crate::journal_entry::Entry;
use crate::storage::STORE_WAIT;
use crate::{ChannelName, CreationId, Error};
use std::sync::Arc;
use std::time::Instant;

impl Owner {
    /// Remove one creation and return whether it was removed and the current creation.
    ///
    /// A stale creation ID leaves a successor of the same name in place.
    pub fn remove_channel(
        &self,
        name: &str,
        creation: CreationId,
    ) -> Result<(bool, Option<CreationId>), Error> {
        let result = self.board.remove_channel(name, creation);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        self.board.events.owner(
            "CHANNEL_REMOVE",
            super::OwnerTarget::Channel(Some(name), None),
            result.as_ref().err(),
        );
        result
    }
}

impl Board {
    pub(super) fn remove_channel(
        self: &Arc<Self>,
        name: &str,
        creation: CreationId,
    ) -> Result<(bool, Option<CreationId>), Error> {
        let name: ChannelName = name.parse()?;
        let deadline = owner_save_deadline();
        let current = self
            .table
            .save_until(deadline)?
            .channels
            .get(&name)
            .cloned();
        let Some(channel) = current else {
            return Ok((false, None));
        };
        if channel.creation != creation {
            return Ok((false, Some(channel.creation)));
        }
        let history = channel
            .history
            .try_lock_arc_until(deadline)
            .ok_or_else(|| Error::unavailable("channel write did not settle in time"))?;
        if channel.is_removed() {
            return Ok((false, self.current_creation(&name, deadline)?));
        }
        channel.available()?;
        let history = self.ready_history(&channel, history, Instant::now() + STORE_WAIT)?;
        {
            let mut table = self.table.save_until(deadline)?;
            if !table.is_current(&channel) {
                return Ok((
                    false,
                    table.channels.get(&name).map(|current| current.creation),
                ));
            }
            channel.available()?;
            self.store.save(Entry::Remove(creation))?;
            table.begin_removal(&channel);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.publish(&table);
        }
        self.changes.notify();
        let board = self.clone();
        self.workers.until(Instant::now() + STORE_WAIT, move || {
            board.finish_removal(&channel, history)
        })?;
        Ok((true, self.current_creation(&name, owner_save_deadline())?))
    }

    fn current_creation(
        &self,
        name: &ChannelName,
        deadline: Instant,
    ) -> Result<Option<CreationId>, Error> {
        Ok(self
            .table
            .save_until(deadline)?
            .channels
            .get(name)
            .map(|current| current.creation))
    }

    fn finish_removal(&self, channel: &Channel, mut history: LockedHistory) -> Result<(), Error> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        history.check_removal_tail(channel, &self.store, &self.key)?;
        history.empty_store(channel, &self.store)?;
        let mut table = self.table.save();
        self.store.save(Entry::RemoveDone(channel.creation))?;
        history.release_removed_space(&self.quota);
        table.finish_removal(channel.creation);
        Ok(())
    }

    pub(super) fn sweep_removals(self: &Arc<Self>) {
        let removals = self
            .table
            .check()
            .removals
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for channel in removals {
            let Some(history) = channel.history.try_lock_arc() else {
                continue;
            };
            if !self.table.check().removals.contains_key(&channel.creation) {
                continue;
            }
            let board = self.clone();
            let _ = self.workers.dispatch(move || {
                let _ = board.finish_removal(&channel, history);
            });
        }
    }
}

impl Table {
    pub(super) fn begin_removal(&mut self, channel: &Arc<Channel>) {
        self.channels.remove(&channel.name);
        self.removals.insert(channel.creation, channel.clone());
        channel.mark_removed();
    }

    pub(super) fn finish_removal(&mut self, creation: CreationId) {
        self.removals.remove(&creation);
        self.readers.forget_creation(creation);
        if let Some(creator) = self.creators.remove(&creation) {
            let count = self.counts.get_mut(&creator).expect("channel maker count");
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&creator);
            }
        }
    }
}
