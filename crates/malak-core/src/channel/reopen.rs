use super::*;
use crate::journal_entry::SavedHead;
use crate::readers::WorkingTask;
use crate::record_fields;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};

const PAGE: usize = 32;

pub(crate) struct Reopen {
    runs: Arc<HashMap<[u8; 16], usize>>,
    participants: Arc<HashSet<ParticipantId>>,
    reservation: Option<SavedReservation>,
    checked: bool,
    first_run: [u8; 16],
    unfinished_cut: bool,
    task_restore: Option<(ParticipantId, u64)>,
}

pub(crate) struct SavedReservation {
    pub(crate) sender: ParticipantId,
    pub(crate) position: Position,
    pub(crate) reservation: Reservation,
}

impl Reopen {
    pub(crate) fn new(
        runs: Arc<HashMap<[u8; 16], usize>>,
        participants: Arc<HashSet<ParticipantId>>,
        reservation: Option<SavedReservation>,
        unfinished_cut: bool,
        task_restore: Option<(ParticipantId, u64)>,
    ) -> Self {
        Self {
            runs,
            participants,
            reservation,
            unfinished_cut,
            task_restore,
            checked: false,
            first_run: [0; 16],
        }
    }
}

impl History {
    pub(crate) fn restore_head(
        &mut self,
        saved: SavedHead,
        pending: SavedReservation,
        quota: &Mutex<Quota>,
    ) -> Result<(), Error> {
        self.restore_head_at(saved, pending, quota, self.stamp.attempt)
    }

    pub(super) fn restore_head_at(
        &mut self,
        saved: SavedHead,
        pending: SavedReservation,
        quota: &Mutex<Quota>,
        attempt: u64,
    ) -> Result<(), Error> {
        if !self.matches_saved_head(&saved, &pending, attempt) {
            return Err(invalid());
        }
        let retry_bytes = retry_bytes(&saved.sender, &saved.id, saved.receipt.trace);
        let extra =
            saved.bytes.checked_add(retry_bytes).ok_or_else(invalid)? - pending.reservation.bytes();
        // An adopted pre-release record may have reserved space before trace
        // bytes were retained in receipts. Charge the additional bytes on replay.
        if extra != 0 {
            pending.reservation.restore_extra(extra, saved.day)?;
        }
        if self
            .stamp_ranges
            .last()
            .is_none_or(|&(_, stamp)| stamp != saved.stamp)
        {
            self.stamp_ranges
                .push((saved.receipt.position, saved.stamp));
        }
        self.bytes_kept = saved.kept;
        self.keep_record(&saved.sender, saved.bytes, Instant::now());
        pending.reservation.commit(saved.day);
        self.remember_retry(
            saved.sender,
            saved.id,
            Retry {
                receipt: saved.receipt,
                digest: saved.digest,
                bytes: retry_bytes,
            },
            quota,
        );
        Ok(())
    }

    fn matches_saved_head(
        &self,
        saved: &SavedHead,
        pending: &SavedReservation,
        attempt: u64,
    ) -> bool {
        let retry_bytes = retry_bytes(&saved.sender, &saved.id, None);
        let traced_bytes = retry_bytes + saved.receipt.trace.map_or(0, crate::TraceId::len) as u64;
        let reserved = Some(pending.reservation.bytes());
        let size_matches = saved.bytes.checked_add(traced_bytes) == reserved
            || saved.bytes.checked_add(retry_bytes) == reserved;
        !(self.head != saved.receipt.position.get()
            || pending.position != saved.receipt.position
            || pending.sender != saved.sender
            || !size_matches
            || self.bytes_kept.checked_add(saved.bytes) != Some(saved.kept)
            || saved.stamp.attempt != attempt
            || self
                .retries
                .get(&saved.sender)
                .is_some_and(|entries| entries.entries.contains_key(&saved.id)))
    }

    pub(crate) fn restore_trim(
        &mut self,
        channel: &Channel,
        floor: u64,
        kept: u64,
        quota: &Mutex<Quota>,
    ) -> Result<(), Error> {
        if !(self.floor..=self.head).contains(&floor) {
            return Err(invalid());
        }
        let count = (floor - self.floor) as usize;
        let dropped: u64 = self
            .retained
            .iter()
            .take(count)
            .map(|item| item.bytes)
            .sum();
        if self.bytes_kept.checked_sub(dropped) != Some(kept) {
            return Err(invalid());
        }
        self.release_prefix(count, &mut quota.lock());
        self.floor = floor;
        self.bytes_kept = kept;
        channel.floor.store(floor, Ordering::Release);
        self.trim_pending = true;
        Ok(())
    }

    pub(crate) fn restore_trim_done(&mut self) -> Result<(), Error> {
        if !self.trim_pending {
            return Err(invalid());
        }
        self.trim_pending = false;
        Ok(())
    }

    pub(crate) fn restore_attempt(&mut self, head: u64, attempt: u64) -> Result<(), Error> {
        if self.head != head || self.stamp.attempt.checked_add(1) != Some(attempt) {
            return Err(invalid());
        }
        self.stamp.attempt = attempt;
        Ok(())
    }

    pub(crate) fn need_reopen(&mut self, run: [u8; 16], mut check: Reopen) {
        check.first_run = self.stamp.run;
        self.stamp.run = run;
        self.reopen = Some(check);
    }

    pub(crate) fn needs_reopen(&self) -> bool {
        self.reopen.as_ref().is_some_and(|check| !check.checked)
    }

    pub(crate) fn take_task_restore(&mut self) -> Option<(ParticipantId, u64)> {
        self.reopen.as_mut()?.task_restore.take()
    }

    pub(crate) fn check_reopen(
        &mut self,
        channel: &Channel,
        store: &StoreLease,
        key: &RecordKey,
        quota: &Arc<Mutex<Quota>>,
        restore_through: Option<u64>,
    ) -> Result<Option<WorkingTask>, Error> {
        let Some(check) = self.reopen.as_ref().filter(|check| !check.checked) else {
            return Ok(None);
        };
        store.running()?;
        let mut tail = self.inspect_tail(channel, store, key)?;
        if let Err(error) = channel.available() {
            self.reopen.as_mut().expect("reopening channel").checked = true;
            return Err(error);
        }
        let retained = malak_record::History {
            channel: &channel.store_name,
            retained: self.floor..self.head,
            stamps: &self.stamp_ranges,
            voids: &self.voids,
        };
        let now = Instant::now();
        let wall = SystemTime::now();
        let mut position = self.floor;
        let mut newest_task = None;
        while position < self.head {
            let index = self.voids.partition_point(|range| range.end <= position);
            let mut limit = self.head;
            if let Some(void) = self.voids.get(index) {
                if void.contains(&position) {
                    position = void.end;
                    continue;
                }
                limit = limit.min(void.start);
            }
            let exclusive = Position::try_from(limit).ok();
            let from = Position::try_from(position).map_err(|_| invalid())?;
            let (records, _) = read_page(store, channel, from, exclusive)?;
            let mut records = records.into_iter();
            for _ in 0..PAGE.min((limit - position) as usize) {
                let at = Position::try_from(position).map_err(|_| invalid())?;
                let Some(record) = records.next().filter(|record| record.position == at) else {
                    return Err(self.damage(channel, store, at));
                };
                let verified = key.verify(&channel.store_name, &record);
                self.check_restored_stamp(store, verified)?;
                let Some(message) = verified
                    .filter(|record| retained.names(at, record.stamp))
                    .and_then(|verified| {
                        record_fields::decode(&channel.name, channel.creation, &record, verified)
                    })
                else {
                    return Err(self.damage(channel, store, at));
                };
                let age = wall
                    .duration_since(message.sent_at)
                    .unwrap_or(Duration::ZERO);
                self.retained[(position - self.floor) as usize].stored =
                    now.checked_sub(age).unwrap_or(now);
                if restore_through.is_some_and(|through| position <= through)
                    && let Some(task) = WorkingTask::from_message(&message)
                {
                    newest_task = Some(task);
                }
                position += 1;
            }
        }
        if check.unfinished_cut {
            self.inspect_tail(channel, store, key)?;
            self.complete_saved_cut(channel, store)?;
            tail = self.inspect_tail(channel, store, key)?;
        }
        if self.trim_pending {
            self.trim(channel, store, quota, Some(self.floor))?;
        } else if self.floor > 1 {
            let before = Position::try_from(self.floor).ok();
            let (older, _) = read_page(store, channel, Position::try_from(1).unwrap(), before)?;
            if !older.is_empty() {
                self.trim_store_before(channel, store)?;
            }
        }
        while self.adopt(channel, store, key, quota, &tail)? {
            tail = self.inspect_tail(channel, store, key)?;
        }
        if tail.observed.has_bytes()
            || self
                .reopen
                .as_ref()
                .is_some_and(|check| check.reservation.is_some())
        {
            let before_cut = self.inspect_tail(channel, store, key)?;
            if before_cut.first != tail.first {
                return Err(unfinished());
            }
            let observed = before_cut.observed;
            self.begin_cut(channel, store, observed.has_bytes().then_some(observed))?;
            self.reopen
                .as_mut()
                .expect("reopening channel")
                .unfinished_cut = true;
            self.complete_saved_cut(channel, store)?;
            if observed.has_bytes()
                && let Ok(position) = Position::try_from(self.head)
            {
                store
                    .events
                    .tail_cut(&channel.name, channel.creation, position, observed.tampered);
            }
        }
        self.reopen.as_mut().expect("reopening channel").checked = true;
        Ok(newest_task)
    }

    fn complete_saved_cut(&mut self, channel: &Channel, store: &StoreLease) -> Result<(), Error> {
        let released = self
            .reopen
            .as_ref()
            .expect("reopening channel")
            .reservation
            .is_some();
        self.finish_cut(channel, store, released)?;
        let check = self.reopen.as_mut().expect("reopening channel");
        check.unfinished_cut = false;
        check.reservation.take();
        Ok(())
    }

    pub(crate) fn check_removal_tail(
        &self,
        channel: &Channel,
        store: &StoreLease,
        key: &RecordKey,
    ) -> Result<(), Error> {
        if self.reopen.is_some() {
            self.inspect_tail(channel, store, key)?;
        }
        Ok(())
    }

    fn inspect_tail(
        &self,
        channel: &Channel,
        store: &StoreLease,
        key: &RecordKey,
    ) -> Result<Tail, Error> {
        let mut tail = Tail::default();
        let Ok(mut from) = Position::try_from(self.head) else {
            return Ok(tail);
        };
        loop {
            let (records, torn) = read_page(store, channel, from, None)?;
            let end = records.last().map(|record| record.position);
            let more = records.len() == PAGE && !torn;
            tail.observed.unparseable |= torn;
            tail.observed.last = end.or(tail.observed.last);
            for record in records {
                let verified = key.verify(&channel.store_name, &record);
                self.check_restored_stamp(store, verified)?;
                if let Some(valid) = verified {
                    tail.observed.tampered |= !self.adoptable_stamp(valid.stamp);
                    if record.position.get() == self.head {
                        tail.first = Some(record);
                    }
                }
            }
            if !more {
                return Ok(tail);
            }
            let Ok(next) = Position::try_from(end.expect("nonempty page").get() + 1) else {
                return Ok(tail);
            };
            from = next;
        }
    }

    fn adoptable_stamp(&self, stamp: Stamp) -> bool {
        let check = self.reopen.as_ref().expect("reopening channel");
        let latest = self
            .stamp_ranges
            .last()
            .map_or(check.first_run, |(_, stamp)| stamp.run);
        stamp.attempt == self.stamp.attempt && check.runs[&stamp.run] >= check.runs[&latest]
    }

    fn adopt(
        &mut self,
        channel: &Channel,
        store: &StoreLease,
        key: &RecordKey,
        quota: &Arc<Mutex<Quota>>,
        tail: &Tail,
    ) -> Result<bool, Error> {
        let Some(first) = &tail.first else {
            return Ok(false);
        };
        let verified = key
            .verify(&channel.store_name, first)
            .expect("verified tail record");
        if !self.adoptable_stamp(verified.stamp) {
            return Ok(false);
        }
        let check = self.reopen.as_ref().expect("reopening channel");
        let Some(message) = record_fields::decode(&channel.name, channel.creation, first, verified)
        else {
            return Err(invalid());
        };
        let digest =
            record_fields::restored_retry_digest(verified, &message).ok_or_else(invalid)?;
        let bytes = first.data.len() as u64;
        let kept = self.bytes_kept.checked_add(bytes).ok_or_else(invalid)?;
        let day = message
            .sent_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            / 86_400;
        let saved = SavedHead {
            lead_session: message.lead_session(),
            receipt: message.receipt,
            stamp: verified.stamp,
            kept,
            bytes,
            day,
            sender: message.sender,
            id: message.id,
            digest,
        };
        if !(check.participants.contains(&saved.sender)
            || crate::upgrade_notice::sent_by_owner(&saved.sender, &channel.name))
        {
            return Err(store.stop_for_older_state());
        }
        if check
            .reservation
            .as_ref()
            .is_some_and(|pending| !self.matches_saved_head(&saved, pending, self.stamp.attempt))
        {
            return Err(invalid());
        }

        settle(store, &channel.store_name)?;
        let again = self.inspect_tail(channel, store, key)?;
        if again.first.as_ref() != Some(first) {
            return Err(unfinished());
        }
        if check.reservation.is_none() {
            let bytes = saved
                .bytes
                .checked_add(retry_bytes(&saved.sender, &saved.id, saved.receipt.trace))
                .ok_or_else(invalid)?;
            let reservation = Reservation::restore(quota, &saved.sender, bytes, saved.day)?;
            let pending = SavedReservation {
                sender: saved.sender.clone(),
                position: saved.receipt.position,
                reservation,
            };
            if !self.matches_saved_head(&saved, &pending, self.stamp.attempt) {
                return Err(invalid());
            }
            store.save(Entry::Reserve {
                creation: channel.creation,
                sender: &saved.sender,
                position: saved.receipt.position,
                day: saved.day,
                bytes,
            })?;
            self.reopen.as_mut().expect("reopening channel").reservation = Some(pending);
        }
        store.save(Entry::Head {
            creation: channel.creation,
            position: saved.receipt.position,
            stamp: saved.stamp,
            new_stamp: true,
            bytes_kept: saved.kept,
            record_bytes: saved.bytes,
            day: saved.day,
            sender: &saved.sender,
            id: &saved.id,
            digest: &saved.digest,
            durability: saved.receipt.durability,
            trace: saved.receipt.trace,
            lead_session: saved.lead_session.as_ref(),
        })?;
        let pending = self
            .reopen
            .as_mut()
            .expect("reopening channel")
            .reservation
            .take()
            .expect("reserved startup append");
        self.restore_head(saved, pending, quota)?;
        channel.publish_head(self.head);
        let now = Instant::now();
        let age = SystemTime::now()
            .duration_since(message.sent_at)
            .unwrap_or_default();
        self.retained.back_mut().expect("adopted record").stored =
            now.checked_sub(age).unwrap_or(now);
        Ok(true)
    }

    pub(super) fn check_restored_stamp(
        &self,
        store: &StoreLease,
        verified: Option<malak_record::Verified<'_>>,
    ) -> Result<(), Error> {
        if let Some(check) = &self.reopen
            && verified.is_some_and(|valid| {
                !check.runs.contains_key(&valid.stamp.run)
                    || valid.stamp.attempt > self.stamp.attempt
            })
        {
            return Err(store.stop_for_older_state());
        }
        Ok(())
    }
}

impl Channel {
    pub(crate) fn restore_damage(&self, at: Position) {
        self.stopped_at.store(at.get(), Ordering::Release);
    }
}

#[derive(Default)]
struct Tail {
    first: Option<Record>,
    observed: crate::journal_entry::TailCut,
}

fn read_page(
    store: &StoreLease,
    channel: &Channel,
    from: Position,
    before: Option<Position>,
) -> Result<(Vec<Record>, bool), Error> {
    match layer(store.store.read(
        &channel.store_name,
        from,
        ReadLimit {
            records: PAGE,
            before,
        },
    ))? {
        ReadResult::Records {
            records,
            unparseable,
        } if records.len() <= PAGE
            && records.first().is_none_or(|record| record.position >= from)
            && records
                .last()
                .is_none_or(|record| before.is_none_or(|before| record.position < before))
            && records
                .windows(2)
                .all(|pair| pair[0].position < pair[1].position) =>
        {
            Ok((records, unparseable))
        }
        _ => Err(Error::unavailable("invalid channel check results")),
    }
}

fn invalid() -> Error {
    Error::unavailable("invalid restored channel state")
}

fn unfinished() -> Error {
    Error::unavailable("channel has an unfinished write or tail")
}
