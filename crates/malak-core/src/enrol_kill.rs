//! Process checkpoints for enrolment and handover publication.

use crate::{
    GetReply, Grant, HandoverFile, Limits, LocalEntrances, LocalStartup, Owner, SendRequest,
};
use malak_mechanism_key::Key;
use malak_store_markdown::MarkdownStore;
use malak_testkit::process::{kill_after_checkpoint, pause_at_checkpoint};
use malak_testkit::unix_socket::SocketDir;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

const CHILD: &str = "MALAK_ENROL_CHECKPOINT_CHILD";
const STEP: &str = "MALAK_ENROL_CHECKPOINT_STEP";
const KEEP: &[u8] = b"saved before enrolment";

pub(crate) fn checkpoint(path: &Path, phase: &str) {
    if let Ok(step) = std::env::var(STEP)
        && step == phase
        && let Some(root) = std::env::var_os(CHILD)
        && path == Path::new(&root).join("worker")
    {
        pause_at_checkpoint(&Path::new(&root).join(format!("checkpoint-{phase}")));
    }
}

pub(crate) fn restart_checkpoint(state: &Path) {
    if let Some(root) = std::env::var_os(CHILD)
        && state == Path::new(&root).join("state")
    {
        checkpoint(&Path::new(&root).join("worker"), "restart");
    }
}

fn kill_at(root: &Path, step: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "enrol_kill::enrol_checkpoint_child"])
        .env(CHILD, root)
        .env(STEP, step);
    kill_after_checkpoint(&mut child, &root.join(format!("checkpoint-{step}")));
}

fn start(root: &Path) -> (Owner, LocalEntrances) {
    Owner::create_local(
        MarkdownStore::new(root.join("store"), false),
        Limits::default(),
        root.join("state"),
        LocalStartup {
            owner_address: "unix:/malak-test/owner".into(),
            lead_address: "unix:/malak-test/lead".into(),
            general_address: "unix:/malak-test/general".into(),
            lead_handover: root.join("lead"),
            mechanism_name: "key".into(),
        },
        Arc::new(Key),
    )
    .unwrap()
}

#[test]
fn killing_each_enrolment_phase_preserves_saved_messages_and_restores_access() {
    for (step, saved, published) in [
        ("preflight", false, false),
        ("keys", false, false),
        ("authority", true, false),
        ("temporary", true, false),
        ("published", true, true),
        ("directory", true, true),
    ] {
        let root = SocketDir::new().unwrap();
        let path = root.path();
        kill_at(path, step);
        kill_at(path, "restart");

        let (board, entrances) = start(path);
        assert!(matches!(board.lead().get("lead", 1).unwrap(),
            GetReply::Message(message) if message.body.as_ref() == KEEP));
        let worker = path.join("worker");
        assert_eq!(worker.exists(), published, "{step}");
        let status = board.status(None, None);
        let participant = status
            .participants
            .iter()
            .find(|entry| entry.name.as_str() == "worker");
        assert_eq!(participant.is_some(), saved, "{step}");
        if let Some(participant) = participant {
            if !published {
                entrances
                    .rekey(
                        &participant.id,
                        participant.fingerprint.as_deref().unwrap(),
                        &worker,
                    )
                    .unwrap();
            }
            assert_eq!(HandoverFile::read(&worker).unwrap().id(), &participant.id);
        } else {
            entrances
                .enrol("worker", Grant::default(), &worker, None)
                .unwrap();
        }
    }
}

#[test]
fn enrol_checkpoint_child() {
    let Some(root) = std::env::var_os(CHILD) else {
        return;
    };
    let root = Path::new(&root);
    let (board, entrances) = start(root);
    board
        .lead()
        .send(SendRequest::note("lead", "prior", KEEP))
        .unwrap();
    entrances
        .enrol("worker", Grant::default(), root.join("worker"), None)
        .unwrap();
    panic!("enrolment did not pause at the selected phase");
}
