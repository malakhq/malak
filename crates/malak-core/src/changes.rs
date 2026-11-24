use parking_lot::{Condvar, Mutex};
use std::time::Instant;

#[derive(Default)]
pub(crate) struct Changes {
    revision: Mutex<u64>,
    changed: Condvar,
}

impl Changes {
    pub(crate) fn revision(&self) -> u64 {
        *self.revision.lock()
    }

    pub(crate) fn notify(&self) {
        let mut revision = self.revision.lock();
        *revision = revision.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Wait for a change after the caller's snapshot, without losing wakeups.
    pub(crate) fn wait(&self, observed: u64, until: Instant) -> bool {
        let mut revision = self.revision.lock();
        while *revision == observed {
            if self.changed.wait_until(&mut revision, until).timed_out() {
                return *revision != observed;
            }
        }
        true
    }
}
