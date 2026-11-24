use super::{Owner, Table, validate_start};
use crate::journal_entry::{Entry, SavedKeys};
use crate::storage::StoreLease;
use crate::workers::Workers;
use crate::{CreationId, Error, Limits, random};
use malak_journal::{Checkpoint, InspectedJournal, StateFolder};
use malak_layers::Store;
use malak_proto::KeyPair;
use malak_record::RecordKey;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zeroize::Zeroizing;

mod recovery;
mod restart;
mod start;
pub use recovery::RecoveryReport;
pub(super) use restart::{LocalReplay, reopen};
pub(super) use start::Start;

pub(super) struct NewBoard {
    store: StoreLease,
    workers: Arc<Workers>,
    key: Arc<RecordKey>,
    pub(super) board_key: KeyPair,
    pub(super) owner_key: KeyPair,
    pub(super) creation: CreationId,
    path: PathBuf,
    limits: Limits,
    lead_saved: bool,
    lead_enrolled_at_ms: Option<u64>,
    complete: bool,
}

impl Owner {
    /// Create a Direct board on an empty durable store and private state folder.
    ///
    /// Reuses the saved keys and lead inbox if the first initialization was
    /// interrupted before a run was saved. Opening a previously run board is
    /// a separate operation.
    pub fn create_durable(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let state = state.as_ref();
        let prepared = Start::direct(inspect(state, &limits)?)?;
        let mut board = NewBoard::initialize(store, limits, state, prepared)?;
        if !board.lead_saved {
            let enrolled_at_ms = board.mark_lead_enrolment();
            board.save(Entry::Lead {
                creation: board.creation,
                enrolled_at_ms,
            })?;
        }
        board.finish().map(|(owner, _)| owner)
    }
}

pub(super) fn check_local(state: &Path, limits: &Limits, uid: u32) -> Result<Start, Error> {
    Start::local(inspect(state, limits)?, uid)
}

fn inspect(state: &Path, limits: &Limits) -> Result<InspectedJournal, Error> {
    validate_start(limits)?;
    let folder = StateFolder::acquire(state)
        .map_err(|error| Error::unavailable(format!("board state: {error}")))?;
    #[cfg(test)]
    crate::durable_kill::hit("state-locked");
    let inspected = folder
        .inspect()
        .map_err(|error| Error::unavailable(format!("board state: {error}")))?;
    #[cfg(test)]
    crate::durable_kill::hit("journal-read");
    Ok(inspected)
}

impl NewBoard {
    pub(super) fn initialize(
        store: impl Store + 'static,
        limits: Limits,
        state: &Path,
        start: Start,
    ) -> Result<Self, Error> {
        let workers = Arc::new(Workers::default());
        let (mut store, ownership) = StoreLease::take(store, workers.clone(), true)?;
        #[cfg(test)]
        crate::durable_kill::hit("first-store-claimed");
        let store_id = ownership.store_id;
        let has_keys = start.keys.is_some();
        let lead = start.creation();
        let lead_enrolled_at_ms = start.lead_enrolled_at_ms();
        let keys = match start.keys {
            Some(keys) => {
                if keys.store != store_id {
                    return Err(Error::refused("the saved board names a different store"));
                }
                *keys
            }
            None => SavedKeys {
                record: Zeroizing::new(random::<32>()?),
                board: key_pair()?,
                owner: key_pair()?,
                store: store_id,
                location: ownership.location.filter(|location| {
                    location.folder_number != 0 && !location.filesystem_id.is_empty()
                }),
            },
        };
        let key = Arc::new(RecordKey::new(&keys.record));
        if has_keys {
            crate::witness::check_start(&start.inspected, &store, &key, &workers)?;
        }
        if ownership.has_records {
            return Err(Error::refused("first initialization needs an empty store"));
        }
        let lead_saved = lead.is_some();
        let creation = match lead {
            Some(lead) => lead,
            None => CreationId::from_bytes(random()?),
        };
        store.install_journal(
            start
                .inspected
                .activate()
                .map_err(|error| Error::unavailable(format!("board journal: {error}")))?,
            Checkpoint::EMPTY,
        );
        #[cfg(test)]
        crate::durable_kill::hit("first-journal-activated");
        if !has_keys {
            store.save(Entry::Keys {
                record: &keys.record,
                board: &keys.board,
                owner: &keys.owner,
                store: keys.store,
                location: keys.location.as_ref(),
            })?;
        }
        Ok(Self {
            store,
            workers,
            key,
            board_key: keys.board,
            owner_key: keys.owner,
            creation,
            path: state.to_path_buf(),
            limits,
            lead_saved,
            lead_enrolled_at_ms,
            complete: start.complete,
        })
    }

    pub(super) fn mark_lead_enrolment(&mut self) -> Option<u64> {
        if !self.lead_saved {
            self.lead_enrolled_at_ms = Some(crate::epoch_millis(std::time::SystemTime::now()));
        }
        self.lead_enrolled_at_ms
    }

    pub(super) fn save(&self, entry: Entry<'_>) -> Result<(), Error> {
        self.store.save(entry)
    }

    pub(super) fn finish(self) -> Result<(Owner, KeyPair), Error> {
        if !self.complete {
            self.save(Entry::Complete)?;
        }
        let run = random()?;
        self.save(Entry::Run(run))?;
        let table = Table::first(self.creation, run, &self.limits, self.lead_enrolled_at_ms);
        let owner = Owner::assemble(
            self.limits,
            self.store,
            self.workers,
            self.key,
            self.path,
            run,
            table,
        )?;
        crate::witness::start(&owner.board.store, &owner.board.key, Default::default())?;
        Ok((owner, self.board_key))
    }
}

pub(super) fn key_pair() -> Result<KeyPair, Error> {
    KeyPair::generate().map_err(|_| Error::unavailable("could not generate a handshake key"))
}
