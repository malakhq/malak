use super::*;
use malak_core::{Error, HeaderValue, Kind, Outcome, Position};
use malak_testkit::tasks;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(8);

#[test]
fn a_pending_inbox_restore_only_blocks_new_implicit_sends_by_its_reader() {
    let fixture = Fixture::new();
    let board = fixture.board(true);
    let worker = board
        .enrol(
            "worker",
            common::grant("**", &[Action::Read, Action::Ack, Action::Send]),
        )
        .unwrap();
    let id = worker.id().clone();
    let inbox = id.inbox();
    let task = board
        .lead()
        .send(SendRequest {
            kind: Kind::Task,
            ..SendRequest::note(inbox.as_str(), "task", b"work")
        })
        .unwrap();
    worker.read(&[inbox.as_str()], 1, Duration::ZERO).unwrap();
    worker
        .ack(inbox.as_str(), task.creation, task.position.get())
        .unwrap();
    let retry = SendRequest::note("lead", "retry", b"progress");
    let mut original = worker.send(retry).unwrap();
    original.duplicate = true;
    drop((worker, board));

    let (store, entered, release) = BlockedStore::new(&fixture, Boundary::InboxRead);
    let board = Owner::restart_durable(store, Limits::default(), &fixture.state).unwrap();
    entered.recv_timeout(WAIT).unwrap();
    let worker = board.handle(&id).unwrap();
    let new = SendRequest::note("lead", "new", b"progress");
    let error = worker.send(new).unwrap_err();
    assert_eq!(error.outcome(), Outcome::Unavailable);
    assert!(
        error
            .to_string()
            .contains("working task trace is still being restored")
    );
    assert_eq!(worker.send(retry).unwrap(), original);
    let lead = board.lead();
    let independent = lead
        .send(SendRequest::note("lead", "separate", b""))
        .unwrap();
    assert_eq!(independent.position.get(), original.position.get() + 1);
    let explicit = worker
        .send(SendRequest {
            headers: &[("trace", HeaderValue::Text("FF"))],
            ..SendRequest::note("lead", "explicit", b"")
        })
        .unwrap();
    assert_eq!(explicit.trace.unwrap().as_str(), "FF");
    let reply = worker
        .send(SendRequest {
            headers: &[(
                "reply_to",
                HeaderValue::ReplyTo {
                    channel: inbox.as_str(),
                    creation: task.creation,
                    position: task.position.get(),
                },
            )],
            ..SendRequest::note("lead", "reply", b"")
        })
        .unwrap();
    assert_eq!(reply.trace, None);

    release.send(()).unwrap();
    lead.get(inbox.as_str(), task.position.get()).unwrap();
    assert_eq!(worker.send(new).unwrap().trace, task.trace);
}

#[test]
fn a_blocked_startup_check_leaves_other_channels_available() {
    tasks::spawn(exercise_blocked_startup)
        .wait(WAIT * 3)
        .expect("blocked startup check scenario timed out");
}

fn exercise_blocked_startup() {
    let fixture = Fixture::new();
    let before = fixture.board(true);
    before
        .lead()
        .send(SendRequest::note("blocked", "first", b"waiting"))
        .unwrap();
    before
        .lead()
        .send(SendRequest::note("independent", "first", b"available"))
        .unwrap();
    before.stop(before.status(None, None).run).unwrap();
    drop(before);

    let (store, entered, release) = BlockedStore::new(&fixture, Boundary::StartupRead);
    let board = Owner::restart_durable(store, Limits::default(), &fixture.state).unwrap();
    entered
        .recv_timeout(WAIT)
        .expect("startup read did not reach the store");
    let lead = board.lead();

    let separate = lead
        .send(SendRequest::note(
            "independent",
            "second",
            b"still available",
        ))
        .unwrap();
    let GetReply::Message(message) = lead.get("independent", 2).unwrap() else {
        panic!("the independent channel could not read its new message");
    };
    assert_eq!(message.receipt, separate);
    assert_eq!(
        lead.get("blocked", 1).unwrap_err().outcome(),
        Outcome::Unavailable
    );
    release.send(()).unwrap();
    let GetReply::Message(message) = lead.get("blocked", 1).unwrap() else {
        panic!("the blocked startup read did not recover");
    };
    assert_eq!(message.body.as_ref(), b"waiting");
}

#[test]
fn a_blocked_witness_append_does_not_delay_durable_changes() {
    tasks::spawn(exercise_blocked_witness)
        .wait(WAIT * 3)
        .expect("blocked witness scenario timed out");
}

fn exercise_blocked_witness() {
    let fixture = Fixture::new();
    let (store, entered, release) = BlockedStore::new(&fixture, Boundary::WitnessAppend);
    let board = Owner::create_durable(store, Limits::default(), &fixture.state).unwrap();
    entered
        .recv_timeout(WAIT)
        .expect("witness append did not reach the store");

    let lead = board.lead();
    let first = lead
        .send(SendRequest::note("lead", "first", b"before trim"))
        .unwrap();
    let second = lead
        .send(SendRequest::note("lead", "second", b"kept"))
        .unwrap();
    board.trim("lead", first.creation, 2).unwrap();
    let enrolled = board.enrol("new-worker", Grant::default()).unwrap();
    release.send(()).unwrap();
    let id = enrolled.id().clone();
    board.stop(board.status(None, None).run).unwrap();
    drop((board, lead, enrolled));

    let board = fixture.restart();
    assert!(board.handle(&id).is_ok());
    assert!(matches!(
        board.lead().get("lead", 1).unwrap(),
        GetReply::Trimmed { before: 2, .. }
    ));
    assert!(
        matches!(board.lead().get("lead", 2).unwrap(), GetReply::Message(message) if message.receipt == second)
    );
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Boundary {
    StartupRead,
    InboxRead,
    WitnessAppend,
}

type Gate = (Sender<()>, Receiver<()>);

struct BlockedStore {
    store: MarkdownStore,
    boundary: Boundary,
    gate: Mutex<Option<Gate>>,
}

impl BlockedStore {
    fn new(fixture: &Fixture, boundary: Boundary) -> (Self, Receiver<()>, Sender<()>) {
        let (entered, observation) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        (
            Self {
                store: MarkdownStore::new(&fixture.storage, true),
                boundary,
                gate: Mutex::new(Some((entered, blocked))),
            },
            observation,
            release,
        )
    }

    fn hold(&self, at: Boundary) -> Result<(), Error> {
        if self.boundary == at {
            let gate = self.gate.lock().unwrap().take();
            if let Some((entered, blocked)) = gate {
                entered.send(()).unwrap();
                blocked
                    .recv_timeout(WAIT * 2)
                    .map_err(|_| Error::unavailable("fixture was not released"))?;
            }
        }
        Ok(())
    }
}

impl Store for BlockedStore {
    fn describe(&self) -> StoreInfo {
        self.store.describe()
    }

    fn claim(&self, mode: Claim<'_>) -> Result<Claimed, Error> {
        self.store.claim(mode)
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        if channel == "_witness" {
            self.hold(Boundary::WitnessAppend)?;
        }
        self.store.append(generation, channel, records)
    }

    fn read(&self, channel: &str, from: Position, bound: ReadLimit) -> Result<ReadResult, Error> {
        if channel.starts_with("blocked/_") {
            self.hold(Boundary::StartupRead)?;
        }
        if channel.starts_with("to/") {
            self.hold(Boundary::InboxRead)?;
        }
        self.store.read(channel, from, bound)
    }

    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        self.store.trim(generation, channel, trim)
    }
}
