use crate::journal_entry::Entry;
use crate::quota::{Quota, Reservation, today};
use crate::record_fields::{Fields, RetryDigest, RetryInput};
use crate::retention::Retention;
use crate::storage::{StoreLease, layer};
use crate::{ChannelName, CreationId, Error, Limits, MessageId, ParticipantId, Position, Receipt};
use malak_layers::{Claim, Claimed, ReadLimit, ReadResult, Record, Trim, Trimmed};
use malak_record::{RecordKey, Stamp, store_name};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod activity;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod reopen;

mod shares;
pub use shares::SenderShareRule;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod repair;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use reopen::{Reopen, SavedReservation};

const RETRY_WINDOW: usize = 10_000;

pub(crate) type LockedHistory = parking_lot::ArcMutexGuard<parking_lot::RawMutex, History>;

pub(crate) struct Channel {
    pub(crate) name: ChannelName,
    pub(crate) creation: CreationId,
    pub(crate) store_name: String,
    pub(crate) history: Arc<Mutex<History>>,
    floor: AtomicU64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) activity: activity::ChannelActivity,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    head: AtomicU64,
    stopped_at: AtomicU64,
    removed: AtomicBool,
    retention: Retention,
    share: Option<shares::Policy>,
}

pub(crate) struct History {
    head: u64,
    floor: u64,
    voids: Vec<std::ops::Range<u64>>,
    stamp: Stamp,
    stamp_ranges: Vec<(Position, Stamp)>,
    pending: Option<Pending>,
    cut: Option<Cut>,
    bytes_kept: u64,
    retries: HashMap<ParticipantId, SenderRetries>,
    retained: VecDeque<Kept>,
    shares: Option<shares::Ledger>,
    trim_pending: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    reopen: Option<Reopen>,
}

#[derive(Default)]
struct SenderRetries {
    newest: VecDeque<MessageId>,
    entries: HashMap<MessageId, Retry>,
}

struct Retry {
    receipt: Receipt,
    digest: RetryDigest,
    bytes: u64,
}

struct Kept {
    stored: Instant,
    bytes: u64,
    lead: bool,
}

struct Pending {
    record: Record,
    stamp: Stamp,
    sender: ParticipantId,
    id: MessageId,
    retry: Retry,
    reservation: Reservation,
    commit_day: Option<u64>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    kind: crate::Kind,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    sent_at: std::time::SystemTime,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    peer: Option<Arc<malak_layers::SenderPeer>>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    lead_session: Option<crate::LeadSession>,
}

#[derive(Clone, Copy)]
struct Cut {
    begun: bool,
    receipt: Option<Receipt>,
}

impl Channel {
    pub(crate) fn new(
        name: ChannelName,
        creation: CreationId,
        run: [u8; 16],
        limits: &Limits,
    ) -> Self {
        let mut retention = Retention::for_channel(&limits.retention, &name);
        let share = shares::Policy::for_inbox(limits, &name).or_else(|| {
            shares::Policy::for_channel(&limits.sender_shares, &name).map(|(policy, count)| {
                retention.messages = retention.messages.min(count);
                policy
            })
        });
        let ledger = share.as_ref().map(|_| shares::Ledger::default());
        Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            activity: activity::ChannelActivity::default(),
            store_name: store_name(&name, creation),
            retention,
            share,
            name,
            creation,
            floor: AtomicU64::new(1),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            head: AtomicU64::new(1),
            stopped_at: AtomicU64::new(0),
            removed: AtomicBool::new(false),
            history: Arc::new(Mutex::new(History {
                head: 1,
                floor: 1,
                voids: Vec::new(),
                stamp: Stamp { run, attempt: 0 },
                stamp_ranges: Vec::new(),
                pending: None,
                cut: None,
                bytes_kept: 0,
                retries: HashMap::new(),
                retained: VecDeque::new(),
                shares: ledger,
                trim_pending: false,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                reopen: None,
            })),
        }
    }

    pub(crate) fn floor(&self) -> u64 {
        self.floor.load(Ordering::Acquire)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn publish_head(&self, head: u64) {
        self.head.store(head, Ordering::Release);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn status(&self) -> (u64, u64, bool) {
        loop {
            let floor = self.floor();
            let stopped = self.stopped_at.load(Ordering::Acquire);
            let head = self.head.load(Ordering::Acquire);
            if floor == self.floor() && stopped == self.stopped_at.load(Ordering::Acquire) {
                return (floor, head, stopped != 0);
            }
        }
    }

    pub(crate) fn mark_removed(&self) {
        self.removed.store(true, Ordering::Release);
    }

    pub(crate) fn is_removed(&self) -> bool {
        self.removed.load(Ordering::Acquire)
    }

    pub(crate) fn present(&self) -> Result<(), Error> {
        if self.is_removed() {
            return Err(Error::unavailable("channel creation has been removed"));
        }
        Ok(())
    }

    pub(crate) fn available(&self) -> Result<(), Error> {
        self.present()?;
        let at = self.stopped_at.load(Ordering::Acquire);
        if at != 0 {
            return Err(Error::unavailable(format!(
                "channel stopped for damage at position {at}"
            )));
        }
        Ok(())
    }
}

pub(crate) struct Write<'a> {
    pub(crate) digest: RetryDigest,
    pub(crate) trace: Option<crate::TraceId>,
    pub(crate) sender: &'a ParticipantId,
    pub(crate) id: MessageId,
    pub(crate) fields: Fields<'a>,
    pub(crate) size: usize,
}

impl History {
    pub(crate) fn trim_needed(&self, channel: &Channel) -> bool {
        self.trim_pending || self.retention_floor(channel) > self.floor
    }

    fn retention_floor(&self, channel: &Channel) -> u64 {
        let count = if self.voids.is_empty() {
            self.retained
                .len()
                .saturating_sub(channel.retention.messages)
        } else {
            self.retained
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, record)| record.bytes != 0)
                .nth(channel.retention.messages)
                .map_or(0, |(index, _)| index + 1)
        };
        let age = channel.retention.age;
        let now = Instant::now();
        let expired = self
            .retained
            .iter()
            .take_while(|entry| now.duration_since(entry.stored) >= age)
            .count();
        let count_floor = self.floor + count as u64;
        let safe_floor = match (&self.shares, &channel.share) {
            (Some(ledger), Some(policy)) if policy.protects_retention() => {
                ledger.permits_trim_through(count_floor)
            }
            _ => count_floor,
        };
        safe_floor.max(self.floor + expired as u64)
    }

    pub(crate) fn trim(
        &mut self,
        channel: &Channel,
        store: &StoreLease,
        quota: &Mutex<Quota>,
        keep_from: Option<u64>,
    ) -> Result<u64, Error> {
        store.writable()?;
        let wanted = keep_from.unwrap_or_else(|| self.retention_floor(channel));
        let floor = wanted.min(self.head);
        if floor > self.floor {
            let count = (floor - self.floor) as usize;
            let dropped: u64 = self
                .retained
                .iter()
                .take(count)
                .map(|entry| entry.bytes)
                .sum();
            let bytes_kept = self.bytes_kept - dropped;
            store.save(Entry::Trim {
                creation: channel.creation,
                floor,
                bytes_kept,
            })?;
            self.bytes_kept = bytes_kept;
            self.release_prefix(count, &mut quota.lock());
            self.floor = floor;
            self.trim_pending = true;
            channel.floor.store(floor, Ordering::Release);
        }

        if self.trim_pending {
            self.trim_store_before(channel, store)?;
            store.save(Entry::TrimDone(channel.creation))?;
            self.trim_pending = false;
        }
        Ok(self.floor)
    }

    fn trim_store_before(&self, channel: &Channel, store: &StoreLease) -> Result<(), Error> {
        store.writable()?;
        if layer(store.store.trim(
            store.generation,
            &channel.store_name,
            Trim::Keep(self.floor..Position::MAX + 1),
        ))? != Trimmed::Done
        {
            return Err(Error::unavailable("store did not trim the channel"));
        }
        Ok(())
    }

    pub(crate) fn empty_store(&self, channel: &Channel, store: &StoreLease) -> Result<(), Error> {
        store.writable()?;
        if layer(
            store
                .store
                .trim(store.generation, &channel.store_name, Trim::Keep(1..1)),
        )? != Trimmed::Done
        {
            return Err(Error::unavailable("store did not remove the channel"));
        }
        settle(store, &channel.store_name)
    }

    pub(crate) fn release_removed_space(&mut self, quota: &Mutex<Quota>) {
        self.pending = None;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.reopen = None;
        }
        let mut quota = quota.lock();
        self.release_prefix(self.retained.len(), &mut quota);
        for (sender, retries) in self.retries.drain() {
            for retry in retries.entries.into_values() {
                quota.release_space(sender.is_lead(), retry.bytes);
            }
        }
    }

    pub(crate) fn head(&self) -> u64 {
        self.head
    }

    pub(crate) fn locate(
        &self,
        creation: CreationId,
        position: Position,
    ) -> Option<crate::GetReply> {
        let at = position.get();
        if at < self.floor {
            Some(crate::GetReply::Trimmed {
                creation,
                before: self.floor,
            })
        } else if at >= self.head {
            Some(crate::GetReply::NeverWritten { creation })
        } else if self.voids.iter().any(|void| void.contains(&at)) {
            Some(crate::GetReply::Void { creation })
        } else {
            None
        }
    }

    pub(crate) fn journal_view<'a>(&'a self, channel: &'a Channel) -> malak_record::History<'a> {
        malak_record::History {
            channel: &channel.store_name,
            retained: self.floor..self.head,
            stamps: &self.stamp_ranges,
            voids: &self.voids,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn attempt(&self) -> u64 {
        self.stamp.attempt
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn latest_signed_run(&self) -> [u8; 16] {
        self.stamp_ranges
            .last()
            .map_or(self.stamp.run, |(_, stamp)| stamp.run)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn retry_positions(&self) -> impl Iterator<Item = Position> + '_ {
        self.retries
            .values()
            .flat_map(|sender| sender.entries.values().map(|retry| retry.receipt.position))
    }

    pub(crate) fn load(
        &mut self,
        channel: &Channel,
        store: &StoreLease,
        key: &RecordKey,
        position: Position,
    ) -> Result<(crate::Message, usize), Error> {
        let range = ReadLimit {
            records: 1,
            before: Position::try_from(self.head).ok(),
        };
        let found = layer(store.store.read(&channel.store_name, position, range))?;
        let ReadResult::Records { ref records, .. } = found else {
            return Err(Error::unavailable("store did not return an ordinary read"));
        };
        let journal = self.journal_view(channel);
        if let [record] = records.as_slice()
            && record.position == position
        {
            let verified = key.verify(&channel.store_name, record);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            self.check_restored_stamp(store, verified)?;
            if let Some(verified) =
                verified.filter(|verified| journal.names(position, verified.stamp))
                && let Some(message) =
                    crate::record_fields::decode(&channel.name, channel.creation, record, verified)
            {
                return Ok((message, record.data.len()));
            }
        }

        Err(self.damage(channel, store, position))
    }

    fn damage(&self, channel: &Channel, store: &StoreLease, position: Position) -> Error {
        if let Err(error) = store.save(Entry::Damage {
            creation: channel.creation,
            position,
        }) {
            return error;
        }
        channel.stopped_at.store(position.get(), Ordering::Release);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        store
            .events
            .damaged(&channel.name, channel.creation, position);
        Error::unavailable(format!("channel stopped for damage at position {position}"))
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn has_retry(&self, sender: &ParticipantId, id: &MessageId) -> bool {
        self.retries
            .get(sender)
            .is_some_and(|entries| entries.entries.contains_key(id))
    }

    pub(crate) fn retry(
        &self,
        sender: &ParticipantId,
        id: &MessageId,
        input: &RetryInput<'_>,
    ) -> Result<Option<Receipt>, Error> {
        let Some(retry) = self.retries.get(sender).and_then(|r| r.entries.get(id)) else {
            return Ok(None);
        };
        if !input.matches(retry.digest) {
            return Err(Error::refused("message ID already names different content"));
        }
        Ok(Some(Receipt {
            duplicate: true,
            ..retry.receipt
        }))
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.is_some() || self.cut.is_some()
    }

    pub(crate) fn prepare(
        &mut self,
        channel: &Channel,
        write: Write<'_>,
        key: &RecordKey,
        ledger: &Arc<Mutex<Quota>>,
        limits: &Limits,
        store: &StoreLease,
    ) -> Result<(), Error> {
        debug_assert!(!self.pending());
        store.writable()?;
        let position = Position::try_from(self.head)
            .map_err(|_| Error::unavailable("channel positions exhausted"))?;
        let size = write.size;
        if size > store.info.max_record_bytes {
            return Err(Error::not_supported(
                "record exceeds the store's size limit",
            ));
        }
        let retry_bytes = retry_bytes(write.sender, &write.id, write.trace);
        let bytes = (size as u64)
            .checked_add(retry_bytes)
            .ok_or_else(|| Error::unavailable("record exceeds storage size"))?;
        let reservation = Reservation::take(ledger, limits, write.sender, bytes)?;
        let receipt = Receipt {
            position,
            creation: channel.creation,
            durability: store.info.durability,
            duplicate: false,
            trace: write.trace,
        };
        store.save(Entry::Reserve {
            creation: channel.creation,
            sender: write.sender,
            position,
            day: reservation.day,
            bytes,
        })?;
        let record = key.seal_hashed(
            &channel.store_name,
            position,
            self.stamp,
            write.fields.encoded.bytes(),
            &write.fields.body,
        )?;
        self.pending = Some(Pending {
            record,
            stamp: self.stamp,
            sender: write.sender.clone(),
            id: write.id,
            retry: Retry {
                receipt,
                digest: write.digest,
                bytes: retry_bytes,
            },
            reservation,
            commit_day: None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            kind: write.fields.kind,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            sent_at: write.fields.sent_at,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            peer: write.fields.peer,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            lead_session: write.fields.lead_session,
        });
        Ok(())
    }

    pub(crate) fn append(
        &mut self,
        store: &StoreLease,
        channel: &Channel,
        quota: &Mutex<Quota>,
    ) -> Result<Receipt, Error> {
        store.writable()?;
        let pending = self.pending.as_ref().expect("prepared write");
        if store
            .store
            .append(
                store.generation,
                &channel.store_name,
                std::slice::from_ref(&pending.record),
            )
            .is_ok()
        {
            return self.commit(store, channel, quota);
        }
        self.settle(store, channel, quota)?
            .ok_or_else(|| Error::unavailable("store did not keep the write"))
    }

    pub(crate) fn settle(
        &mut self,
        store: &StoreLease,
        channel: &Channel,
        quota: &Mutex<Quota>,
    ) -> Result<Option<Receipt>, Error> {
        store.writable()?;
        if self.cut.is_none() {
            let pending = self.pending.as_ref().expect("pending write");
            let (mut whole, mut tail) = pending.readback(store, &channel.store_name)?;
            if whole {
                settle(store, &channel.store_name)?;
                (whole, tail) = pending.readback(store, &channel.store_name)?;
            }
            let receipt = if whole {
                Some(self.commit(store, channel, quota)?)
            } else {
                None
            };
            if whole && !tail {
                return Ok(receipt);
            }
            self.cut = Some(Cut {
                begun: false,
                receipt,
            });
        }

        if !self.cut.expect("cut in progress").begun {
            self.begin_cut(channel, store, None)?;
            self.cut.as_mut().expect("cut in progress").begun = true;
        }
        self.finish_cut(channel, store, self.pending.is_some())?;
        self.pending = None;
        Ok(self.cut.take().and_then(|cut| cut.receipt))
    }

    fn begin_cut(
        &mut self,
        channel: &Channel,
        store: &StoreLease,
        tail: Option<crate::journal_entry::TailCut>,
    ) -> Result<(), Error> {
        let attempt = self
            .stamp
            .attempt
            .checked_add(1)
            .ok_or_else(|| Error::unavailable("channel attempts exhausted"))?;
        store.save(Entry::Cut {
            creation: channel.creation,
            head: self.head,
            attempt,
            tail,
        })?;
        self.stamp.attempt = attempt;
        Ok(())
    }

    fn finish_cut(
        &self,
        channel: &Channel,
        store: &StoreLease,
        released: bool,
    ) -> Result<(), Error> {
        store.writable()?;
        let end = self
            .retained
            .iter()
            .rposition(|record| record.bytes != 0)
            .map_or(self.floor, |index| self.floor + index as u64 + 1);
        #[cfg(test)]
        crate::durable_kill::hit("cut-trim-before");
        if layer(store.store.trim(
            store.generation,
            &channel.store_name,
            Trim::Keep(self.floor..end),
        ))? != Trimmed::Done
        {
            return Err(Error::unavailable("store did not cut the channel tail"));
        }
        #[cfg(test)]
        crate::durable_kill::hit("cut-trim-after");
        settle(store, &channel.store_name)?;
        #[cfg(test)]
        crate::durable_kill::hit("cut-settled");
        store.save(Entry::CutDone {
            creation: channel.creation,
            released,
        })
    }

    fn commit(
        &mut self,
        store: &StoreLease,
        channel: &Channel,
        quota: &Mutex<Quota>,
    ) -> Result<Receipt, Error> {
        let pending = self.pending.as_mut().expect("prepared write");
        let receipt = pending.retry.receipt;
        let new_stamp = self
            .stamp_ranges
            .last()
            .is_none_or(|&(_, stamp)| stamp != pending.stamp);
        let record_bytes = pending.record.data.len() as u64;
        let bytes_kept = self.bytes_kept + record_bytes;
        let day = *pending.commit_day.get_or_insert_with(today);
        store.save(Entry::Head {
            creation: channel.creation,
            position: receipt.position,
            stamp: pending.stamp,
            new_stamp,
            bytes_kept,
            record_bytes,
            day,
            sender: &pending.sender,
            id: &pending.id,
            digest: &pending.retry.digest,
            durability: receipt.durability,
            trace: receipt.trace,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            lead_session: pending.lead_session.as_ref(),
        })?;
        let pending = self.pending.take().expect("saved write");
        if new_stamp {
            self.stamp_ranges.push((receipt.position, pending.stamp));
        }
        self.bytes_kept = bytes_kept;
        self.keep_record(&pending.sender, record_bytes, Instant::now());
        pending.reservation.commit(day);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            channel.publish_head(self.head);
            store.events.stored(
                channel,
                &pending.sender,
                receipt.position,
                pending.kind,
                pending.sent_at,
                pending.peer,
            );
        }
        self.remember_retry(pending.sender, pending.id, pending.retry, quota);
        Ok(receipt)
    }

    fn remember_retry(
        &mut self,
        sender: ParticipantId,
        id: MessageId,
        retry: Retry,
        quota: &Mutex<Quota>,
    ) {
        let lead = sender.is_lead();
        let retries = self.retries.entry(sender).or_default();
        retries.newest.push_back(id.clone());
        retries.entries.insert(id, retry);
        if retries.newest.len() > RETRY_WINDOW {
            let old = retries.newest.pop_front().expect("retry window");
            let dropped = retries.entries.remove(&old).expect("retry entry");
            quota.lock().release_space(lead, dropped.bytes);
        }
    }
}

fn retry_bytes(sender: &ParticipantId, id: &MessageId, trace: Option<crate::TraceId>) -> u64 {
    (id.as_str().len()
        + sender.as_str().len()
        + 8
        + 16
        + 2
        + 32
        + trace.map_or(0, crate::TraceId::len)) as u64
}

impl Pending {
    fn readback(&self, store: &StoreLease, name: &str) -> Result<(bool, bool), Error> {
        let range = ReadLimit {
            records: 2,
            before: None,
        };
        let ReadResult::Records {
            records,
            unparseable,
        } = layer(store.store.read(name, self.record.position, range))?
        else {
            return Err(Error::unavailable("store did not return an ordinary read"));
        };
        let whole = records.first() == Some(&self.record);
        let tail = unparseable || records.len() > usize::from(whole);
        Ok((whole, tail))
    }
}

fn settle(store: &StoreLease, name: &str) -> Result<(), Error> {
    if !matches!(
        layer(store.store.claim(Claim::Settle {
            generation: store.generation,
            channel: name
        }))?,
        Claimed::Done
    ) {
        return Err(Error::unavailable("store did not settle the channel"));
    }
    Ok(())
}
