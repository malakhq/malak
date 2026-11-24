//! Key replacement races at the saved-key boundary.

use super::*;
use crate::{Action, LocalClient, LocalParticipant, Outcome};
use malak_layers::Transport;
use malak_mechanism_key::Key;
use malak_store_markdown::MarkdownStore;
use malak_testkit::unix_socket::SocketDir;
use malak_transport_unix::UnixTransport;
use std::sync::mpsc;

mod live_keys;
mod owner_frames;

const WAIT: Duration = Duration::from_secs(5);

struct Fixture {
    root: SocketDir,
    owner: Owner,
    entrances: LocalEntrances,
}

impl Fixture {
    fn new() -> Self {
        Self::with_mechanism(Arc::new(Key))
    }

    fn with_mechanism(mechanism: Arc<dyn Mechanism>) -> Self {
        let root = SocketDir::new().unwrap();
        let (owner, entrances) = Owner::create_local(
            MarkdownStore::new(root.path().join("records"), true),
            Limits::default(),
            root.path().join("state"),
            LocalStartup {
                owner_address: format!("unix:{}/o", root.path().display()),
                lead_address: format!("unix:{}/l", root.path().display()),
                general_address: root.address(),
                lead_handover: root.path().join("lead"),
                mechanism_name: "key".into(),
            },
            mechanism,
        )
        .unwrap();
        Self {
            root,
            owner,
            entrances,
        }
    }

    fn enrol(&self, name: &str) -> ParticipantId {
        self.enrol_with(name, [])
    }

    fn enrol_with(&self, name: &str, actions: impl IntoIterator<Item = Action>) -> ParticipantId {
        let mut grant = Grant::default();
        grant.allow("lead".parse().unwrap(), actions);
        self.entrances
            .enrol(name, grant, self.root.path().join(name), None)
            .unwrap()
    }

    fn accept(
        &self,
        unix: &Arc<UnixTransport>,
        address: &str,
    ) -> Result<LocalParticipant<UnixTransport>, Error> {
        let link = unix.accept(address)?;
        self.entrances
            .open(Entrance::General, unix.clone(), link)?
            .attach_participant()
    }

    fn saved_fingerprint(&self, id: &ParticipantId) -> String {
        self.owner.status(Some(id), None).participants[0]
            .fingerprint
            .clone()
            .unwrap()
    }

    fn assert_attach(&self, credential: HandoverFile, allowed: bool) {
        let address = credential.address().to_owned();
        let entrance = credential.binding().entrance;
        let mechanism =
            (entrance == Entrance::General).then(|| Arc::new(Key) as Arc<dyn Mechanism>);
        let listener = Arc::new(UnixTransport::bind(&address).unwrap());
        std::thread::scope(|tasks| {
            let server = tasks.spawn(|| {
                let link = listener.accept(&address).unwrap();
                let result = self
                    .entrances
                    .open(entrance, listener.clone(), link)
                    .and_then(|link| link.attach_participant());
                assert_eq!(result.is_ok(), allowed);
            });
            let result = LocalClient::participant(listener.clone(), credential, mechanism);
            assert_eq!(result.is_ok(), allowed);
            server.join().unwrap();
        });
    }

    fn pause_after_saving(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (saved, reached) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let provision = self.entrances.provisioning().unwrap();
        *provision.after_rekey_save.lock() = Some(Box::new(move || {
            saved.send(()).unwrap();
            release.recv_timeout(WAIT).unwrap();
        }));
        (reached, resume)
    }
}

fn fingerprint(file: &HandoverFile) -> String {
    let public = KeyPair::public_for(file.private_key()).unwrap();
    crate::authority::fingerprint(&public)
}

#[test]
fn overlapping_lead_rekeys_leave_the_last_saved_key_in_the_handover() {
    let fixture = Fixture::new();
    let id = ParticipantId::lead();
    let path = fixture.root.path().join("lead");
    let original = fixture.saved_fingerprint(&id);
    let (saved, resume) = fixture.pause_after_saving();

    std::thread::scope(|tasks| {
        let first = tasks.spawn(|| fixture.entrances.rekey(&id, &original, &path));
        saved.recv_timeout(WAIT).unwrap();
        let interim = fixture.saved_fingerprint(&id);
        assert_ne!(interim, original);
        assert_eq!(fingerprint(&HandoverFile::read(&path).unwrap()), original);
        let gate = fixture
            .owner
            .board
            .table
            .check()
            .authority
            .rekey_gate(&id)
            .unwrap();
        assert!(
            gate.try_lock().is_none(),
            "first rekey released its gate before publishing"
        );
        let (started, requested) = mpsc::channel();
        let (finished, result) = mpsc::channel();
        let (id, path, entrances) = (&id, &path, &fixture.entrances);
        let second = tasks.spawn(move || {
            started.send(()).unwrap();
            finished.send(entrances.rekey(id, &interim, path)).unwrap();
        });
        requested.recv_timeout(WAIT).unwrap();
        let waited = result.recv_timeout(Duration::from_millis(100));

        resume.send(()).unwrap();

        assert!(matches!(waited, Err(mpsc::RecvTimeoutError::Timeout)));
        first.join().unwrap().unwrap();
        assert_eq!(result.recv_timeout(WAIT).unwrap().unwrap(), *path);
        second.join().unwrap();
    });

    let final_file = HandoverFile::read(&path).unwrap();
    let final_key = fixture.saved_fingerprint(&id);
    assert_ne!(final_key, original);
    assert_eq!(fingerprint(&final_file), final_key);
    fixture.assert_attach(final_file, true);
}

#[test]
fn revocation_wins_after_a_rekey_has_saved_its_new_key() {
    let fixture = Fixture::new();
    let id = fixture.enrol("worker");
    let path = fixture.root.path().join("worker");
    let previous = fixture.saved_fingerprint(&id);
    let (saved, resume) = fixture.pause_after_saving();

    std::thread::scope(|tasks| {
        let replacement = tasks.spawn(|| fixture.entrances.rekey(&id, &previous, &path));
        saved.recv_timeout(WAIT).unwrap();

        fixture.owner.revoke(&id).unwrap();

        resume.send(()).unwrap();
        assert_eq!(replacement.join().unwrap().unwrap(), path);
    });
    assert!(fixture.owner.status(Some(&id), None).participants[0].revoked);
    let file = HandoverFile::read(&path).unwrap();
    assert_ne!(fingerprint(&file), previous);
    fixture.assert_attach(file, false);
}

#[test]
fn a_failed_handover_after_rekey_cannot_block_revoking_either_participant() {
    let fixture = Fixture::new();
    let id = fixture.enrol("worker");
    let peer = fixture.enrol("peer");
    let target = fixture.root.path().join("replacement");
    let previous = fixture.saved_fingerprint(&id);
    let (saved, resume) = fixture.pause_after_saving();

    std::thread::scope(|tasks| {
        let replacement = tasks.spawn(|| fixture.entrances.rekey(&id, &previous, &target));
        saved.recv_timeout(WAIT).unwrap();
        std::fs::create_dir(&target).unwrap();
        let (revoked, results) = mpsc::channel();
        let (owner, id, peer) = (&fixture.owner, &id, &peer);
        tasks.spawn(move || {
            revoked
                .send([owner.revoke(id), owner.revoke(peer)])
                .unwrap()
        });

        let before_releasing_write = results.recv_timeout(Duration::from_secs(2));
        resume.send(()).unwrap();

        assert!(
            before_releasing_write
                .unwrap()
                .into_iter()
                .all(|outcome| outcome.is_ok())
        );
        assert_eq!(
            replacement.join().unwrap().unwrap_err().outcome(),
            Outcome::Unavailable
        );
        assert!(target.is_dir());
    });
}
