//! Controlled store failures through the board's ordinary send path.

mod common;

use common::{Probe, limits, publisher};
use malak_core::{CreationId, Error, Outcome, Owner, Position, Receipt, SendRequest};
use malak_layers::{
    Claim, Claimed, ReadLimit, ReadResult, Record, Store, StoreInfo, Trim, Trimmed,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::Duration;

struct InjectedStore {
    inner: Probe,
    appends: AtomicUsize,
    fault: Fault,
}

enum Fault {
    ChangeRead,
    PauseRead {
        started: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    },
    Reject,
    ReportFailureAfterWriting,
    Pause {
        appends: usize,
        started: mpsc::Sender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
    },
}

impl InjectedStore {
    fn new(fault: Fault) -> Self {
        Self {
            inner: Probe::default(),
            appends: AtomicUsize::new(0),
            fault,
        }
    }
}

impl Store for InjectedStore {
    fn describe(&self) -> StoreInfo {
        self.inner.describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        self.inner.claim(mode)
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        let affected = match &self.fault {
            Fault::Pause { appends, .. } => *appends,
            _ => 1,
        };
        if self.appends.fetch_add(1, Ordering::Relaxed) >= affected {
            return self.inner.append(generation, channel, records);
        }
        let failed = || Error::unavailable("injected append failure");
        match &self.fault {
            Fault::ChangeRead | Fault::PauseRead { .. } => {
                self.inner.append(generation, channel, records)
            }
            Fault::Reject => Err(failed()),
            Fault::ReportFailureAfterWriting => {
                self.inner.append(generation, channel, records)?;
                Err(failed())
            }
            Fault::Pause {
                started, resume, ..
            } => {
                started.send(()).unwrap();
                resume.lock().unwrap().recv().unwrap();
                self.inner.append(generation, channel, records)
            }
        }
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        if let Fault::PauseRead { started, resume } = &self.fault {
            started.send(()).unwrap();
            resume.lock().unwrap().recv().unwrap();
        }
        let mut result = self.inner.read(channel, from, limit)?;
        if matches!(self.fault, Fault::ChangeRead)
            && let ReadResult::Records { records, .. } = &mut result
            && let Some(record) = records.first_mut()
        {
            let mut bytes = record.data.to_vec();
            bytes[0] ^= 1;
            record.data = bytes.into();
        }
        Ok(result)
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        self.inner.trim(generation, channel, trim)
    }
}

#[test]
fn a_whole_late_write_is_committed_after_it_reads_back() {
    let owner = Owner::in_memory(
        InjectedStore::new(Fault::ReportFailureAfterWriting),
        limits(),
    )
    .unwrap();
    let lead = owner.lead();
    let request = SendRequest::note("lead", "message", b"sent");

    assert_eq!(lead.send(request).unwrap().position, Position::FIRST);
    assert!(lead.send(request).unwrap().duplicate);
}

#[test]
fn an_abandoned_write_releases_its_storage_reservation() {
    let mut settings = limits();
    settings.daily_storage_bytes = 300;
    let owner = Owner::in_memory(InjectedStore::new(Fault::Reject), settings).unwrap();
    let worker = publisher(&owner, "worker");
    let request = SendRequest::note("lead", "message", b"sent");

    assert_eq!(
        worker.send(request).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    assert_eq!(worker.send(request).unwrap().position, Position::FIRST);
}

#[test]
fn timed_out_write_keeps_its_reservation_until_it_commits() {
    let (started, entered) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let store = InjectedStore::new(Fault::Pause {
        appends: 1,
        started,
        resume: Mutex::new(resume),
    });
    let mut settings = limits();
    settings.daily_storage_bytes = 300;
    let owner = Owner::in_memory(store, settings).unwrap();
    let worker = publisher(&owner, "worker");
    let request = SendRequest::note("first", "message", b"sent");

    thread::scope(|scope| {
        let original = scope.spawn(|| worker.send(request));
        entered.recv().unwrap();
        let result = original.join().unwrap();

        let other = worker.send(SendRequest {
            channel: "other",
            id: "new",
            ..request
        });
        release.send(()).unwrap();

        assert_eq!(result.unwrap_err().outcome(), Outcome::Unavailable);
        assert_eq!(other.unwrap_err().outcome(), Outcome::Unavailable);
    });
    let retry = worker.send(request).unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.position, Position::FIRST);
}

#[test]
fn timed_out_writes_on_two_channels_keep_their_daily_allowance() {
    pending_writes_keep_their_storage(2_850, u64::MAX, "daily storage allowance exceeded");
}

#[test]
fn timed_out_writes_on_two_channels_cannot_spend_the_leads_reserve() {
    pending_writes_keep_their_storage(u64::MAX, 4_750, "board storage allowance exceeded");
}

fn pending_writes_keep_their_storage(daily: u64, board: u64, reason: &str) {
    let (started, entered) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let store = InjectedStore::new(Fault::Pause {
        appends: 2,
        started,
        resume: Mutex::new(resume),
    });
    let mut settings = limits();
    settings.daily_storage_bytes = daily;
    settings.board_storage_bytes = board;
    settings.lead_storage_percent = 40;
    let owner = Owner::in_memory(store, settings).unwrap();
    let worker = publisher(&owner, "worker");
    let lead = owner.lead();
    let body = [1; 1024];
    let first = SendRequest::note("one", "first", &body);
    let second = SendRequest::note("two", "second", &body);
    let excess = SendRequest::note("three", "third", &body);
    const TIMEOUT: Duration = Duration::from_secs(10);

    let (before, after, retained) = thread::scope(|scope| {
        let worker = &worker;
        let pending = [first, second].map(|request| scope.spawn(move || worker.send(request)));
        for _ in &pending {
            entered.recv_timeout(TIMEOUT).unwrap();
        }
        let before = worker.send(excess);
        for request in pending {
            let timeout = request.join().unwrap().unwrap_err();
            assert_eq!(timeout.to_string(), "worker did not answer in time");
        }
        let after = worker.send(excess);
        let retained = lead.send(SendRequest::note("lead", "reserve", &body));
        release.send(()).unwrap();
        release.send(()).unwrap();
        (before, after, retained)
    });

    for result in [before, after, worker.send(excess)] {
        let error = result.unwrap_err();
        assert!(error.is_limit());
        assert_eq!(error.to_string(), reason);
    }
    assert_eq!(retained.unwrap().position, Position::FIRST);
    for request in [first, second] {
        let receipt = worker.send(request).unwrap();
        assert_eq!(receipt.position, Position::FIRST);
        assert!(receipt.duplicate);
    }
}

#[test]
fn a_record_that_fails_verification_stops_the_channel() {
    let owner = Owner::in_memory(InjectedStore::new(Fault::ChangeRead), limits()).unwrap();
    let lead = owner.lead();
    let request = SendRequest::note("lead", "message", b"sent");
    lead.send(request).unwrap();

    let damage = lead.get("lead", 1).unwrap_err();
    assert_eq!(damage.outcome(), Outcome::Unavailable);
    assert!(damage.to_string().contains("damage at position 1"));
    assert_eq!(
        lead.send(request).unwrap_err().outcome(),
        Outcome::Unavailable
    );
}

fn change_during_read(
    change: impl FnOnce(&Owner, &malak_core::Handle),
) -> Result<malak_core::ReadReply, Error> {
    let (started, entered) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let store = InjectedStore::new(Fault::PauseRead {
        started,
        resume: Mutex::new(resume),
    });
    let owner = Owner::in_memory(store, limits()).unwrap();
    let lead = owner.lead();
    let grant = common::grant("lead", &[malak_core::Action::Read]);
    let worker = owner.enrol("worker", grant).unwrap();
    lead.send(SendRequest::note("lead", "one", b"one")).unwrap();

    thread::scope(|scope| {
        let reading = scope.spawn(|| worker.read(&["lead"], 1, std::time::Duration::from_secs(1)));
        entered.recv().unwrap();
        change(&owner, &worker);
        release.send(()).unwrap();
        reading.join().unwrap()
    })
}

#[test]
fn a_read_hands_over_nothing_after_revocation() {
    let result = change_during_read(|owner, worker| owner.revoke(worker.id()).unwrap());
    assert_eq!(result.unwrap_err().outcome(), Outcome::Refused);
}

#[test]
fn a_read_holds_back_a_message_after_the_grant_changes() {
    let reply = change_during_read(|owner, worker| {
        owner
            .replace_grant(worker.id(), malak_core::Grant::default())
            .unwrap()
    })
    .unwrap();
    assert!(reply.messages.is_empty());
    assert!(reply.held_back);
}

fn race_a_read_with(change: impl FnOnce(&Owner, CreationId) + Send) -> (Owner, Receipt) {
    let (started, entered) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let store = InjectedStore::new(Fault::PauseRead {
        started,
        resume: Mutex::new(resume),
    });
    let owner = Owner::in_memory(store, limits()).unwrap();
    let lead = owner.lead();
    let receipt = lead
        .send(SendRequest::note("work", "initial", b"initial"))
        .unwrap();
    thread::scope(|scope| {
        let read = scope.spawn(|| lead.read(&["work"], 1, Duration::ZERO));
        entered.recv_timeout(Duration::from_secs(3)).unwrap();
        let (announced, changing) = mpsc::channel();
        let board = &owner;
        let mutation = scope.spawn(move || {
            announced.send(()).unwrap();
            change(board, receipt.creation);
        });
        changing.recv_timeout(Duration::from_secs(3)).unwrap();

        release.send(()).unwrap();

        let result = read.join().unwrap();
        if let Err(error) = result {
            assert_eq!(error.outcome(), Outcome::Unavailable);
            assert!(
                error.to_string().contains("creation has been removed"),
                "{error}"
            );
        }
        mutation.join().unwrap();
    });
    (owner, receipt)
}

#[test]
fn a_read_overlapping_trim_does_not_stop_the_channel() {
    let (owner, first) = race_a_read_with(|owner, creation| {
        assert_eq!(owner.trim("work", creation, 2).unwrap(), 2);
    });

    let second = owner
        .lead()
        .send(SendRequest::note("work", "after-trim", b"after trim"))
        .unwrap();

    assert_eq!(second.creation, first.creation);
    assert_eq!(second.position.get(), 2);
}

#[test]
fn a_read_overlapping_removal_does_not_stop_the_successor_creation() {
    let (owner, first) = race_a_read_with(|owner, creation| {
        assert_eq!(
            owner.remove_channel("work", creation).unwrap(),
            (true, None)
        );
    });

    let second = owner
        .lead()
        .send(SendRequest::note("work", "after-removal", b"after removal"))
        .unwrap();

    assert_ne!(second.creation, first.creation);
    assert_eq!(second.position, Position::FIRST);
}

#[test]
fn another_channel_completes_while_a_store_worker_is_blocked() {
    let (started, entered) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let store = InjectedStore::new(Fault::Pause {
        appends: 1,
        started,
        resume: Mutex::new(resume),
    });
    let owner = Owner::in_memory(store, limits()).unwrap();
    let lead = owner.lead();

    thread::scope(|scope| {
        let blocked = scope.spawn(|| lead.send(SendRequest::note("blocked", "one", b"one")));
        entered.recv().unwrap();

        let independent = lead.send(SendRequest::note("independent", "two", b"two"));
        release.send(()).unwrap();

        assert_eq!(independent.unwrap().position, Position::FIRST);
        assert_eq!(blocked.join().unwrap().unwrap().position, Position::FIRST);
    });
}
