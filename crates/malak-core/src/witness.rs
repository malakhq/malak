//! Read-back of the store's durable witness and its independent publisher.

use crate::storage::{STORE_WAIT, StoreLease, layer};
use crate::workers::Workers;
use crate::{Error, Position};
use bytes::Bytes;
use malak_journal::{Checkpoint, InspectedJournal};
use malak_layers::{Claim, Claimed, ReadLimit, ReadResult, Record, Store, Trim, Trimmed};
use malak_record::RecordKey;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const CHANNEL: &str = "_witness";
const PAGE: usize = 64;
pub(crate) const RETAINED: usize = 16;

pub(crate) fn check_start(
    inspected: &InspectedJournal,
    store: &StoreLease,
    key: &Arc<RecordKey>,
    workers: &Workers,
) -> Result<(), Error> {
    let held = WitnessStore::new(store, key);
    let scanned = workers.until(Instant::now() + STORE_WAIT, move || held.scan(None))?;
    check_checkpoint(inspected, scanned.newest, scanned.conflict)
}

pub(crate) fn check_checkpoint(
    inspected: &InspectedJournal,
    newest: Option<Checkpoint>,
    conflict: bool,
) -> Result<(), Error> {
    let Some(latest) = newest else {
        return Ok(());
    };
    let accepted = inspected
        .accepted_entries()
        .map_err(|error| Error::unavailable(format!("board journal: {error}")))?
        .map(|(at, _)| at)
        .find(|at| at.sequence == latest.sequence);
    if conflict || accepted != Some(latest) {
        return Err(Error::refused(
            "the store's witness is newer than or differs from the state folder",
        ));
    }
    Ok(())
}

pub(crate) fn start(
    store: &StoreLease,
    key: &Arc<RecordKey>,
    mut confirmed: VecDeque<Position>,
) -> Result<(), Error> {
    let journal = Arc::downgrade(store.witness_journal());
    let generation = store.generation;
    let store = Arc::downgrade(&store.store);
    let key = Arc::downgrade(key);
    std::thread::Builder::new()
        .name("malak-witness".into())
        .spawn(move || {
            let mut next = None;
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let (Some(journal), Some(store), Some(key)) =
                    (journal.upgrade(), store.upgrade(), key.upgrade())
                else {
                    break;
                };
                let Ok(Some(at)) = journal.hand() else {
                    continue;
                };
                let target = WitnessStore {
                    store,
                    generation,
                    key,
                };
                let minimum = confirmed.back().map_or(1, |p| p.get() + 1);
                let Ok(position) = target.publish(at, next, minimum) else {
                    next = None;
                    continue;
                };
                next = position
                    .get()
                    .checked_add(1)
                    .and_then(|next| next.try_into().ok());

                if journal.confirm(at, position).is_err() {
                    continue;
                }
                confirmed.push_back(position);
                if confirmed.len() > RETAINED {
                    confirmed.pop_front();
                    let floor = confirmed.front().expect("confirmations remain").get();
                    #[cfg(test)]
                    crate::durable_kill::hit("witness-prune-before");
                    let trimmed = target.keep(floor..Position::MAX + 1);
                    #[cfg(test)]
                    if trimmed.is_ok() {
                        crate::durable_kill::hit("witness-prune-after");
                    }
                    if trimmed.is_err() {
                        next = None;
                    }
                }
            }
        })
        .map_err(|_| Error::unavailable("could not start the witness publisher"))?;
    Ok(())
}

struct WitnessStore {
    store: Arc<dyn Store>,
    generation: u64,
    key: Arc<RecordKey>,
}

#[derive(Default)]
struct Scan {
    newest: Option<Checkpoint>,
    end: u64,
    kept: u64,
    unparseable: bool,
    conflict: bool,
    found: bool,
}

impl WitnessStore {
    fn new(lease: &StoreLease, key: &Arc<RecordKey>) -> Self {
        Self {
            store: lease.store.clone(),
            generation: lease.generation,
            key: key.clone(),
        }
    }

    fn scan(&self, want: Option<(Position, Checkpoint)>) -> Result<Scan, Error> {
        let mut result = Scan::default();
        while let Ok(from) = Position::try_from(result.end + 1) {
            let ReadResult::Records {
                records,
                unparseable,
            } = layer(self.store.read(
                CHANNEL,
                from,
                ReadLimit {
                    records: PAGE,
                    before: None,
                },
            ))?
            else {
                return Err(Error::unavailable(
                    "the store did not return its witness records",
                ));
            };
            if records.len() > PAGE {
                return Err(Error::unavailable("witness read exceeded its bound"));
            }
            let more = records.len() == PAGE;
            for record in records {
                if record.position.get() <= result.end {
                    return Err(Error::unavailable(
                        "the store returned witness records out of order",
                    ));
                }
                result.end = record.position.get();
                if let Some(at) = decode(&self.key, &record.data) {
                    result.kept = result.end;
                    if result.newest.is_none_or(|seen| seen.sequence < at.sequence) {
                        result.newest = Some(at);
                        result.conflict = false;
                    } else if let Some(previous) = result.newest
                        && previous.sequence == at.sequence
                        && previous != at
                    {
                        result.conflict = true;
                    }
                    result.found |= want == Some((record.position, at));
                }
            }
            result.unparseable |= unparseable;
            if unparseable || !more {
                break;
            }
        }
        Ok(result)
    }

    fn next_position(&self) -> Result<Position, Error> {
        let scan = self.scan(None)?;
        if scan.unparseable || scan.kept < scan.end {
            self.keep(1..scan.kept + 1)?;
        }
        self.settle()?;
        Position::try_from(scan.kept + 1)
            .map_err(|_| Error::unavailable("witness positions are exhausted"))
    }

    fn publish(
        &self,
        at: Checkpoint,
        next: Option<Position>,
        minimum: u64,
    ) -> Result<Position, Error> {
        let position = match next {
            Some(next) => next,
            None => Position::try_from(self.next_position()?.get().max(minimum))?,
        };
        let record = Record {
            position,
            data: encode(&self.key, at),
        };
        #[cfg(test)]
        crate::durable_kill::hit("witness-before-append");
        layer(self.store.append(self.generation, CHANNEL, &[record]))?;
        #[cfg(test)]
        crate::durable_kill::hit("witness-appended");
        self.settle()?;
        #[cfg(test)]
        crate::durable_kill::hit("witness-settled");
        if !self.scan(Some((position, at)))?.found {
            return Err(Error::unavailable("published witness cannot be read back"));
        }
        #[cfg(test)]
        crate::durable_kill::hit("witness-read-back");
        Ok(position)
    }

    fn settle(&self) -> Result<(), Error> {
        let settled = layer(self.store.claim(Claim::Settle {
            generation: self.generation,
            channel: CHANNEL,
        }))?;
        if !matches!(settled, Claimed::Done) {
            return Err(Error::unavailable("the store did not settle its witness"));
        }
        Ok(())
    }

    fn keep(&self, range: std::ops::Range<u64>) -> Result<(), Error> {
        let trimmed = layer(self.store.trim(self.generation, CHANNEL, Trim::Keep(range)))?;
        if trimmed != Trimmed::Done {
            return Err(Error::unavailable("the store did not trim its witness"));
        }
        Ok(())
    }
}

pub(crate) fn encode(key: &RecordKey, at: Checkpoint) -> Bytes {
    let mut encoded = Vec::with_capacity(72);
    encoded.extend_from_slice(&at.sequence.to_be_bytes());
    encoded.extend_from_slice(&at.running_hash);
    encoded.extend_from_slice(&key.witness_check(at.sequence, &at.running_hash));
    encoded.into()
}

pub(crate) fn decode(key: &RecordKey, bytes: &[u8]) -> Option<Checkpoint> {
    let bytes: &[u8; 72] = bytes.try_into().ok()?;
    let sequence = u64::from_be_bytes(bytes[..8].try_into().unwrap());
    let running_hash = bytes[8..40].try_into().unwrap();
    if sequence == 0 || !key.verify_witness(sequence, &running_hash, &bytes[40..]) {
        return None;
    }
    Some(Checkpoint {
        sequence,
        running_hash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal_entry::Entry;
    use malak_journal::{Save, StateFolder};
    use malak_store_markdown::MarkdownStore;
    use malak_testkit::unix_socket::SocketDir;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;

    fn claim(path: &Path, key: &Arc<RecordKey>) -> WitnessStore {
        let store = Arc::new(MarkdownStore::new(path, false));
        let Claimed::Taken(owner) = store.claim(Claim::Take).unwrap() else {
            panic!("expected an exclusive claim");
        };
        WitnessStore {
            store,
            generation: owner.generation,
            key: key.clone(),
        }
    }

    fn checkpoint(sequence: u64) -> Checkpoint {
        Checkpoint {
            sequence,
            running_hash: [sequence as u8; 32],
        }
    }

    fn writable_journal(path: &Path) -> malak_journal::Journal {
        StateFolder::acquire(path)
            .unwrap()
            .inspect()
            .unwrap()
            .activate()
            .unwrap()
    }

    #[test]
    fn a_torn_tail_cannot_hide_a_later_witness() {
        let root = SocketDir::new().unwrap();
        let path = root.path().join("messages");
        let key = Arc::new(RecordKey::new(&[7; 32]));
        let first = checkpoint(1);
        let store = claim(&path, &key);
        let original = store.publish(first, None, 1).unwrap();
        drop(store);
        let channel = fs::read_dir(&path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|extension| extension == "md"))
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(channel)
            .unwrap()
            .write_all(b"torn witness bytes")
            .unwrap();

        let store = claim(&path, &key);
        let newer = checkpoint(2);
        let published = store.publish(newer, None, 1).unwrap();

        assert!(store.scan(Some((original, first))).unwrap().found);
        let scanned = store.scan(Some((published, newer))).unwrap();
        assert!(scanned.found);
        assert_eq!(scanned.newest, Some(newer));
        assert!(!scanned.unparseable);
    }

    #[test]
    fn a_store_witness_ahead_of_the_inspected_journal_is_refused() {
        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        let path = root.path().join("messages");
        let key = Arc::new(RecordKey::new(&[7; 32]));
        let mut journal = writable_journal(&state);
        let original = journal.append(b"first save", Save::OnDisk).unwrap();
        drop(journal);
        let store = claim(&path, &key);
        store.publish(checkpoint(2), None, 1).unwrap();
        drop(store);
        let workers = Arc::new(Workers::default());
        let (lease, _) =
            StoreLease::take(MarkdownStore::new(path, false), workers.clone(), true).unwrap();
        let inspected = StateFolder::acquire(&state).unwrap().inspect().unwrap();

        let result = check_start(&inspected, &lease, &key, &workers);

        assert_eq!(result.unwrap_err().outcome(), crate::Outcome::Refused);
        drop(inspected);
        let the_same = StateFolder::acquire(&state).unwrap().inspect().unwrap();
        assert_eq!(the_same.loaded().head, original);
    }

    #[test]
    fn restart_refuses_an_older_copy_beside_a_published_witness() {
        use crate::journal_entry::StartupEntry;
        use crate::{Grant, Limits, Owner};

        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        let older = root.path().join("older");
        let storage = root.path().join("records");
        let owner = Owner::create_durable(
            MarkdownStore::new(&storage, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        owner.enrol("new-worker", Grant::default()).unwrap();
        drop(owner);
        let latest = StateFolder::acquire(&state).unwrap().inspect().unwrap();
        let StartupEntry::Keys(keys) =
            StartupEntry::decode(latest.entries().next().unwrap().1).unwrap()
        else {
            panic!("expected startup keys");
        };
        let key = Arc::new(RecordKey::new(&keys.record));
        let mut prior = writable_journal(&older);
        for (_, bytes) in latest.entries().take(4) {
            prior.append(bytes, Save::OnDisk).unwrap();
        }
        let original = prior.head();
        drop(prior);
        let store = claim(&storage, &key);
        store.publish(latest.loaded().head, None, 1).unwrap();
        drop(store);

        let refused = Owner::restart_durable(
            MarkdownStore::new(&storage, false),
            Limits::default(),
            &older,
        );

        assert_eq!(refused.err().unwrap().outcome(), crate::Outcome::Refused);
        assert_eq!(
            StateFolder::acquire(older)
                .unwrap()
                .inspect()
                .unwrap()
                .loaded()
                .head,
            original
        );
    }

    #[test]
    fn a_divergent_newer_journal_cannot_override_the_published_witness() {
        use crate::journal_entry::StartupEntry;
        use crate::{Grant, Limits, Owner};
        use std::collections::BTreeMap;

        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        let branch = root.path().join("branch");
        let storage = root.path().join("records");
        let board = Owner::create_durable(
            MarkdownStore::new(&storage, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        drop(board);
        let initial = StateFolder::acquire(&state).unwrap().inspect().unwrap();
        let StartupEntry::Keys(keys) =
            StartupEntry::decode(initial.entries().next().unwrap().1).unwrap()
        else {
            panic!("expected startup keys");
        };
        let key = Arc::new(RecordKey::new(&keys.record));
        let mut other = writable_journal(&branch);
        for (_, bytes) in initial.entries() {
            other.append(bytes, Save::OnDisk).unwrap();
        }
        drop(initial);
        let original = Owner::restart_durable(
            MarkdownStore::new(&storage, false),
            Limits::default(),
            &state,
        )
        .unwrap();
        original.enrol("new-worker", Grant::default()).unwrap();
        // The startup channel checks can still hold the state lock after drop.
        original.stop(original.status(None, None).run).unwrap();
        drop(original);
        let latest = StateFolder::acquire(&state).unwrap().inspect().unwrap();
        let published = latest.loaded().head;
        let store = claim(&storage, &key);
        store.publish(published, None, 1).unwrap();
        drop(store);
        for sequence in other.head().sequence..=published.sequence {
            let mut run = [0; 16];
            run[8..].copy_from_slice(&sequence.to_be_bytes());
            other
                .append(&Entry::Run(run).encode(), Save::OnDisk)
                .unwrap();
        }
        let handed = other.head();
        assert!(handed.sequence > published.sequence);
        drop(other);
        let snapshot = || {
            fs::read_dir(&branch)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), fs::read(entry.path()).unwrap())
                })
                .collect::<BTreeMap<_, _>>()
        };

        for with_handed_value in [false, true] {
            if with_handed_value {
                writable_journal(&branch)
                    .append(&Entry::WitnessHanded(handed).encode(), Save::OnDisk)
                    .unwrap();
            }
            let before = snapshot();
            for _ in 0..2 {
                let error = Owner::restart_durable(
                    MarkdownStore::new(&storage, false),
                    Limits::default(),
                    &branch,
                )
                .err()
                .unwrap();
                assert_eq!(error.outcome(), crate::Outcome::Refused);
                assert!(error.to_string().contains("store's witness"));
                assert_eq!(snapshot(), before);
            }
        }
        drop(latest);
        assert!(
            Owner::restart_durable(
                MarkdownStore::new(&storage, false),
                Limits::default(),
                &state,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_witness_pulse_cannot_keep_the_state_folder_locked_after_drop() {
        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        let workers = Arc::new(Workers::default());
        let store = MarkdownStore::new(root.path().join("messages"), false);
        let (mut lease, _) = StoreLease::take(store, workers, true).unwrap();
        lease.install_journal(writable_journal(&state), Checkpoint::EMPTY);
        let lease = Arc::new(lease);
        let pulse = Arc::downgrade(lease.witness_journal()).upgrade().unwrap();

        drop(lease);

        StateFolder::acquire(&state).expect("a witness pulse still owns the state lock");
        drop(pulse);
    }

    #[test]
    fn the_publisher_records_read_back_without_publishing_its_own_markers() {
        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        let path = root.path().join("messages");
        let key = Arc::new(RecordKey::new(&[7; 32]));
        let workers = Arc::new(Workers::default());
        let (mut lease, _) =
            StoreLease::take(MarkdownStore::new(path, false), workers, true).unwrap();
        lease.install_journal(writable_journal(&state), Checkpoint::EMPTY);
        lease.save(Entry::Run([1; 16])).unwrap();
        let journal = lease.witness_journal();

        start(&lease, &key, VecDeque::new()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let confirmed = loop {
            let at = journal.confirmed();
            if at != Checkpoint::EMPTY {
                break at;
            }
            assert!(
                Instant::now() < deadline,
                "publisher did not confirm its witness"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(confirmed.sequence, 1);
        assert!(journal.hand().unwrap().is_none());
        let record = WitnessStore::new(&lease, &key).scan(None).unwrap();
        assert_eq!(record.newest, Some(confirmed));
    }
}
