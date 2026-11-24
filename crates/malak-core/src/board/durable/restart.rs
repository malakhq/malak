use super::{Start, inspect};
use crate::board::{BoardState, Table};
use crate::channel::{Channel, Reopen, SavedReservation};
use crate::journal_entry::{AcceptedPlan, Change, Entry};
use crate::quota::{Quota, Reservation};
use crate::storage::{STORE_WAIT, StoreLease};
use crate::workers::Workers;
use crate::{ChannelName, CreationId, Error, Limits, Owner, ParticipantId, Position, random};
use malak_journal::Checkpoint;
use malak_layers::Store;
use malak_proto::KeyPair;
use malak_record::RecordKey;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

const ENTRIES_BEFORE_ARCHIVE: usize = 4096;

impl Owner {
    /// Reopen a previously initialized Direct board on its original store.
    ///
    /// Restores authority, reader positions and the retained retry window.
    /// Each channel is checked before use and saved cuts and trims resume.
    /// A complete append matching its saved reservation can be adopted.
    /// Cuts any uncommitted tail that the journal cannot adopt.
    pub fn restart_durable(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
    ) -> Result<Self, Error> {
        let state = state.as_ref();
        let loaded = Start::load(inspect(state, &limits)?)?;
        if loaded.current_lead_key().is_some() {
            return Err(Error::not_supported("state belongs to a local board"));
        }
        reopen(store, limits, state, loaded, None).map(|(owner, _)| owner)
    }
}

pub(in crate::board) struct LocalReplay<'a> {
    pub(in crate::board) lead_uid: u32,
    pub(in crate::board) mechanism: &'a str,
}

pub(in crate::board) fn reopen(
    store: impl Store + 'static,
    limits: Limits,
    state: &Path,
    loaded: Start,
    local: Option<LocalReplay<'_>>,
) -> Result<(Owner, KeyPair), Error> {
    let replay = Replay::load(&loaded, &limits, local.as_ref())?;
    #[cfg(test)]
    crate::durable_kill::hit("state-replayed");
    let workers = Arc::new(Workers::default());
    let store: Arc<dyn Store> = Arc::new(store);
    let (loaded, mut replay) = super::recovery::resume_at_start(
        loaded,
        replay,
        store.clone(),
        &workers,
        &limits,
        local.as_ref(),
    )?;
    let keys = loaded.keys.as_ref().ok_or_else(invalid)?;
    let (mut store, ownership) = StoreLease::take_shared(store, workers.clone(), true)?;
    #[cfg(test)]
    crate::durable_kill::hit("store-claimed");
    if keys.store != ownership.store_id {
        return Err(Error::refused("the saved board names a different store"));
    }
    let key = Arc::new(RecordKey::new(&keys.record));
    crate::witness::check_start(&loaded.inspected, &store, &key, &workers)?;
    #[cfg(test)]
    crate::durable_kill::hit("witness-checked");
    let archive = (loaded.inspected.entries().count() >= ENTRIES_BEFORE_ARCHIVE)
        .then(|| loaded.inspected.archive_payload())
        .transpose()
        .map_err(|error| Error::unavailable(format!("board journal archive: {error}")))?;
    let mut journal = loaded
        .inspected
        .activate()
        .map_err(|error| Error::unavailable(format!("board journal: {error}")))?;
    #[cfg(test)]
    crate::durable_kill::hit("journal-activated");
    if let Some(archive) = archive {
        journal
            .compact(&archive)
            .map_err(|error| Error::unavailable(format!("board journal archive: {error}")))?;
    }
    let (confirmed, positions) = replay.confirmed();
    let run = random()?;
    if replay.runs.contains_key(&run) {
        return Err(Error::unavailable("generated run ID is already on record"));
    }
    let board = replay.finish(run);
    store.install_journal(journal, confirmed);
    store.save(Entry::Run(run))?;
    #[cfg(test)]
    crate::enrol_kill::restart_checkpoint(state);
    let owner = Owner::assemble_state(limits, store, workers, key, state.to_path_buf(), board)?;
    owner.board.sweep_retention();
    crate::witness::start(&owner.board.store, &owner.board.key, positions)?;
    #[cfg(test)]
    crate::durable_kill::hit("authority-loaded");
    Ok((owner, loaded.keys.expect("validated saved keys").board))
}

pub(super) struct Replay {
    pub(super) table: Table,
    pub(super) quota: Arc<Mutex<Quota>>,
    pub(super) channels: HashMap<CreationId, Arc<Channel>>,
    run: [u8; 16],
    pub(super) runs: HashMap<[u8; 16], usize>,
    pub(super) reservations: HashMap<CreationId, SavedReservation>,
    pub(super) cuts: HashSet<CreationId>,
    pub(super) repair: Option<AcceptedPlan>,
    handed: Option<Checkpoint>,
    published: Checkpoint,
    positions: VecDeque<Position>,
    lead_session: Option<crate::LeadSession>,
}

impl Replay {
    pub(super) fn load(
        loaded: &Start,
        limits: &Limits,
        local: Option<&LocalReplay<'_>>,
    ) -> Result<Self, Error> {
        let (first_sequence, first_run) = loaded.run.ok_or_else(|| {
            Error::refused(
                "board initialization has not completed; start a new run with create_durable",
            )
        })?;
        let keys = loaded.keys.as_ref().ok_or_else(invalid)?;
        let creation = loaded.creation().ok_or_else(invalid)?;
        let mut replay = Replay::new(creation, first_run, limits, loaded.lead_enrolled_at_ms());
        if let Some(local) = local {
            let lead_key = loaded.current_lead_key().ok_or_else(invalid)?;
            replay
                .table
                .authority
                .set_local_keys(
                    *keys.board.public(),
                    *keys.owner.public(),
                    *lead_key,
                    local.lead_uid,
                )
                .map_err(|_| invalid())?;
            for key in loaded.retired_lead_keys() {
                replay
                    .table
                    .authority
                    .retire_key(key)
                    .map_err(|_| invalid())?;
            }
        }
        let mut checkpoints = HashMap::new();
        let entries = loaded
            .inspected
            .accepted_entries()
            .map_err(|error| Error::unavailable(format!("board journal: {error}")))?;
        for (at, bytes) in entries {
            if at.sequence > first_sequence {
                replay.apply(Change::decode(bytes)?, limits, &checkpoints, local)?;
            }
            checkpoints.insert(at.sequence, at);
        }
        Ok(replay)
    }

    fn new(
        creation: CreationId,
        run: [u8; 16],
        limits: &Limits,
        enrolled_at_ms: Option<u64>,
    ) -> Self {
        let table = Table::first(creation, run, limits, enrolled_at_ms);
        let lead = table.channels[&ParticipantId::lead().inbox()].clone();
        Self {
            table,
            quota: Arc::new(Mutex::new(Quota::default())),
            channels: HashMap::from([(creation, lead)]),
            run,
            runs: HashMap::from([(run, 0)]),
            reservations: HashMap::new(),
            cuts: HashSet::new(),
            repair: None,
            handed: None,
            published: Checkpoint::EMPTY,
            positions: VecDeque::new(),
            lead_session: None,
        }
    }

    fn apply(
        &mut self,
        change: Change,
        limits: &Limits,
        checkpoints: &HashMap<u64, Checkpoint>,
        local: Option<&LocalReplay<'_>>,
    ) -> Result<(), Error> {
        match change {
            Change::Repair(change) => self.apply_repair(change)?,
            Change::Run(run) => {
                if self.runs.insert(run, self.runs.len()).is_some() {
                    return Err(invalid());
                }
                self.run = run;
            }
            Change::Enrol {
                id,
                name,
                creation,
                grant,
                socket,
                enrolled_at_ms,
            } => {
                if self.table.authority.has_id(&id)
                    || self.table.authority.check_name(&name).is_err()
                {
                    return Err(invalid());
                }
                self.create(id.inbox(), creation, limits)?;
                self.table
                    .authority
                    .insert(id.clone(), name, grant, enrolled_at_ms);
                if let Some(socket) = socket {
                    let local = local.ok_or_else(invalid)?;
                    if socket.mechanism != local.mechanism
                        || socket
                            .record
                            .as_ref()
                            .is_some_and(|record| record.len() > crate::authority::MAX_RECORD)
                    {
                        return Err(invalid());
                    }
                    self.table
                        .authority
                        .require_new_key(&socket.key)
                        .map_err(|_| invalid())?;
                    self.table.authority.register_socket(
                        &id,
                        Some(socket.key),
                        socket.place,
                        Some(socket.uid),
                        socket.record,
                    );
                }
            }
            Change::Grant(id, generation, grant, changed_at_ms) => {
                self.table
                    .authority
                    .restore_grant(&id, generation, grant, changed_at_ms)
                    .map_err(|_| invalid())?;
            }
            Change::Revoke(id, revoked_at_ms) => {
                self.table
                    .authority
                    .revoke(&id, revoked_at_ms, || Ok(()))
                    .map_err(|_| invalid())?;
                self.table.readers.revoke(&id);
            }
            Change::Rekey(id, generation, key, record) => {
                if local.is_none() {
                    return Err(invalid());
                }
                self.table
                    .authority
                    .restore_key(&id, generation, key, record)
                    .map_err(|_| invalid())?;
            }
            Change::Create(name, creation, creator) => {
                if !self.table.authority.has_id(&creator) {
                    return Err(invalid());
                }
                self.create(name, creation, limits)?;
                *self.table.counts.entry(creator.clone()).or_default() += 1;
                self.table.creators.insert(creation, creator);
            }
            Change::Remove(creation) => {
                let channel = self.channel(creation)?.clone();
                if !self.table.is_current(&channel) || channel.available().is_err() {
                    return Err(invalid());
                }
                self.table.begin_removal(&channel);
            }
            Change::RemoveDone(creation) => {
                let channel = self.table.removals.get(&creation).ok_or_else(invalid)?;
                self.reservations.remove(&creation);
                self.cuts.remove(&creation);
                channel.history.lock().release_removed_space(&self.quota);
                self.table.finish_removal(creation);
            }
            Change::Reserve(creation, sender, position, day, bytes) => {
                let channel = self.channel(creation)?;
                if !(self.table.authority.has_id(&sender)
                    || crate::upgrade_notice::sent_by_owner(&sender, &channel.name))
                    || self.reservations.contains_key(&creation)
                    || self.cuts.contains(&creation)
                    || channel.history.lock().head() != position.get()
                    || bytes == 0
                {
                    return Err(invalid());
                }
                let reservation = Reservation::restore(&self.quota, &sender, bytes, day)?;
                self.reservations.insert(
                    creation,
                    SavedReservation {
                        sender,
                        position,
                        reservation,
                    },
                );
            }
            Change::Head(mut head) => {
                let creation = head.receipt.creation;
                if !self.runs.contains_key(&head.stamp.run) || self.cuts.contains(&creation) {
                    return Err(invalid());
                }
                let pending = self.reservations.remove(&creation).ok_or_else(invalid)?;
                self.restore_lead_session(&mut head);
                self.channel(creation)?
                    .history
                    .lock()
                    .restore_head(head, pending, &self.quota)?;
            }
            Change::Trim(creation, floor, bytes) => {
                let channel = self.channel(creation)?;
                channel
                    .history
                    .lock()
                    .restore_trim(channel, floor, bytes, &self.quota)?;
            }
            Change::TrimDone(creation) => {
                self.channel(creation)?.history.lock().restore_trim_done()?;
            }
            Change::Cut(creation, head, attempt, tail) => {
                if tail
                    .and_then(|tail| tail.last)
                    .is_some_and(|last| last.get() < head)
                {
                    return Err(invalid());
                }
                self.channel(creation)?
                    .history
                    .lock()
                    .restore_attempt(head, attempt)?;
                if !self.cuts.insert(creation) {
                    return Err(invalid());
                }
            }
            Change::CutDone(creation, released) => {
                if !self.cuts.remove(&creation)
                    || self.reservations.remove(&creation).is_some() != released
                {
                    return Err(invalid());
                }
            }
            Change::Damage(creation, position) => self.channel(creation)?.restore_damage(position),
            Change::Ack(creation, participant, position) => {
                let channel = self.channel(creation)?;
                let mut history = channel.history.lock();
                if !self.table.authority.has_id(&participant) || history.head() <= position.get() {
                    return Err(invalid());
                }
                history.note_ack(channel, &participant, position);
                drop(history);
                self.table
                    .readers
                    .restore(creation, participant, position)?;
            }
            Change::SocketRecord(id, generation, record) => {
                if local.is_none() {
                    return Err(invalid());
                }
                self.table
                    .authority
                    .restore_socket_record(&id, generation, record)
                    .map_err(|_| invalid())?;
            }
            Change::WitnessHanded(at) => {
                if checkpoints.get(&at.sequence) != Some(&at)
                    || at.sequence <= self.published.sequence
                    || self
                        .handed
                        .is_some_and(|older| older.sequence > at.sequence)
                {
                    return Err(invalid());
                }
                self.handed = Some(at);
            }
            Change::WitnessPublished(at, position) => {
                if self.handed != Some(at)
                    || self
                        .positions
                        .back()
                        .is_some_and(|newest| *newest >= position)
                {
                    return Err(invalid());
                }
                self.handed = None;
                self.published = at;
                self.positions.push_back(position);
                if self.positions.len() > crate::witness::RETAINED {
                    self.positions.pop_front();
                }
            }
        }
        Ok(())
    }

    fn create(
        &mut self,
        name: ChannelName,
        creation: CreationId,
        limits: &Limits,
    ) -> Result<(), Error> {
        if self.channels.contains_key(&creation) || self.table.channels.contains_key(&name) {
            return Err(invalid());
        }
        let channel = Arc::new(Channel::new(name.clone(), creation, self.run, limits));
        self.channels.insert(creation, channel.clone());
        self.table.channels.insert(name, channel);
        Ok(())
    }

    pub(super) fn channel(&self, id: CreationId) -> Result<&Arc<Channel>, Error> {
        self.channels
            .get(&id)
            .filter(|channel| !channel.is_removed())
            .ok_or_else(invalid)
    }

    fn confirmed(&mut self) -> (Checkpoint, VecDeque<Position>) {
        (self.published, std::mem::take(&mut self.positions))
    }

    pub(super) fn restore_lead_session(&mut self, head: &mut crate::journal_entry::SavedHead) {
        if head.sender.is_lead() {
            self.lead_session = head.lead_session.take();
        }
    }

    fn finish(mut self, run: [u8; 16]) -> BoardState {
        self.runs.insert(run, self.runs.len());
        let runs = Arc::new(self.runs);
        let participants = Arc::new(self.table.authority.ids());
        let deadline = Instant::now() + STORE_WAIT;
        let mut task_restores = HashMap::new();
        for id in participants.iter() {
            if self.table.authority.active(id).is_ok()
                && let Some(inbox) = self.table.channels.get(&id.inbox())
                && inbox.available().is_ok()
                && let Some(through) = self.table.readers.begin_task_restore(
                    id,
                    inbox.creation,
                    inbox.floor(),
                    deadline,
                )
            {
                task_restores.insert(inbox.creation, (id.clone(), through));
            }
        }
        for (id, channel) in self.channels {
            channel
                .activity
                .readers
                .store(self.table.readers.positions(id).into());
            let mut history = channel.history.lock();
            channel.publish_head(history.head());
            if channel.is_removed() && !self.table.removals.contains_key(&id) {
                continue;
            }
            let check = Reopen::new(
                runs.clone(),
                participants.clone(),
                self.reservations.remove(&id),
                self.cuts.contains(&id),
                task_restores.remove(&id),
            );
            history.need_reopen(run, check);
        }
        BoardState {
            table: self.table,
            pending_repair: self.repair.map(|plan| plan.number.to_string()),
            lead_session: self.lead_session,
            run,
            quota: self.quota,
        }
    }
}

fn invalid() -> Error {
    Error::unavailable("invalid saved board state")
}
