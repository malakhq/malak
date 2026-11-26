//! The stopped-board recovery workflow and its store evidence checks.

use super::Start;
use super::restart::{LocalReplay, Replay};
use crate::channel::{Channel, History};
use crate::journal_entry::{
    AcceptedPlace, AcceptedPlan, Phase, PlanNumber, PlannedChannel, RepairChange, SavedHead,
};
use crate::storage::{RepairClaim, STORE_WAIT, StoreLease, layer};
use crate::witness;
use crate::workers::Workers;
use crate::{ChannelName, CreationId, Error, Limits, Owner, ParticipantId, Position};
use malak_journal::{Checkpoint, StateFolder};
use malak_layers::{Candidate, KeptRecord, ReadLimit, ReadResult, Store, StorePlace};
use malak_record::RecordKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write};
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

mod apply;
mod progress;
mod report;
pub(super) use apply::resume_at_start;
pub use report::RecoveryReport;
use report::{ChannelReport, Loss, LossReason, RecoveryPlan};

impl Owner {
    /// Inspect a stopped board and describe a repair plan without changing its history.
    ///
    /// Supply the configured mechanism for a Unix board or `None` for Direct.
    /// The work factor bounds checked candidate bytes per channel relative to
    /// its size. The resulting plan is bound to that factor and to the store
    /// ID and its host-attested or saved local place, if available.
    #[doc(hidden)]
    pub fn look_recovery(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
        mechanism: Option<&str>,
        work_factor: u64,
    ) -> Result<RecoveryReport, Error> {
        if work_factor == 0 {
            return Err(Error::refused("recovery work factor must be positive"));
        }
        let loaded = load(state.as_ref(), &limits)?;
        let replay = replay(&loaded, &limits, mechanism)?;
        if let Some(plan) = &replay.repair {
            return Ok(RecoveryReport::progress(plan.clone()));
        }
        let workers = Arc::new(Workers::default());
        let (store, claim) = StoreLease::repair(store, workers.clone())?;
        let evidence = Evidence::new(&loaded, &replay, &store, &claim, &workers, work_factor)?;
        Ok(RecoveryReport::proposal(evidence.proposal()?))
    }
}

fn load(state: &Path, limits: &Limits) -> Result<Start, Error> {
    super::validate_start(limits)?;
    let inspected = StateFolder::acquire_existing(state)
        .and_then(StateFolder::inspect)
        .map_err(|error| Error::unavailable(format!("board state: {error}")))?;
    Start::load(inspected)
}

fn replay(loaded: &Start, limits: &Limits, mechanism: Option<&str>) -> Result<Replay, Error> {
    let local = if let Some(lead_uid) = loaded.lead_uid() {
        let mechanism =
            mechanism.ok_or_else(|| Error::refused("the saved board needs its local mechanism"))?;
        Some(LocalReplay {
            lead_uid,
            mechanism,
        })
    } else {
        None
    };
    Replay::load(loaded, limits, local.as_ref())
}

struct Evidence<'a> {
    loaded: &'a Start,
    replay: &'a Replay,
    store: &'a StoreLease,
    claim: &'a RepairClaim,
    workers: &'a Workers,
    keys: &'a crate::journal_entry::SavedKeys,
    key: RecordKey,
    factor: u64,
}

impl<'a> Evidence<'a> {
    fn new(
        loaded: &'a Start,
        replay: &'a Replay,
        store: &'a StoreLease,
        claim: &'a RepairClaim,
        workers: &'a Workers,
        factor: u64,
    ) -> Result<Self, Error> {
        let keys = loaded
            .keys
            .as_ref()
            .ok_or_else(|| Error::refused("board has no saved keys"))?;
        if keys.store != claim.ownership.store_id {
            return Err(Error::refused("the saved board names a different store"));
        }
        Ok(Self {
            loaded,
            replay,
            store,
            claim,
            workers,
            keys,
            key: RecordKey::new(&keys.record),
            factor,
        })
    }

    fn proposal(&self) -> Result<RecoveryPlan, Error> {
        Ok(RecoveryPlan::new(
            self.loaded.inspected.loaded().head,
            self.claim.ownership.store_id,
            self.place(),
            self.factor,
            self.look(true)?,
        ))
    }

    fn saved_location_matches(&self) -> bool {
        self.keys
            .location
            .as_ref()
            .is_none_or(|saved| self.claim.ownership.location.as_ref() == Some(saved))
    }

    fn place(&self) -> Option<AcceptedPlace> {
        if !self.saved_location_matches() {
            return None;
        }
        match &self.claim.place {
            StorePlace::Established(bytes) if !bytes.is_empty() => {
                Some(AcceptedPlace::Host(bytes.clone()))
            }
            StorePlace::OwnerConfirmation => self.keys.location.clone().map(AcceptedPlace::Local),
            _ => None,
        }
    }

    fn look(&self, require_proof: bool) -> Result<Vec<ChannelReport>, Error> {
        let witness = self.read(witness::CHANNEL)?;
        let mut newest: Option<Checkpoint> = None;
        let mut conflict = false;
        for candidate in &witness.candidates {
            if let Some(at) = witness::decode(&self.key, &candidate.record.data) {
                if newest.is_none_or(|previous| at.sequence > previous.sequence) {
                    newest = Some(at);
                    conflict = false;
                } else if newest.is_some_and(|prior| prior.sequence == at.sequence && prior != at) {
                    conflict = true;
                }
            }
        }
        witness::check_checkpoint(&self.loaded.inspected, newest, conflict)?;
        let mut verified_any = newest.is_some();
        let mut has_bytes = self.claim.ownership.has_records || witness.bytes != 0;
        let known: BTreeMap<_, _> = self
            .replay
            .channels
            .values()
            .map(|channel| (channel.store_name.as_str(), channel))
            .collect();
        let mut names: BTreeSet<_> = known.keys().copied().collect();
        names.extend(self.claim.channels.iter().map(String::as_str));
        names.remove(witness::CHANNEL);

        let mut reports = Vec::new();
        for name in names {
            let scan = self.read(name)?;
            has_bytes |= scan.bytes != 0;
            let channel = known.get(name).copied();
            let history = channel.map(|channel| channel.history.lock());
            let removed = channel.is_some_and(|channel| channel.is_removed());
            let deleting = channel
                .is_some_and(|channel| self.replay.table.removals.contains_key(&channel.creation));
            let stopped = require_proof
                && !removed
                && channel.is_some_and(|channel| channel.available().is_err());
            let mut stamps = stopped.then(Vec::new);
            for candidate in &scan.candidates {
                let stamp = self
                    .key
                    .verify(name, &candidate.record)
                    .map(|verified| verified.stamp);
                if let Some(stamp) = stamp {
                    if (!removed || deleting)
                        && (!self.replay.runs.contains_key(&stamp.run)
                            || history
                                .as_ref()
                                .is_none_or(|history| stamp.attempt > history.attempt()))
                    {
                        return Err(Error::refused(
                            "a verifying record shows the state folder is older than the store",
                        ));
                    }
                    verified_any = true;
                }
                if let Some(stamps) = &mut stamps {
                    stamps.push(stamp);
                }
            }
            if let (Some(channel), Some(history), Some(stamps)) = (channel, history, stamps) {
                reports.push(self.classify(channel, &history, &scan, stamps));
            }
        }
        if require_proof && has_bytes && !verified_any {
            return Err(Error::refused(
                "no store record verifies under the board's key",
            ));
        }
        Ok(reports)
    }

    fn read(&self, name: &str) -> Result<Scan, Error> {
        scan(self.store, self.workers, name, self.factor)
    }

    fn classify(
        &self,
        channel: &Channel,
        history: &History,
        scan: &Scan,
        stamps: Vec<Option<malak_record::Stamp>>,
    ) -> ChannelReport {
        let view = history.journal_view(channel);
        let head = view.retained.end;
        let minimum_run = self.replay.runs[&history.latest_signed_run()];
        let mut positions = BTreeMap::<u64, Slot<'_>>::new();
        let mut authentic = Vec::new();
        for (candidate, stamp) in scan.candidates.iter().zip(stamps) {
            let position = candidate.record.position;
            let at = position.get();
            if stamp.is_some() {
                authentic.push(kept(candidate));
            }
            if at < view.retained.start || view.voids.iter().any(|span| span.contains(&at)) {
                continue;
            }
            let slot = positions.entry(at).or_default();
            match stamp {
                Some(stamp)
                    if view.names(position, stamp)
                        || (at >= head
                            && stamp.attempt == history.attempt()
                            && self.replay.runs[&stamp.run] >= minimum_run) =>
                {
                    slot.include(candidate);
                }
                Some(_) => slot.wrong_stamp = true,
                None => slot.invalid = true,
            }
        }

        let mut keep = Vec::new();
        let mut lost = Vec::new();
        let mut conflicts = Vec::new();
        let mut members = positions.range(view.retained.clone()).peekable();
        let mut at = view.retained.start;
        for boundary in view
            .voids
            .iter()
            .cloned()
            .chain(std::iter::once(head..head))
        {
            if boundary.end <= at && at < head {
                continue;
            }
            let end = boundary.start.min(head);
            while let Some((&position, slot)) = members.next_if(|(position, _)| **position < end) {
                push_loss(&mut lost, at..position, LossReason::Missing);
                if slot.conflict {
                    conflicts.push(position);
                } else if let Some(candidate) = slot.chosen {
                    keep.push(kept(candidate));
                } else {
                    push_loss(&mut lost, position..position + 1, slot.reason());
                }
                at = position + 1;
            }
            push_loss(&mut lost, at..end, LossReason::Missing);
            at = at.max(boundary.end);
        }
        let mut new_head = head;
        for (&position, slot) in positions.range(head..) {
            if position != new_head {
                break;
            }
            if slot.conflict {
                conflicts.push(position);
                break;
            }
            let Some(candidate) = slot.chosen else { break };
            keep.push(kept(candidate));
            new_head += 1;
        }

        let mut selected = keep
            .iter()
            .map(|record| (record.position, record.digest))
            .collect::<BTreeSet<_>>();
        let mut set_aside = authentic
            .into_iter()
            .filter(|record| !selected.remove(&(record.position, record.digest)))
            .map(|record| record.position.get())
            .collect::<Vec<_>>();
        set_aside.sort_unstable();
        let behind = lost.last().map_or_else(Vec::new, |last| {
            self.replay
                .table
                .readers
                .behind(channel.creation, last.range.end - 1)
        });
        let known_lost = history
            .retry_positions()
            .filter(|position| {
                let at = position.get();
                let index = lost.partition_point(|loss| loss.range.end <= at);
                lost.get(index).is_some_and(|loss| loss.range.contains(&at))
            })
            .count() as u64;
        ChannelReport {
            name: channel.name.clone(),
            creation: channel.creation,
            head,
            new_head,
            keep,
            lost,
            conflicts,
            behind,
            known_lost,
            set_aside,
            bytes: scan.bytes,
            input: scan.digest,
        }
    }
}

struct Scan {
    candidates: Vec<Candidate>,
    bytes: u64,
    digest: [u8; 32],
    exact: bool,
}

fn scan(store: &StoreLease, workers: &Workers, name: &str, factor: u64) -> Result<Scan, Error> {
    let store = store.store.clone();
    let name = name.to_owned();
    let result = workers
        .until(Instant::now() + STORE_WAIT, move || {
            layer(store.read(
                &name,
                Position::FIRST,
                ReadLimit {
                    records: 0,
                    before: None,
                },
            ))
        })
        .map_err(|error| Error::unavailable(format!("recovery look unfinished: {error}")))?;
    let ReadResult::Repair {
        candidates,
        bytes,
        digest,
        exact,
    } = result
    else {
        return Err(Error::unavailable(
            "recovery look unfinished: expected repair candidates",
        ));
    };
    let checked: u128 = candidates
        .iter()
        .map(|candidate| candidate.encoded.len() as u128)
        .sum();
    if checked > u128::from(bytes) * u128::from(factor) {
        return Err(Error::unavailable(
            "recovery look unfinished: work bound exceeded",
        ));
    }
    Ok(Scan {
        candidates,
        bytes,
        digest,
        exact,
    })
}

#[derive(Default)]
struct Slot<'a> {
    chosen: Option<&'a Candidate>,
    conflict: bool,
    wrong_stamp: bool,
    invalid: bool,
}

impl<'a> Slot<'a> {
    fn include(&mut self, candidate: &'a Candidate) {
        self.conflict |= self
            .chosen
            .is_some_and(|existing| existing.record.data != candidate.record.data);
        self.chosen.get_or_insert(candidate);
    }

    fn reason(&self) -> LossReason {
        if self.wrong_stamp {
            LossReason::WrongStamp
        } else if self.invalid {
            LossReason::Invalid
        } else {
            LossReason::Missing
        }
    }
}

fn kept(candidate: &Candidate) -> KeptRecord {
    KeptRecord {
        position: candidate.record.position,
        digest: Sha256::digest(&candidate.encoded).into(),
    }
}

fn push_loss(lost: &mut Vec<Loss>, range: Range<u64>, reason: LossReason) {
    if range.is_empty() {
        return;
    }
    if let Some(last) = lost.last_mut()
        && last.reason == reason
        && last.range.end == range.start
    {
        last.range.end = range.end;
    } else {
        lost.push(Loss { range, reason });
    }
}
