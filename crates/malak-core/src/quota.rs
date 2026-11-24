use crate::{Error, Limits, ParticipantId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

#[derive(Default)]
pub(crate) struct Quota {
    total: u64,
    participant_bytes: u64,
    accounts: HashMap<ParticipantId, Daily>,
}

#[derive(Default)]
struct Daily {
    day: u64,
    kept: u64,
    pending: u64,
}

impl Daily {
    fn roll(&mut self, today: u64) {
        if today > self.day {
            self.day = today;
            self.kept = 0;
        }
    }
}

pub(crate) fn today() -> u64 {
    UNIX_EPOCH.elapsed().unwrap_or_default().as_secs() / (24 * 60 * 60)
}

pub(crate) struct Reservation {
    ledger: Arc<Mutex<Quota>>,
    sender: ParticipantId,
    bytes: u64,
    pending: bool,
    pub(crate) day: u64,
}

impl Reservation {
    pub(crate) fn take(
        ledger: &Arc<Mutex<Quota>>,
        limits: &Limits,
        sender: &ParticipantId,
        bytes: u64,
    ) -> Result<Self, Error> {
        let mut quota = ledger.lock();
        let reserve = (u128::from(limits.board_storage_bytes)
            * u128::from(limits.lead_storage_percent)
            / 100) as u64;
        if bytes > limits.board_storage_bytes.saturating_sub(quota.total)
            || !sender.is_lead()
                && bytes
                    > (limits.board_storage_bytes - reserve).saturating_sub(quota.participant_bytes)
        {
            return Err(Error::unavailable("board storage allowance exceeded").at_limit());
        }

        let account = quota.accounts.entry(sender.clone()).or_default();
        account.roll(today());
        let day = account.day;
        if bytes
            > limits
                .daily_storage_bytes
                .saturating_sub(account.kept + account.pending)
        {
            return Err(Error::unavailable("daily storage allowance exceeded").at_limit());
        }
        account.pending += bytes;
        quota.total += bytes;
        if !sender.is_lead() {
            quota.participant_bytes += bytes;
        }
        Ok(Self {
            ledger: ledger.clone(),
            sender: sender.clone(),
            bytes,
            pending: true,
            day,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restore(
        ledger: &Arc<Mutex<Quota>>,
        sender: &ParticipantId,
        bytes: u64,
        day: u64,
    ) -> Result<Self, Error> {
        let mut quota = ledger.lock();
        let total = quota.total.checked_add(bytes).ok_or_else(invalid)?;
        let participant_bytes = quota
            .participant_bytes
            .checked_add(if sender.is_lead() { 0 } else { bytes })
            .ok_or_else(invalid)?;
        let account = quota.accounts.entry(sender.clone()).or_default();
        account.roll(day);
        account
            .kept
            .checked_add(account.pending)
            .and_then(|value| value.checked_add(bytes))
            .ok_or_else(invalid)?;
        account.pending += bytes;
        quota.total = total;
        quota.participant_bytes = participant_bytes;
        Ok(Self {
            ledger: ledger.clone(),
            sender: sender.clone(),
            bytes,
            pending: true,
            day,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restore_extra(&self, bytes: u64, day: u64) -> Result<(), Error> {
        Self::restore(&self.ledger, &self.sender, bytes, day)?.commit(day);
        Ok(())
    }

    pub(crate) fn commit(mut self, day: u64) {
        let mut quota = self.ledger.lock();
        let account = quota
            .accounts
            .get_mut(&self.sender)
            .expect("reserved quota account");
        account.roll(day);
        account.pending -= self.bytes;
        if account.day == day {
            account.kept += self.bytes;
        }
        self.pending = false;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.pending {
            let mut quota = self.ledger.lock();
            quota
                .accounts
                .get_mut(&self.sender)
                .expect("reserved quota account")
                .pending -= self.bytes;
            quota.release_space(self.sender.is_lead(), self.bytes);
        }
    }
}

impl Quota {
    pub(crate) fn release_space(&mut self, lead: bool, bytes: u64) {
        self.total -= bytes;
        if !lead {
            self.participant_bytes -= bytes;
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn invalid() -> Error {
    Error::unavailable("invalid restored storage reservation")
}
