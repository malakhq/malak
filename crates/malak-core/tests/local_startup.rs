//! Durable credential provisioning for local boards.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_core::{
    Grant, HandoverFile, Limits, LocalClient, LocalEntrances, LocalStartup, Outcome, Owner,
    OwnerCredential,
};
use malak_journal::StateFolder;
use malak_layers::{Enrolment, Handover, Identity, Mechanism, Proves, Session, Step, Transport};
use malak_mechanism_key::Key;
use malak_proto::{Entrance, KeyPair, encode_frame};
use malak_store_markdown::MarkdownStore;
use malak_testkit::unix_socket::SocketDir;
use malak_transport_unix::UnixTransport;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod common;
#[path = "local_startup/readers.rs"]
mod readers;
#[path = "local_startup/retries.rs"]
mod retries;
#[path = "local_startup/vm.rs"]
mod vm;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Fixture {
    root: SocketDir,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = SocketDir::new().unwrap();
        let state = root.path().join("state");
        Self { root, state }
    }

    fn start(&self) -> Result<(Owner, LocalEntrances), malak_core::Error> {
        self.start_on(Entrance::Lead, "unix:/local/l")
    }

    fn start_on(
        &self,
        entrance: Entrance,
        address: &str,
    ) -> Result<(Owner, LocalEntrances), malak_core::Error> {
        self.start_at(&self.state, entrance, address)
    }

    fn start_at(
        &self,
        state: &Path,
        entrance: Entrance,
        address: &str,
    ) -> Result<(Owner, LocalEntrances), malak_core::Error> {
        let address_for = |role, default: &str| {
            if role == entrance {
                address.to_owned()
            } else {
                default.to_owned()
            }
        };
        self.start_settings(state, address_for)
    }

    fn start_settings(
        &self,
        state: &Path,
        address_for: impl Fn(Entrance, &str) -> String,
    ) -> Result<(Owner, LocalEntrances), malak_core::Error> {
        self.start_settings_with(state, address_for, Arc::new(Key))
    }

    fn start_settings_with(
        &self,
        state: &Path,
        address_for: impl Fn(Entrance, &str) -> String,
        mechanism: Arc<dyn Mechanism>,
    ) -> Result<(Owner, LocalEntrances), malak_core::Error> {
        self.start_settings_with_limits(state, address_for, mechanism, Limits::default())
    }

    fn start_settings_with_limits(
        &self,
        state: &Path,
        address_for: impl Fn(Entrance, &str) -> String,
        mechanism: Arc<dyn Mechanism>,
        limits: Limits,
    ) -> Result<(Owner, LocalEntrances), malak_core::Error> {
        Owner::create_local(
            MarkdownStore::new(self.root.path().join("store"), false),
            limits,
            state,
            LocalStartup {
                owner_address: address_for(Entrance::Owner, "unix:/local/o"),
                lead_address: address_for(Entrance::Lead, "unix:/local/l"),
                general_address: address_for(Entrance::General, "unix:/local/p"),
                lead_handover: self.root.path().join("lead"),
                mechanism_name: "key".into(),
            },
            mechanism,
        )
    }

    fn startup_prefix(&self, entries: usize) -> PathBuf {
        let state = self.root.path().join("interrupted");
        common::copy_journal_prefix(&self.state, &state, entries);
        state
    }

    fn tags(&self) -> Vec<u8> {
        StateFolder::acquire(&self.state)
            .unwrap()
            .inspect()
            .unwrap()
            .entries()
            .map(|(_, payload)| {
                let mut input = minicbor::Decoder::new(payload);
                input.array().unwrap();
                input.u8().unwrap();
                input.u8().unwrap()
            })
            .collect()
    }
}

#[test]
fn the_same_board_key_is_pinned_in_owner_and_lead_credentials() {
    let fixture = Fixture::new();
    let (owner, entrances) = fixture.start().unwrap();
    let lead = HandoverFile::read(fixture.root.path().join("lead")).unwrap();
    let admin = OwnerCredential::read(&fixture.state).unwrap();

    assert_eq!(lead.binding().entrance, Entrance::Lead);
    assert_eq!(lead.address(), "unix:/local/l");
    assert_eq!(admin.address(), "unix:/local/o");
    assert_eq!(lead.board_key(), admin.board_key());
    assert_ne!(lead.private_key(), admin.private_key());
    drop(entrances);
    drop(owner);
    assert_eq!(fixture.tags(), [0, 16, 2, 3]);
}

#[test]
fn an_interrupted_local_start_replaces_the_saved_lead_key() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    drop(fixture.start_on(Entrance::Lead, &address).unwrap());
    let path = fixture.root.path().join("lead");
    let old = HandoverFile::read(&path).unwrap();
    let interrupted = fixture.startup_prefix(2);

    let (_board, entrances) = fixture
        .start_at(&interrupted, Entrance::Lead, &address)
        .unwrap();
    let current = HandoverFile::read(path).unwrap();
    let transport = Arc::new(UnixTransport::bind(&address).unwrap());

    assert_eq!(old.board_key(), current.board_key());
    std::thread::scope(|tasks| {
        let serve = tasks.spawn(|| {
            let link = transport.accept(&address).unwrap();
            assert!(
                entrances
                    .open(Entrance::Lead, transport.clone(), link)
                    .is_err()
            );
        });
        assert!(LocalClient::participant(transport.clone(), old, None).is_err());
        serve.join().unwrap();
    });
}

#[test]
fn a_completed_initialization_keeps_its_lead_handover_when_the_run_is_missing() {
    let fixture = Fixture::new();
    drop(fixture.start().unwrap());
    let path = fixture.root.path().join("lead");
    let previous = HandoverFile::read(&path).unwrap();
    let interrupted = fixture.startup_prefix(3);
    fs::copy(
        fixture.state.join("owner.credential"),
        interrupted.join("owner.credential"),
    )
    .unwrap();

    drop(
        fixture
            .start_at(&interrupted, Entrance::Lead, "unix:/local/l")
            .unwrap(),
    );

    let lead = HandoverFile::read(path).unwrap();
    assert_eq!(lead.private_key(), previous.private_key());
}

#[test]
fn a_resumed_local_start_refuses_an_unrelated_existing_handover() {
    let fixture = Fixture::new();
    drop(fixture.start().unwrap());
    let interrupted = fixture.startup_prefix(2);
    let path = fixture.root.path().join("lead");
    fs::write(&path, b"unrelated private file").unwrap();

    let result = fixture.start_at(&interrupted, Entrance::Lead, "unix:/local/l");

    assert_eq!(result.err().unwrap().outcome(), Outcome::Refused);
    assert_eq!(fs::read(path).unwrap(), b"unrelated private file");
    let inspected = StateFolder::acquire(interrupted)
        .unwrap()
        .inspect()
        .unwrap();
    assert_eq!(inspected.loaded().head.sequence, 2);
}

#[test]
fn lead_handover_preflight_refuses_start_before_keys_are_saved() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("lead"), b"already exists").unwrap();

    let error = fixture.start().err().unwrap();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(fixture.tags().is_empty());
    assert!(!fixture.root.path().join("store").exists());
}

#[test]
fn a_local_enrolment_has_a_durable_entry_and_a_private_handover() {
    let fixture = Fixture::new();
    let (owner, entrances) = fixture.start().unwrap();
    let path = fixture.root.path().join("worker");

    let id = entrances
        .enrol("worker", Grant::default(), &path, None)
        .unwrap();

    let file = HandoverFile::read(path).unwrap();
    assert_eq!(file.id(), &id);
    assert_eq!(file.mechanism(), "key");
    assert!(owner.handle(&id).is_ok());
    drop(entrances);
    drop(owner);
    assert_eq!(fixture.tags(), [0, 16, 2, 3, 17]);
}

#[test]
fn a_duplicate_enrolment_writes_no_handover_and_changes_no_authority() {
    let fixture = Fixture::new();
    let (owner, entrances) = fixture.start().unwrap();
    entrances
        .enrol(
            "worker",
            Grant::default(),
            fixture.root.path().join("worker"),
            None,
        )
        .unwrap();
    let other = fixture.root.path().join("other");

    let error = entrances
        .enrol("worker", Grant::default(), &other, None)
        .unwrap_err();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert!(!other.exists());
    drop(entrances);
    drop(owner);
    assert_eq!(fixture.tags(), [0, 16, 2, 3, 17]);
}

#[test]
fn an_enrolled_participant_attaches_with_the_file_written_at_enrolment() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (_owner, entrances) = fixture.start_on(Entrance::General, &address).unwrap();
    let path = fixture.root.path().join("worker");
    let id = entrances
        .enrol("worker", Grant::default(), &path, None)
        .unwrap();
    let handover = HandoverFile::read(path).unwrap();
    let transport = Arc::new(UnixTransport::bind(&address).unwrap());

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| {
            let link = transport.accept(&address).unwrap();
            let participant = entrances
                .open(Entrance::General, transport.clone(), link)
                .unwrap()
                .attach_participant()
                .unwrap();
            participant.id().clone()
        });
        let _client =
            LocalClient::participant(transport.clone(), handover, Some(Arc::new(Key))).unwrap();

        assert_eq!(server.join().unwrap(), id);
    });
}

#[test]
fn an_owner_client_uses_the_private_credential_written_at_startup() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (_owner, entrances) = fixture.start_on(Entrance::Owner, &address).unwrap();
    let credential = OwnerCredential::read(&fixture.state).unwrap();
    let transport = Arc::new(UnixTransport::bind(&address).unwrap());

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| {
            let link = transport.accept(&address).unwrap();
            let mut owner = entrances
                .open(Entrance::Owner, transport.clone(), link)
                .unwrap()
                .owner_requests()
                .unwrap();
            let request = owner.next_request().unwrap();
            assert!(owner.reply_if_immediate(&request).unwrap());
        });
        let mut client = LocalClient::owner(transport.clone(), credential).unwrap();
        client
            .send_frame(encode_frame(3, "PING", 0, |_| {}, &[]).unwrap(), TIMEOUT)
            .unwrap();

        let reply = client.read_frame(TIMEOUT).unwrap();

        assert_eq!((reply.op(), reply.rid()), ("OK", 3));
        server.join().unwrap();
    });
}

#[test]
fn owner_enrolment_returns_only_the_id_and_handover_path() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (board, entrances) = fixture.start_on(Entrance::Owner, &address).unwrap();
    let credential = OwnerCredential::read(&fixture.state).unwrap();
    let path = fixture.root.path().join("worker");
    let transport = Arc::new(UnixTransport::bind(&address).unwrap());

    let id = std::thread::scope(|tasks| {
        let server = tasks.spawn(|| {
            let link = transport.accept(&address).unwrap();
            let mut owner = entrances
                .open(Entrance::Owner, transport.clone(), link)
                .unwrap()
                .owner_requests()
                .unwrap();
            let request = owner.next_request().unwrap();
            owner.dispatch(request).unwrap();
        });
        let mut client = LocalClient::owner(transport.clone(), credential).unwrap();
        let (id, returned_path) = client
            .enrol("worker", &Grant::default(), path.to_str().unwrap(), None)
            .unwrap();
        assert_eq!(PathBuf::from(returned_path), path);
        server.join().unwrap();
        id
    });

    assert_eq!(HandoverFile::read(path).unwrap().id(), &id);
    assert!(board.handle(&id).is_ok());
    drop(entrances);
    drop(board);
    assert_eq!(fixture.tags(), [0, 16, 2, 3, 17]);
}

fn fingerprint(file: &HandoverFile) -> String {
    use sha2::{Digest, Sha256};

    let key = KeyPair::public_for(file.private_key()).unwrap();
    Sha256::digest(key)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assert_can_attach(
    entrances: &LocalEntrances,
    entrance: Entrance,
    address: &str,
    files: impl IntoIterator<Item = (HandoverFile, bool)>,
) {
    let transport = Arc::new(UnixTransport::bind(address).unwrap());
    for (file, expected) in files {
        std::thread::scope(|tasks| {
            let server = tasks.spawn(|| {
                let link = transport.accept(address).unwrap();
                let attached = entrances
                    .open(entrance, transport.clone(), link)
                    .and_then(|opening| opening.attach_participant());
                assert_eq!(attached.is_ok(), expected);
            });
            let mechanism = (entrance == Entrance::General)
                .then(|| Arc::new(Key) as Arc<dyn malak_layers::Mechanism>);
            let attached = LocalClient::participant(transport.clone(), file, mechanism);
            assert_eq!(attached.is_ok(), expected);
            server.join().unwrap();
        });
    }
}

#[test]
fn the_lead_can_call_after_general_connections_close_before_attaching() {
    assert_lead_call_after_openings_close(Entrance::General);
}

#[test]
fn the_lead_can_call_after_lead_connections_close_before_attaching() {
    assert_lead_call_after_openings_close(Entrance::Lead);
}

fn assert_lead_call_after_openings_close(entrance: Entrance) {
    let fixture = Fixture::new();
    let general = fixture.root.address();
    let lead = format!("unix:{}", fixture.root.path().join("l").display());
    let (_board, entrances) = fixture
        .start_settings(&fixture.state, |entrance, default| match entrance {
            Entrance::General => general.clone(),
            Entrance::Lead => lead.clone(),
            Entrance::Owner => default.into(),
        })
        .unwrap();
    let lead_transport = Arc::new(UnixTransport::bind(&lead).unwrap());
    let (transport, address) = match entrance {
        Entrance::General => (Arc::new(UnixTransport::bind(&general).unwrap()), &general),
        Entrance::Lead => (lead_transport.clone(), &lead),
        Entrance::Owner => unreachable!(),
    };

    for _ in 0..3 {
        let connections: Vec<_> = (0..8)
            .map(|_| {
                let caller = transport.dial(address).unwrap();
                let link = transport.accept(address).unwrap();
                (caller, link)
            })
            .collect();
        std::thread::scope(|tasks| {
            for (caller, link) in connections {
                let transport = transport.clone();
                let entrances = &entrances;
                tasks.spawn(move || {
                    drop(caller);
                    assert!(entrances.open(entrance, transport, link).is_err());
                });
            }
        });
    }

    let handover = HandoverFile::read(fixture.root.path().join("lead")).unwrap();
    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| {
            let link = lead_transport.accept(&lead).unwrap();
            let participant = entrances
                .open(Entrance::Lead, lead_transport.clone(), link)
                .unwrap()
                .attach_participant()
                .unwrap();
            let _ = participant.serve();
        });
        let mut client = LocalClient::participant(lead_transport.clone(), handover, None).unwrap();
        client.send_frame(read_request(1, 0), TIMEOUT).unwrap();
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "OK");
        drop(client);
        server.join().unwrap();
    });
}

#[test]
fn a_replaced_lead_key_stays_retired_after_restart() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (board, entrances) = fixture.start_on(Entrance::Lead, &address).unwrap();
    let path = fixture.root.path().join("lead");
    let old = HandoverFile::read(&path).unwrap();

    entrances
        .rekey(old.id(), &fingerprint(&old), &path)
        .unwrap();
    let new = HandoverFile::read(&path).unwrap();
    drop(entrances);
    drop(board);
    let (_board, entrances) = fixture.start_on(Entrance::Lead, &address).unwrap();

    assert_can_attach(
        &entrances,
        Entrance::Lead,
        &address,
        [(old, false), (new, true)],
    );
}

#[test]
fn a_running_client_rereads_its_handover_after_failed_attachments() {
    malak_testkit::tasks::spawn(exercise_handover_retry)
        .wait(std::time::Duration::from_secs(35))
        .expect("handover retry did not complete");
}

fn exercise_handover_retry() {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (_board, entrances) = fixture.start_on(Entrance::General, &address).unwrap();
    let path = fixture.root.path().join("worker");
    let next = fixture.root.path().join("replacement");
    let participant = entrances
        .enrol("worker", Grant::default(), &path, None)
        .unwrap();
    let retired = HandoverFile::read(&path).unwrap();
    entrances
        .rekey(&participant, &fingerprint(&retired), &next)
        .unwrap();
    let listener = Arc::new(UnixTransport::bind(&address).unwrap());

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| {
            let attach = || {
                let link = listener.accept(&address).unwrap();
                entrances
                    .open(Entrance::General, listener.clone(), link)
                    .and_then(|opening| opening.attach_participant())
            };
            assert!(attach().is_err(), "the first retired key was accepted");
            assert!(attach().is_err(), "the second retired key was accepted");
            assert!(attach().is_ok(), "the replacement key was refused");
        });

        let replacement = HandoverFile::read(&next).unwrap();
        let attempt = std::cell::Cell::new(0);
        LocalClient::participant_from_file(listener.clone(), &path, |handover| {
            let index = attempt.get() + 1;
            attempt.set(index);
            match index {
                1 => assert_eq!(fingerprint(handover), fingerprint(&retired)),
                2 => {
                    assert_eq!(fingerprint(handover), fingerprint(&retired));
                    // This attempt already loaded the old key.
                    fs::rename(&next, &path).unwrap();
                }
                3 => assert_eq!(fingerprint(handover), fingerprint(&replacement)),
                _ => panic!("the replacement failed to attach"),
            }
            Ok(Some(Arc::new(Key)))
        })
        .expect("the next retry must attach with the replacement handover");

        server.join().unwrap();
    });
}

#[test]
fn repeating_owner_rekey_keeps_the_first_replacement_key_working() {
    let fixture = Fixture::new();
    let owner_address = format!("unix:{}", fixture.root.path().join("o").display());
    let general_address = fixture.root.address();
    let (_board, entrances) = fixture
        .start_settings(&fixture.state, |entrance, default| match entrance {
            Entrance::Owner => owner_address.clone(),
            Entrance::General => general_address.clone(),
            _ => default.into(),
        })
        .unwrap();
    let path = fixture.root.path().join("worker");
    let id = entrances
        .enrol("worker", Grant::default(), &path, None)
        .unwrap();
    let old = HandoverFile::read(&path).unwrap();
    let expected = fingerprint(&old);

    let transport = Arc::new(UnixTransport::bind(&owner_address).unwrap());
    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| {
            let link = transport.accept(&owner_address).unwrap();
            let mut owner = entrances
                .open(Entrance::Owner, transport.clone(), link)
                .unwrap()
                .owner_requests()
                .unwrap();
            for _ in 0..2 {
                let request = owner.next_request().unwrap();
                owner.dispatch(request).unwrap();
            }
        });
        let mut client = LocalClient::owner(
            transport.clone(),
            OwnerCredential::read(&fixture.state).unwrap(),
        )
        .unwrap();

        let destination = path.to_str().unwrap();
        assert_eq!(
            client.rekey(&id, &expected, destination).unwrap(),
            destination
        );
        let after_first = fs::read(&path).unwrap();
        let repeated = client.rekey(&id, &expected, destination).unwrap_err();
        assert_eq!(repeated.outcome(), Outcome::Refused);
        assert_eq!(fs::read(&path).unwrap(), after_first);
        server.join().unwrap();
    });
    let new = HandoverFile::read(&path).unwrap();
    assert_can_attach(
        &entrances,
        Entrance::General,
        &general_address,
        [(old, false), (new, true)],
    );
}

#[test]
fn revocation_does_not_wait_for_a_hung_rekey_mechanism() {
    let fixture = Fixture::new();
    let (entered, starting) = std::sync::mpsc::channel();
    let (release, unblocked) = std::sync::mpsc::channel();
    let mechanism = Arc::new(PausedRekey {
        pause: std::sync::atomic::AtomicBool::new(false),
        entered,
        unblock: std::sync::Mutex::new(unblocked),
    });
    let (board, entrances) = fixture
        .start_settings_with(
            &fixture.state,
            |_, default| default.into(),
            mechanism.clone(),
        )
        .unwrap();
    let path = fixture.root.path().join("worker");
    let id = entrances
        .enrol("worker", Grant::default(), &path, None)
        .unwrap();
    let peer = entrances
        .enrol(
            "peer",
            Grant::default(),
            fixture.root.path().join("peer"),
            None,
        )
        .unwrap();
    let expected = fingerprint(&HandoverFile::read(&path).unwrap());
    let old_file = fs::read(&path).unwrap();
    mechanism
        .pause
        .store(true, std::sync::atomic::Ordering::Release);

    std::thread::scope(|tasks| {
        let replacing = tasks.spawn(|| entrances.rekey(&id, &expected, &path));
        starting.recv_timeout(TIMEOUT).unwrap();
        let (finished, reply) = std::sync::mpsc::channel();
        let (board, id, peer) = (&board, &id, &peer);
        tasks.spawn(move || {
            finished
                .send([board.revoke(id), board.revoke(peer)])
                .unwrap()
        });
        let revoked = reply.recv_timeout(std::time::Duration::from_secs(2));
        release.send(()).unwrap();

        assert!(revoked.unwrap().into_iter().all(|result| result.is_ok()));
        assert_eq!(
            replacing.join().unwrap().unwrap_err().outcome(),
            Outcome::Refused
        );
    });
    assert_eq!(fs::read(path).unwrap(), old_file);
}

struct PausedRekey {
    pause: std::sync::atomic::AtomicBool,
    entered: std::sync::mpsc::Sender<()>,
    unblock: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Mechanism for PausedRekey {
    fn describe(&self) -> Proves {
        Key.describe()
    }

    fn enrol(
        &self,
        id: &malak_core::ParticipantId,
        request: &Enrolment<'_>,
    ) -> Result<Handover, malak_core::Error> {
        if self.pause.swap(false, std::sync::atomic::Ordering::AcqRel) {
            self.entered.send(()).unwrap();
            self.unblock.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
        }
        Key.enrol(id, request)
    }

    fn step(&self, session: &Session<'_>, incoming: &[u8]) -> Result<Step, malak_core::Error> {
        Key.step(session, incoming)
    }

    fn allow(
        &self,
        identity: &Identity<'_>,
        action: malak_core::Action,
        channel: &malak_core::ChannelName,
    ) -> Result<bool, malak_core::Error> {
        Key.allow(identity, action, channel)
    }

    fn revoke(&self, id: &malak_core::ParticipantId) -> Result<(), malak_core::Error> {
        Key.revoke(id)
    }
}

fn with_participant(grant: Grant, use_link: impl FnOnce(&Owner, &mut LocalClient<UnixTransport>)) {
    let fixture = Fixture::new();
    let address = fixture.root.address();
    let (owner, entrances) = fixture.start_on(Entrance::General, &address).unwrap();
    let path = fixture.root.path().join("worker");
    entrances.enrol("worker", grant, &path, None).unwrap();
    let handover = HandoverFile::read(path).unwrap();
    let transport = Arc::new(UnixTransport::bind(&address).unwrap());

    std::thread::scope(|tasks| {
        let server = tasks.spawn(|| serve_participant(&entrances, &address, transport.clone()));
        {
            let mut client =
                LocalClient::participant(transport.clone(), handover, Some(Arc::new(Key))).unwrap();
            use_link(&owner, &mut client);
        }
        let _ = server.join().unwrap();
    });
}

fn serve_participant<T: Transport + 'static>(
    entrances: &LocalEntrances,
    address: &str,
    transport: Arc<T>,
) -> Result<(), malak_core::Error>
where
    T::Connection: 'static,
{
    let link = transport.accept(address)?;
    entrances
        .open(Entrance::General, transport, link)?
        .attach_participant()?
        .serve()
}

fn allow(actions: impl IntoIterator<Item = malak_core::Action>) -> Grant {
    let mut grant = Grant::default();
    grant.allow("lead".parse().unwrap(), actions);
    grant
}

fn field<'a>(frame: &'a malak_proto::Frame, name: &str) -> minicbor::Decoder<'a> {
    let mut input = minicbor::Decoder::new(frame.header());
    for _ in 0..input.map().unwrap().unwrap() {
        if input.str().unwrap() == name {
            return input;
        }
        input.skip().unwrap();
    }
    panic!("missing {name}");
}

fn read_request(rid: u64, wait_ms: u64) -> bytes::Bytes {
    read_channels(rid, &["lead"], None, wait_ms)
}

fn read_channels(rid: u64, channels: &[&str], limit: Option<u64>, wait_ms: u64) -> bytes::Bytes {
    encode_frame(
        rid,
        "READ",
        2 + usize::from(limit.is_some()),
        |out| {
            out.str("channels")
                .unwrap()
                .array(channels.len() as u64)
                .unwrap();
            for name in channels {
                out.str(name).unwrap();
            }
            out.str("wait_ms").unwrap().u64(wait_ms).unwrap();
            if let Some(limit) = limit {
                out.str("limit").unwrap().u64(limit).unwrap();
            }
        },
        &[],
    )
    .unwrap()
}

#[test]
fn the_participant_wire_supports_send_get_read_and_ack() {
    use malak_core::Action::{Ack, Get, Read, Send};
    with_participant(allow([Send, Read, Get, Ack]), |owner, client| {
        let send = encode_frame(
            1,
            "SEND",
            2,
            |out| {
                out.str("channel")
                    .unwrap()
                    .str("lead")
                    .unwrap()
                    .str("id")
                    .unwrap()
                    .str("report")
                    .unwrap();
            },
            b"hello board",
        )
        .unwrap();
        client.send_frame(send, TIMEOUT).unwrap();
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "OK");
        let read = owner
            .lead()
            .read(&["lead"], 1, std::time::Duration::ZERO)
            .unwrap();
        let creation = read.messages[0].receipt.creation.to_string();

        let get = encode_frame(
            2,
            "GET",
            2,
            |out| {
                out.str("channel")
                    .unwrap()
                    .str("lead")
                    .unwrap()
                    .str("position")
                    .unwrap()
                    .u64(1)
                    .unwrap();
            },
            &[],
        )
        .unwrap();
        client.send_frame(get, TIMEOUT).unwrap();
        let got = client.read_frame(TIMEOUT).unwrap();
        assert_eq!((got.op(), got.body()), ("MSG", b"hello board".as_slice()));
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "OK");

        let ack = encode_frame(
            3,
            "ACK",
            3,
            |out| {
                out.str("channel")
                    .unwrap()
                    .str("lead")
                    .unwrap()
                    .str("position")
                    .unwrap()
                    .u64(1)
                    .unwrap()
                    .str("creation")
                    .unwrap()
                    .str(&creation)
                    .unwrap();
            },
            &[],
        )
        .unwrap();

        client.send_frame(ack.clone(), TIMEOUT).unwrap();
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "ERR");

        client.send_frame(read_request(4, 0), TIMEOUT).unwrap();
        let due = client.read_frame(TIMEOUT).unwrap();
        assert_eq!((due.op(), due.body()), ("MSG", b"hello board".as_slice()));
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "OK");

        client.send_frame(ack, TIMEOUT).unwrap();
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "OK");

        client.send_frame(read_request(5, 0), TIMEOUT).unwrap();
        assert_eq!(client.read_frame(TIMEOUT).unwrap().op(), "OK");
    });
}

#[test]
fn eight_waiting_reads_leave_room_for_cancel_and_ping() {
    with_participant(allow([malak_core::Action::Read]), |_, client| {
        for rid in 10..=18 {
            client
                .send_frame(read_request(rid, 30_000), TIMEOUT)
                .unwrap();
        }
        let refused = client.read_frame(TIMEOUT).unwrap();
        assert_eq!((refused.op(), refused.rid()), ("ERR", 18));

        let cancel = encode_frame(
            30,
            "CANCEL",
            1,
            |out| {
                out.str("target").unwrap().u64(12).unwrap();
            },
            &[],
        )
        .unwrap();
        client.send_frame(cancel, TIMEOUT).unwrap();
        client
            .send_frame(encode_frame(31, "PING", 0, |_| {}, &[]).unwrap(), TIMEOUT)
            .unwrap();

        for wanted in [12, 30, 31] {
            let reply = client.read_frame(TIMEOUT).unwrap();
            assert_eq!((reply.op(), reply.rid()), ("OK", wanted));
        }
    });
}

#[test]
fn a_participant_with_no_read_grant_gets_no_message() {
    with_participant(allow([malak_core::Action::Send]), |owner, client| {
        owner
            .lead()
            .send(malak_core::SendRequest::note("lead", "private", b"private"))
            .unwrap();

        client.send_frame(read_request(7, 0), TIMEOUT).unwrap();

        let reply = client.read_frame(TIMEOUT).unwrap();
        assert_eq!((reply.op(), reply.rid(), reply.body()), ("ERR", 7, &[][..]));
    });
}
