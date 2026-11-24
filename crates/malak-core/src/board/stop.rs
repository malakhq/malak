use super::{Board, Owner, OwnerTarget, owner_save_deadline};
use crate::{CreationId, Error};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const TAIL_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

impl Owner {
    /// Stop this specific run and release its store and state folder.
    ///
    /// A different or earlier run ID is refused.
    pub fn stop(&self, run: CreationId) -> Result<(), Error> {
        let result = self.board.begin_stop(run);
        self.board
            .events
            .owner("STOP", OwnerTarget::Run(Some(run)), result.as_ref().err());
        result?;
        self.board.finish_stop();
        Ok(())
    }
}

impl Board {
    pub(super) fn begin_stop(&self, run: CreationId) -> Result<(), Error> {
        if CreationId::from_bytes(self.run) != run {
            return Err(Error::refused("board run ID does not match"));
        }
        let deadline = owner_save_deadline();
        let _authority = self.table.save_until(deadline)?;
        self.store.begin_stop(deadline)
    }

    pub(super) fn finish_stop(&self) {
        self.events.flush(Instant::now() + TAIL_FLUSH_TIMEOUT);
        self.links.close_all();
        self.changes.notify();
        if let Some(worker) = self.retention_thread.lock().take() {
            worker.unpark();
        }
        self.store.finish_stop();
        self.stopped.store(true, Ordering::Release);
    }
}
