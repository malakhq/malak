use super::*;
use crate::journal_entry::{PlannedChannel, SavedHead};
use std::ops::Range;

impl Channel {
    pub(crate) fn clear_damage(&self) {
        self.stopped_at.store(0, Ordering::Release);
    }
}

impl History {
    pub(crate) fn validate_plan(&self, plan: &PlannedChannel) -> Result<(), Error> {
        if self.head != plan.head
            || plan.voids.iter().any(|range| {
                range.start < self.floor
                    || range.end > self.head
                    || self.voids.iter().any(|old| overlaps(old, range))
            })
            || plan
                .kept
                .iter()
                .take_while(|item| item.position.get() < self.head)
                .any(|item| {
                    let at = item.position.get();
                    at < self.floor
                        || self
                            .voids
                            .iter()
                            .chain(&plan.voids)
                            .any(|range| range.contains(&at))
                })
        {
            return Err(invalid());
        }
        let void_count: u64 = self
            .voids
            .iter()
            .chain(&plan.voids)
            .map(|range| {
                range
                    .end
                    .min(self.head)
                    .saturating_sub(range.start.max(self.floor))
            })
            .sum();
        let retained = plan
            .kept
            .iter()
            .take_while(|item| item.position.get() < self.head)
            .count() as u64;
        if retained + void_count != self.head - self.floor {
            return Err(invalid());
        }
        Ok(())
    }

    pub(crate) fn void_bytes(&self, ranges: &[Range<u64>]) -> u64 {
        ranges
            .iter()
            .map(|range| {
                let first = (range.start - self.floor) as usize;
                let count = (range.end - range.start) as usize;
                self.retained
                    .iter()
                    .skip(first)
                    .take(count)
                    .map(|item| item.bytes)
                    .sum::<u64>()
            })
            .sum()
    }

    pub(crate) fn restored_repair_bytes(&self, plan: &PlannedChannel) -> u64 {
        self.bytes_kept - self.void_bytes(&plan.voids)
    }

    pub(crate) fn finish_repair(
        &mut self,
        channel: &Channel,
        plan: &PlannedChannel,
        adopted: Vec<SavedHead>,
        quota: &Arc<Mutex<Quota>>,
    ) -> Result<(), Error> {
        self.validate_plan(plan)?;
        let prior_attempt = self.stamp.attempt.checked_sub(1).ok_or_else(invalid)?;
        if adopted.len() as u64 != plan.new_head - plan.head {
            return Err(invalid());
        }
        self.void(&plan.voids, quota);
        for head in adopted {
            let bytes = head
                .bytes
                .checked_add(retry_bytes(&head.sender, &head.id, head.receipt.trace))
                .ok_or_else(invalid)?;
            let hold = Reservation::restore(quota, &head.sender, bytes, head.day)?;
            let pending = SavedReservation {
                sender: head.sender.clone(),
                position: head.receipt.position,
                reservation: hold,
            };
            self.restore_head_at(head, pending, quota, prior_attempt)?;
        }
        self.trim_pending = false;
        channel.clear_damage();
        Ok(())
    }

    fn void(&mut self, ranges: &[Range<u64>], quota: &Mutex<Quota>) {
        let mut quota = quota.lock();
        for range in ranges {
            for at in range.clone() {
                let record = &mut self.retained[(at - self.floor) as usize];
                let bytes = std::mem::take(&mut record.bytes);
                if let Some(ledger) = &mut self.shares {
                    ledger.remove(at);
                }
                self.bytes_kept -= bytes;
                quota.release_space(record.lead, bytes);
            }
        }
        for (sender, retries) in &mut self.retries {
            retries.entries.retain(|_, retry| {
                let at = retry.receipt.position.get();
                let index = ranges.partition_point(|range| range.end <= at);
                if ranges.get(index).is_some_and(|range| range.contains(&at)) {
                    quota.release_space(sender.is_lead(), retry.bytes);
                    false
                } else {
                    true
                }
            });
            retries.newest.retain(|id| retries.entries.contains_key(id));
        }
        self.voids.extend_from_slice(ranges);
        self.voids.sort_unstable_by_key(|range| range.start);
    }
}

fn overlaps(left: &Range<u64>, right: &Range<u64>) -> bool {
    left.start < right.end && right.start < left.end
}

fn invalid() -> Error {
    Error::unavailable("invalid saved channel repair")
}
