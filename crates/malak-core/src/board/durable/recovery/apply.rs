use super::*;
use crate::record_fields;
use malak_journal::{Journal, Save};
use malak_layers::{Claim, Claimed, Trim, Trimmed};
use std::time::UNIX_EPOCH;

impl Owner {
    /// Apply an approved plan to the stopped board's original, attested store.
    ///
    /// An accepted plan records progress before each destructive store change.
    /// Retry with the same number to continue after interruption.
    #[doc(hidden)]
    pub fn apply_recovery(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
        mechanism: Option<&str>,
        number: &str,
    ) -> Result<RecoveryReport, Error> {
        apply(store, limits, state.as_ref(), mechanism, number, false)
    }

    /// Apply or continue a repair with the owner's explicit volume confirmation.
    ///
    /// The owner confirms that the volume has not been restored from an image
    /// or copied. The board still verifies its saved filesystem and directory
    /// identities. A store configured for host attestation needs valid host
    /// evidence.
    #[doc(hidden)]
    pub fn apply_recovery_with_owner_confirmation(
        store: impl Store + 'static,
        limits: Limits,
        state: impl AsRef<Path>,
        mechanism: Option<&str>,
        number: &str,
    ) -> Result<RecoveryReport, Error> {
        apply(store, limits, state.as_ref(), mechanism, number, true)
    }

    /// Withdraw the unfinished channels of an accepted repair plan.
    ///
    /// This operation only opens the private journal. Any begun channels keep
    /// their raised attempts and every unfinished channel remains stopped.
    #[doc(hidden)]
    pub fn withdraw_recovery(
        limits: Limits,
        state: impl AsRef<Path>,
        mechanism: Option<&str>,
        number: &str,
    ) -> Result<RecoveryReport, Error> {
        let number: PlanNumber = number.parse()?;
        let loaded = load(state.as_ref(), &limits)?;
        let mut replay = replay(&loaded, &limits, mechanism)?;
        let mut plan = replay.require_plan(number)?.clone();
        let mut journal = activate(loaded)?;
        save(&mut journal, &mut replay, RepairChange::Withdraw(number))?;
        for item in &mut plan.channels {
            if item.phase.unfinished() {
                item.phase = Phase::Withdrawn;
            }
        }
        Ok(RecoveryReport::progress(plan))
    }
}

fn apply(
    store: impl Store + 'static,
    limits: Limits,
    state: &Path,
    mechanism: Option<&str>,
    number: &str,
    owner_confirmation: bool,
) -> Result<RecoveryReport, Error> {
    let number: PlanNumber = number.parse()?;
    let loaded = load(state, &limits)?;
    let mut replay = replay(&loaded, &limits, mechanism)?;
    let workers = Arc::new(Workers::default());
    let (store, claim) = StoreLease::repair(store, workers.clone())?;
    let evidence = Evidence::new(&loaded, &replay, &store, &claim, &workers, number.factor)?;
    let place = evidence.applicable_place(owner_confirmation)?;
    let (plan, accepting) = match &replay.repair {
        Some(_) => {
            let plan = replay.require_plan(number)?.clone();
            evidence.approved(&plan, &place)?;
            (plan, false)
        }
        None => {
            let proposal = evidence.proposal()?;
            if proposal.number != number {
                return Err(Error::refused("the recovery plan has changed; look again"));
            }
            (accepted(proposal, place)?, true)
        }
    };
    let key = evidence.key;
    let mut journal = activate(loaded)?;
    if accepting {
        save(
            &mut journal,
            &mut replay,
            RepairChange::Accept(plan.clone()),
        )?;
        #[cfg(test)]
        crate::durable_kill::hit("repair-accepted");
    }
    let progress = carry(&mut journal, &mut replay, &store, &workers, &key, plan)?;
    Ok(RecoveryReport::progress(progress))
}

pub(in crate::board::durable) fn resume_at_start(
    loaded: Start,
    mut replay: Replay,
    store: Arc<dyn Store>,
    workers: &Arc<Workers>,
    limits: &Limits,
    local: Option<&LocalReplay<'_>>,
) -> Result<(Start, Replay), Error> {
    let Some(plan) = replay.repair.clone() else {
        return Ok((loaded, replay));
    };
    let Ok((lease, claim)) = StoreLease::repair_shared(store, workers.clone()) else {
        return Ok((loaded, replay));
    };
    let evidence = Evidence::new(
        &loaded,
        &replay,
        &lease,
        &claim,
        workers,
        plan.number.factor,
    )
    .and_then(|evidence| {
        let place = evidence.applicable_place(false)?;
        evidence.approved(&plan, &place)?;
        Ok(evidence.key)
    });
    let Ok(key) = evidence else {
        return Ok((loaded, replay));
    };
    let mut journal = activate(loaded)?;
    // Operational errors leave the plan in the journal. Reload the accepted
    // prefix before the ordinary start so partially applied memory is discarded.
    let _ = carry(&mut journal, &mut replay, &lease, workers, &key, plan);
    drop(lease);
    let inspected = journal
        .inspect()
        .map_err(|error| Error::unavailable(format!("board state after repair: {error}")))?;
    let loaded = Start::load(inspected)?;
    let replay = Replay::load(&loaded, limits, local)?;
    Ok((loaded, replay))
}

impl Evidence<'_> {
    fn applicable_place(&self, owner_confirmation: bool) -> Result<AcceptedPlace, Error> {
        if !self.saved_location_matches() {
            return Err(Error::refused(
                "the store's filesystem ID or folder number has changed",
            ));
        }
        if self.claim.place == StorePlace::OwnerConfirmation && self.keys.location.is_none() {
            return Err(Error::refused("the board has no saved local store place"));
        }
        match self.place() {
            Some(place @ AcceptedPlace::Host(_)) => Ok(place),
            Some(place @ AcceptedPlace::Local(_)) => {
                if !owner_confirmation {
                    return Err(Error::refused(
                        "local store repair requires --owner-vouches-same-volume",
                    ));
                }
                Ok(place)
            }
            None => Err(Error::refused(
                "the host has not established the store's place",
            )),
        }
    }

    fn approved(&self, plan: &AcceptedPlan, place: &AcceptedPlace) -> Result<(), Error> {
        if self.claim.ownership.store_id != plan.store_id || place != &plan.place {
            return Err(Error::refused(
                "repair store ID or attested place has changed",
            ));
        }
        self.look(false).map(|_| ())
    }
}

fn accepted(proposal: RecoveryPlan, place: AcceptedPlace) -> Result<AcceptedPlan, Error> {
    let channels: Vec<_> = proposal
        .channels
        .into_iter()
        .filter(|item| item.conflicts.is_empty())
        .map(|item| PlannedChannel {
            name: item.name,
            creation: item.creation,
            input: item.input,
            head: item.head,
            new_head: item.new_head,
            kept: item.keep,
            voids: item.lost.into_iter().map(|loss| loss.range).collect(),
            phase: Phase::Pending,
        })
        .collect();
    if channels.is_empty() {
        return Err(Error::refused(
            "the plan has no channels that can be repaired",
        ));
    }
    Ok(AcceptedPlan {
        number: proposal.number,
        store_id: proposal.store_id,
        place,
        channels,
    })
}

pub(super) fn carry(
    journal: &mut Journal,
    replay: &mut Replay,
    store: &StoreLease,
    workers: &Workers,
    key: &RecordKey,
    mut plan: AcceptedPlan,
) -> Result<AcceptedPlan, Error> {
    for item in &mut plan.channels {
        if !item.phase.unfinished() {
            continue;
        }
        let creation = item.creation;
        let channel = replay.channel(creation)?.clone();
        #[cfg(test)]
        let checkpoint = |step| crate::durable_kill::repair_hit(creation, step);
        if item.phase == Phase::Pending {
            #[cfg(test)]
            checkpoint("input-before");
            let current = scan(store, workers, &channel.store_name, plan.number.factor)?;
            #[cfg(test)]
            checkpoint("input-after");
            if current.digest != item.input {
                save(
                    journal,
                    replay,
                    RepairChange::WithdrawChannel(plan.number, creation),
                )?;
                item.phase = Phase::Withdrawn;
                continue;
            }
            let attempt = channel
                .history
                .lock()
                .attempt()
                .checked_add(1)
                .ok_or_else(|| Error::unavailable("channel attempts exhausted"))?;
            save(
                journal,
                replay,
                RepairChange::Begin(plan.number, creation, attempt),
            )?;
            item.phase = Phase::Begun;
            #[cfg(test)]
            checkpoint("begun");
        }

        #[cfg(test)]
        checkpoint("rewrite-before");
        let trimmed = rewrite(store, workers, &channel.store_name, plan.number, item)?;
        #[cfg(test)]
        checkpoint("rewrite-after");
        if trimmed == Trimmed::ChangedInput {
            save(
                journal,
                replay,
                RepairChange::WithdrawChannel(plan.number, creation),
            )?;
            item.phase = Phase::Withdrawn;
            continue;
        }
        settle(store, workers, &channel.store_name)?;
        #[cfg(test)]
        checkpoint("settled");
        let result = scan(store, workers, &channel.store_name, plan.number.factor)?;
        #[cfg(test)]
        checkpoint("read-back");
        let Some(adopted) = validate_rewrite(replay, key, &channel, item, &result) else {
            save(
                journal,
                replay,
                RepairChange::WithdrawChannel(plan.number, creation),
            )?;
            item.phase = Phase::Withdrawn;
            continue;
        };
        save(
            journal,
            replay,
            RepairChange::Finish(plan.number, creation, adopted),
        )?;
        item.phase = Phase::Finished;
        #[cfg(test)]
        checkpoint("finished");
    }
    Ok(plan)
}

fn rewrite(
    store: &StoreLease,
    workers: &Workers,
    channel: &str,
    number: PlanNumber,
    item: &PlannedChannel,
) -> Result<Trimmed, Error> {
    let generation = store.generation;
    let store = store.store.clone();
    let channel = channel.to_owned();
    let input = item.input;
    let keep = item.kept.clone();
    workers.until(Instant::now() + STORE_WAIT, move || {
        layer(store.trim(
            generation,
            &channel,
            Trim::Repair {
                plan: number.digest,
                input,
                keep: &keep,
            },
        ))
    })
}

fn settle(store: &StoreLease, workers: &Workers, channel: &str) -> Result<(), Error> {
    let generation = store.generation;
    let store = store.store.clone();
    let channel = channel.to_owned();
    workers.until(Instant::now() + STORE_WAIT, move || {
        if !matches!(
            layer(store.claim(Claim::Settle {
                generation,
                channel: &channel
            }))?,
            Claimed::Done
        ) {
            return Err(Error::unavailable(
                "store did not settle the repaired channel",
            ));
        }
        Ok(())
    })
}

fn validate_rewrite(
    replay: &Replay,
    key: &RecordKey,
    channel: &Channel,
    item: &PlannedChannel,
    result: &Scan,
) -> Option<Vec<SavedHead>> {
    if !result.exact || result.candidates.len() != item.kept.len() {
        return None;
    }
    let history = channel.history.lock();
    let view = history.journal_view(channel);
    let prior_attempt = history.attempt().checked_sub(1)?;
    let minimum = replay.runs[&history.latest_signed_run()];
    let mut bytes_kept = history.restored_repair_bytes(item);
    let mut adopted = Vec::new();
    let mut adopted_ids = BTreeSet::new();
    for (candidate, approved) in result.candidates.iter().zip(&item.kept) {
        if candidate.record.position != approved.position
            || Sha256::digest(&candidate.encoded).as_slice() != approved.digest
        {
            return None;
        }
        let verified = key.verify(&channel.store_name, &candidate.record)?;
        if approved.position.get() < item.head {
            if !view.names(approved.position, verified.stamp) {
                return None;
            }
            continue;
        }
        if verified.stamp.attempt != prior_attempt
            || *replay.runs.get(&verified.stamp.run)? < minimum
        {
            return None;
        }
        let message =
            record_fields::decode(&channel.name, channel.creation, &candidate.record, verified)?;
        let digest = record_fields::restored_retry_digest(verified, &message)?;
        if !(replay.table.authority.has_id(&message.sender)
            || crate::upgrade_notice::sent_by_owner(&message.sender, &channel.name))
            || history.has_retry(&message.sender, &message.id)
            || !adopted_ids.insert((message.sender.clone(), message.id.clone()))
        {
            return None;
        }
        let bytes = candidate.record.data.len() as u64;
        bytes_kept = bytes_kept.checked_add(bytes)?;
        adopted.push(SavedHead {
            lead_session: message.lead_session(),
            receipt: message.receipt,
            stamp: verified.stamp,
            kept: bytes_kept,
            bytes,
            day: message.sent_at.duration_since(UNIX_EPOCH).ok()?.as_secs() / 86_400,
            sender: message.sender,
            id: message.id,
            digest,
        });
    }
    Some(adopted)
}

pub(super) fn activate(loaded: Start) -> Result<Journal, Error> {
    loaded
        .inspected
        .activate()
        .map_err(|error| Error::unavailable(format!("board journal: {error}")))
}

fn save(journal: &mut Journal, replay: &mut Replay, change: RepairChange) -> Result<(), Error> {
    let bytes = change.encode();
    replay.apply_repair(change)?;
    journal
        .append(&bytes, Save::OnDisk)
        .map_err(|error| Error::unavailable(format!("board repair journal: {error}")))?;
    Ok(())
}
