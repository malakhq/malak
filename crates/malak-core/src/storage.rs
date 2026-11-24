use crate::Error;
use crate::journal_entry::Entry;
use crate::workers::Workers;
use malak_layers::{Claim, Claimed, Durability, Ownership, Store, StoreInfo};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod journal;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use journal::Writer;

pub(crate) const STORE_WAIT: Duration = Duration::from_secs(5);

pub(crate) struct StoreLease {
    pub(crate) store: Arc<dyn Store>,
    pub(crate) generation: u64,
    pub(crate) info: StoreInfo,
    workers: Arc<Workers>,
    run: Arc<RunState>,
    released: AtomicBool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) events: Arc<crate::board::events::Events>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) lead_session: arc_swap::ArcSwapOption<crate::LeadSession>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    journal: Option<Arc<WitnessJournal>>,
}

#[derive(Default)]
struct RunState {
    older_state: AtomicBool,
    stopped: AtomicBool,
}

impl RunState {
    fn running(&self) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(stopped());
        }
        if self.older_state.load(Ordering::Acquire) {
            return Err(older_state());
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct WitnessJournal {
    writer: parking_lot::Mutex<Option<Writer>>,
    run: Arc<RunState>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl WitnessJournal {
    pub(crate) fn hand(&self) -> Result<Option<malak_journal::Checkpoint>, Error> {
        let mut writer = self.writer.lock();
        self.run.running()?;
        writer.as_mut().ok_or_else(stopped)?.hand_witness()
    }

    pub(crate) fn confirm(
        &self,
        at: malak_journal::Checkpoint,
        position: crate::Position,
    ) -> Result<(), Error> {
        let mut writer = self.writer.lock();
        self.run.running()?;
        writer
            .as_mut()
            .ok_or_else(stopped)?
            .confirm_witness(at, position)
    }

    #[cfg(test)]
    pub(crate) fn confirmed(&self) -> malak_journal::Checkpoint {
        self.writer
            .lock()
            .as_ref()
            .expect("open journal")
            .confirmed_witness()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct RepairClaim {
    pub(crate) ownership: Ownership,
    pub(crate) place: malak_layers::StorePlace,
    pub(crate) channels: Vec<String>,
}

impl StoreLease {
    pub(crate) fn take_new(
        store: impl Store + 'static,
        workers: Arc<Workers>,
        durable: bool,
    ) -> Result<(Self, [u8; 16]), Error> {
        let (lease, owner) = Self::take(store, workers, durable)?;
        if owner.has_records {
            return Err(Error::refused("a new board needs an empty store"));
        }
        Ok((lease, owner.store_id))
    }

    pub(crate) fn take(
        store: impl Store + 'static,
        workers: Arc<Workers>,
        durable: bool,
    ) -> Result<(Self, Ownership), Error> {
        Self::take_shared(Arc::new(store), workers, durable)
    }

    pub(crate) fn take_shared(
        store: Arc<dyn Store>,
        workers: Arc<Workers>,
        durable: bool,
    ) -> Result<(Self, Ownership), Error> {
        let (lease, claimed) = Self::claim(store, workers, durable, Claim::Take)?;
        let Claimed::Taken(owner) = claimed else {
            return Err(Error::unavailable("store did not grant ownership"));
        };
        Ok((lease, owner))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn repair(
        store: impl Store + 'static,
        workers: Arc<Workers>,
    ) -> Result<(Self, RepairClaim), Error> {
        Self::repair_shared(Arc::new(store), workers)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn repair_shared(
        store: Arc<dyn Store>,
        workers: Arc<Workers>,
    ) -> Result<(Self, RepairClaim), Error> {
        let (lease, claimed) = Self::claim(store, workers, true, Claim::Repair)?;
        let Claimed::Repair {
            ownership,
            place,
            channels,
        } = claimed
        else {
            return Err(Error::unavailable("store did not grant repair ownership"));
        };
        Ok((
            lease,
            RepairClaim {
                ownership,
                place,
                channels,
            },
        ))
    }

    fn claim(
        store: Arc<dyn Store>,
        workers: Arc<Workers>,
        durable: bool,
        request: Claim<'static>,
    ) -> Result<(Self, Claimed), Error> {
        let pool = workers.clone();
        workers.until(Instant::now() + STORE_WAIT, move || {
            let info = store.describe();
            if durable == (info.durability == Durability::Memory) {
                return Err(Error::not_supported(
                    "store durability does not match the board",
                ));
            }
            let claimed = match store.claim(request) {
                Err(error)
                    if matches!(request, Claim::Repair)
                        && error.outcome() == malak_layers::Outcome::NotSupported =>
                {
                    return Err(error);
                }
                result => layer(result)?,
            };
            let generation = match &claimed {
                Claimed::Taken(owner)
                | Claimed::Repair {
                    ownership: owner, ..
                } => owner.generation,
                Claimed::Done => return Err(Error::unavailable("store did not grant ownership")),
            };
            let lease = Self {
                store,
                generation,
                info,
                workers: pool,
                run: Arc::default(),
                released: AtomicBool::new(false),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                events: Arc::default(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                lead_session: Default::default(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                journal: None,
            };
            Ok((lease, claimed))
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn install_journal(
        &mut self,
        journal: malak_journal::Journal,
        confirmed: malak_journal::Checkpoint,
    ) {
        self.journal = Some(Arc::new(WitnessJournal {
            writer: parking_lot::Mutex::new(Some(Writer::new(journal, confirmed))),
            run: self.run.clone(),
        }));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn journaled(&self) -> bool {
        self.journal.is_some()
    }

    pub(crate) fn save(&self, entry: Entry<'_>) -> Result<(), Error> {
        self.running()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let lead_session = match &entry {
                Entry::Head {
                    sender,
                    lead_session,
                    ..
                } if sender.is_lead() => {
                    Some(lead_session.map(|session| Arc::new(session.clone())))
                }
                _ => None,
            };
            // Keep publication in the journal's order across distinct channels.
            let mut writer = self.journal.as_ref().map(|journal| journal.writer.lock());
            if let Some(journal) = &mut writer {
                self.running()?;
                #[cfg(test)]
                let checkpoint = crate::durable_kill::before_save(&entry);
                journal
                    .as_mut()
                    .expect("open journal")
                    .save(entry, self.info.durability)?;
                #[cfg(test)]
                crate::durable_kill::after_save(checkpoint);
            }
            if let Some(session) = lead_session {
                self.lead_session.store(session);
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = entry;
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn witness_journal(&self) -> &Arc<WitnessJournal> {
        self.journal.as_ref().expect("durable store")
    }

    pub(crate) fn running(&self) -> Result<(), Error> {
        self.run.running()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn stop_for_older_state(&self) -> Error {
        let _journal = self.journal.as_ref().map(|journal| journal.writer.lock());
        self.run.older_state.store(true, Ordering::Release);
        older_state()
    }

    pub(crate) fn writable(&self) -> Result<(), Error> {
        self.running()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if self
            .journal
            .as_ref()
            .is_some_and(|journal| !journal.writer.lock().as_ref().is_some_and(Writer::writable))
        {
            return Err(Error::unavailable("board journal has stopped writes"));
        }
        Ok(())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn begin_stop(&self, deadline: Instant) -> Result<(), Error> {
        let _journal = self
            .journal
            .as_ref()
            .map(|journal| {
                journal
                    .writer
                    .try_lock_until(deadline)
                    .ok_or_else(|| Error::unavailable("journal save did not finish in time"))
            })
            .transpose()?;
        self.running()?;
        self.run.stopped.store(true, Ordering::Release);
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn finish_stop(&self) {
        self.release(true);
    }

    fn release(&self, wait: bool) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        // Remove access from the publisher while holding the folder lock
        // until the store has been released.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let _journal = self
            .journal
            .as_ref()
            .and_then(|journal| journal.writer.lock().take());
        let store = self.store.clone();
        let generation = self.generation;
        let release = move || store.claim(Claim::Release { generation }).map(|_| ());
        if wait {
            let _ = self.workers.until(Instant::now() + STORE_WAIT, release);
        } else {
            let _ = self.workers.dispatch(move || {
                let _ = release();
            });
        }
    }
}

impl Drop for StoreLease {
    fn drop(&mut self) {
        self.release(self.info.durability != Durability::Memory);
    }
}

pub(crate) fn layer<T>(result: Result<T, Error>) -> Result<T, Error> {
    result.map_err(|error| {
        if error.outcome() == malak_layers::Outcome::Unavailable {
            error
        } else {
            Error::unavailable("store operation failed")
        }
    })
}

fn older_state() -> Error {
    Error::unavailable("the state folder is older than the store")
}

fn stopped() -> Error {
    Error::unavailable("board has stopped")
}
