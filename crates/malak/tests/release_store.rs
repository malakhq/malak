//! Release cases on the three stock stores through the board's normal API.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak::{GetReply, HeaderValue, Limits, Outcome, Owner, Position, Receipt, SendRequest};
use malak_layers::{
    Claim, Claimed, Error, ReadLimit, ReadResult, Record, Store, StoreInfo, Trim, Trimmed,
};
use malak_store_markdown::MarkdownStore;
use malak_store_memory::MemoryStore;
use malak_store_sqlite::SqliteStore;
use malak_testkit::unix_socket::SocketDir;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

const HISTORY: &str = "history";

#[derive(Clone, Copy, Debug)]
enum Backend {
    Memory,
    Markdown,
    Sqlite,
}

const BACKENDS: [Backend; 3] = [Backend::Memory, Backend::Markdown, Backend::Sqlite];

// Hold the concrete store alongside the board so a test can change its
// records independently of the channel head and the board's record check.
#[derive(Clone)]
struct StoreAccess {
    store: Arc<dyn Store>,
    generation: Arc<AtomicU64>,
}

impl StoreAccess {
    fn new(store: impl Store + 'static) -> Self {
        Self {
            store: Arc::new(store),
            generation: Arc::default(),
        }
    }

    fn records(&self, channel: &str) -> Vec<Record> {
        let ReadResult::Records {
            records,
            unparseable: false,
        } = self
            .read(
                channel,
                Position::FIRST,
                ReadLimit {
                    records: 8,
                    before: None,
                },
            )
            .unwrap()
        else {
            panic!("fixture could not read whole records");
        };
        records
    }

    fn overwrite(&self, channel: &str, records: &[Record]) {
        let generation = self.generation.load(Ordering::Relaxed);
        self.trim(generation, channel, Trim::Keep(1..1)).unwrap();
        self.append(generation, channel, records).unwrap();
    }
}

impl Store for StoreAccess {
    fn describe(&self) -> StoreInfo {
        self.store.describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        let claimed = self.store.claim(mode)?;
        if let Claimed::Taken(owner) = &claimed {
            self.generation.store(owner.generation, Ordering::Relaxed);
        }
        Ok(claimed)
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        self.store.append(generation, channel, records)
    }

    fn read(&self, channel: &str, from: Position, limit: ReadLimit) -> Result<ReadResult, Error> {
        self.store.read(channel, from, limit)
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        self.store.trim(generation, channel, trim)
    }
}

struct Fixture {
    root: SocketDir,
    backend: Backend,
    store: StoreAccess,
}

impl Fixture {
    fn new(backend: Backend) -> Self {
        let root = SocketDir::new().unwrap();
        let path = root.path().join("store");
        let store = match backend {
            Backend::Memory => StoreAccess::new(MemoryStore::default()),
            Backend::Markdown => StoreAccess::new(MarkdownStore::new(path, false)),
            Backend::Sqlite => StoreAccess::new(SqliteStore::new(path)),
        };
        Self {
            root,
            backend,
            store,
        }
    }

    fn create(&self) -> Owner {
        let mut limits = Limits::default();
        limits.minimum_free_disk_bytes = 0;
        match self.backend {
            Backend::Memory => Owner::in_memory(self.store.clone(), limits).unwrap(),
            Backend::Markdown | Backend::Sqlite => {
                Owner::create_durable(self.store.clone(), limits, self.root.path().join("state"))
                    .unwrap()
            }
        }
    }

    fn restart(&self, board: Owner) -> Owner {
        board.stop(board.status(None, None).run).unwrap();
        drop(board);
        Owner::restart_durable(
            self.store.clone(),
            Limits::default(),
            self.root.path().join("state"),
        )
        .unwrap()
    }
}

fn store_name(receipt: Receipt) -> String {
    format!("{HISTORY}/_{}", receipt.creation)
}

fn seed(board: &Owner) -> Receipt {
    let lead = board.lead();
    let mut last = None;
    for id in ["first", "middle", "last"] {
        last = Some(
            lead.send(SendRequest::note(HISTORY, id, id.as_bytes()))
                .unwrap(),
        );
    }
    last.unwrap()
}

#[test]
fn hostile_fields_cannot_create_or_collide_with_channels_in_any_store() {
    for backend in BACKENDS {
        let fixture = Fixture::new(backend);
        let board = fixture.create();
        let lead = board.lead();
        let long = "a".repeat(129);
        let controls = "a\u{1b}[31m";
        let valid = lead
            .send(SendRequest::note(HISTORY, "first", b"original"))
            .unwrap();
        let channels_before = board.status(None, None).channels.len();

        for channel in [
            "../history",
            "history/../history",
            "history//alias",
            "History",
            "history/./alias",
            "_history",
            controls,
            &long,
        ] {
            assert_eq!(
                lead.send(SendRequest::note(channel, "first", b"invalid"))
                    .unwrap_err()
                    .outcome(),
                Outcome::Refused,
                "{backend:?} {channel:?}"
            );
        }
        for id in ["../first", "a/first", controls, &long] {
            assert_eq!(
                lead.send(SendRequest::note(HISTORY, id, b"invalid"))
                    .unwrap_err()
                    .outcome(),
                Outcome::Refused,
                "{backend:?} {id:?}"
            );
        }
        let long_header = format!("x-{}", "h".repeat(2048));
        for key in [
            "../x-name",
            "x-\u{1b}[31m",
            controls,
            "sender",
            &long_header,
        ] {
            let headers = [(key, HeaderValue::Text("forged"))];
            let request = SendRequest {
                headers: &headers,
                ..SendRequest::note(HISTORY, "second", b"invalid")
            };
            assert_eq!(
                lead.send(request).unwrap_err().outcome(),
                Outcome::Refused,
                "{backend:?} {key:?}"
            );
        }
        let headers = [("x-display", HeaderValue::Text(controls))];
        let request = SendRequest {
            headers: &headers,
            ..SendRequest::note(HISTORY, "second", b"invalid")
        };
        assert_eq!(
            lead.send(request).unwrap_err().outcome(),
            Outcome::Refused,
            "{backend:?}"
        );

        assert_eq!(
            board.status(None, None).channels.len(),
            channels_before,
            "{backend:?}"
        );
        assert_eq!(
            fixture.store.records(&store_name(valid)).len(),
            1,
            "{backend:?}"
        );
        let GetReply::Message(original) = lead.get(HISTORY, 1).unwrap() else {
            panic!("original record disappeared in {backend:?}");
        };
        assert_eq!(original.body.as_ref(), b"original");
    }
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Changed,
    Removed,
    EarlierCreation,
}

#[test]
fn a_changed_record_stops_every_store_at_each_position() {
    check_damage(Damage::Changed);
}

#[test]
fn a_missing_record_stops_every_store_at_each_position() {
    check_damage(Damage::Removed);
}

#[test]
fn a_record_rolled_back_to_an_earlier_creation_stops_every_store_at_each_position() {
    check_damage(Damage::EarlierCreation);
}

fn check_damage(damage: Damage) {
    for backend in BACKENDS {
        for index in 0..3 {
            let fixture = Fixture::new(backend);
            let board = fixture.create();
            let old = if matches!(damage, Damage::EarlierCreation) {
                let receipt = seed(&board);
                let records = fixture.store.records(&store_name(receipt));
                board.remove_channel(HISTORY, receipt.creation).unwrap();
                records
            } else {
                Vec::new()
            };
            let receipt = seed(&board);
            let name = store_name(receipt);
            let mut records = fixture.store.records(&name);
            let position = records[index].position.get();
            match damage {
                Damage::Changed => {
                    let mut data = records[index].data.to_vec();
                    *data.last_mut().unwrap() ^= 1;
                    records[index].data = data.into();
                }
                Damage::Removed => {
                    records.remove(index);
                }
                Damage::EarlierCreation => records[index] = old[index].clone(),
            }

            fixture.store.overwrite(&name, &records);

            let lead = board.lead();
            let context = format!("{backend:?} {damage:?} at {position}");
            assert_eq!(
                lead.get(HISTORY, position).unwrap_err().outcome(),
                Outcome::Unavailable,
                "{context}"
            );
            assert!(
                board.status(None, Some(&HISTORY.parse().unwrap())).channels[0].stopped,
                "{context}"
            );
            assert_eq!(
                lead.read(&[HISTORY], 3, Duration::ZERO)
                    .unwrap_err()
                    .outcome(),
                Outcome::Unavailable,
                "{context}"
            );
        }
    }
}

#[test]
fn an_extra_record_cannot_take_a_position_already_in_history() {
    for backend in BACKENDS {
        let fixture = Fixture::new(backend);
        let board = fixture.create();
        let receipt = seed(&board);
        let channel = store_name(receipt);
        let records = fixture.store.records(&channel);
        let generation = fixture.store.generation.load(Ordering::Relaxed);
        for record in &records {
            let forged = Record {
                position: record.position,
                data: b"extra".as_slice().into(),
            };
            assert!(
                fixture
                    .store
                    .append(generation, &channel, &[forged])
                    .is_err(),
                "{backend:?} {}",
                record.position
            );
        }
        assert_eq!(fixture.store.records(&channel), records);
    }
}

#[test]
fn an_extra_tail_record_is_never_delivered() {
    for backend in BACKENDS {
        let fixture = Fixture::new(backend);
        let board = fixture.create();
        let receipt = seed(&board);
        let channel = store_name(receipt);
        let generation = fixture.store.generation.load(Ordering::Relaxed);
        let forged = Record {
            position: 4.try_into().unwrap(),
            data: b"extra".as_slice().into(),
        };
        fixture
            .store
            .append(generation, &channel, &[forged])
            .unwrap();
        assert_eq!(fixture.store.records(&channel).len(), 4);

        let lead = board.lead();
        assert!(
            matches!(lead.get(HISTORY, 4).unwrap(), GetReply::NeverWritten { .. }),
            "{backend:?}"
        );
        assert_eq!(
            lead.read(&[HISTORY], 4, Duration::ZERO)
                .unwrap()
                .messages
                .len(),
            3,
            "{backend:?}"
        );
        drop(lead);
        if !matches!(backend, Backend::Memory) {
            let restarted = fixture.restart(board);
            let lead = restarted.lead();
            assert!(
                matches!(lead.get(HISTORY, 4).unwrap(), GetReply::NeverWritten { .. }),
                "{backend:?}"
            );
            let next = lead
                .send(SendRequest::note(HISTORY, "next", b"new record"))
                .unwrap();
            assert_eq!(next.position.get(), 4, "{backend:?}");
        }
    }
}

#[test]
fn printable_path_like_extension_headers_are_opaque_in_every_store() {
    for backend in BACKENDS {
        let fixture = Fixture::new(backend);
        let board = fixture.create();
        let lead = board.lead();
        let headers = [("x-../alias", HeaderValue::Text("../somewhere"))];
        let request = SendRequest {
            headers: &headers,
            ..SendRequest::note(HISTORY, "one", b"original body")
        };

        lead.send(request).unwrap();

        let GetReply::Message(message) = lead.get(HISTORY, 1).unwrap() else {
            panic!("record was not delivered from {backend:?}");
        };
        assert_eq!(
            message.headers,
            [malak::Header::Extension {
                key: "x-../alias".into(),
                value: "../somewhere".into(),
            }]
        );
    }
}
