//! Kill the process at durable change and startup boundaries.

use crate::journal_entry::Entry;
use crate::{
    Action, CreationId, Error, GetReply, Grant, Limits, LocalEntrances, LocalStartup, Owner,
    ParticipantId, Position, SendRequest,
};
use malak_layers::{
    Claim, Claimed, ReadLimit, ReadResult, Record, Store, StoreInfo, Trim, Trimmed,
};
use malak_mechanism_key::Key;
use malak_store_markdown::MarkdownStore;
use malak_testkit::process::{kill_after_checkpoint, pause_at_checkpoint};
use malak_testkit::unix_socket::SocketDir;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const ROOT: &str = "MALAK_DURABLE_KILL_ROOT";
const OPERATION: &str = "MALAK_DURABLE_KILL_OPERATION";
const STAGE: &str = "MALAK_DURABLE_KILL_STAGE";
const SYNCED: &str = "MALAK_DURABLE_KILL_SYNCED";

mod repair;
mod replay_records;

pub(crate) fn hit(stage: &str) {
    static CHILD: OnceLock<Option<(PathBuf, String)>> = OnceLock::new();
    if let Some((root, requested)) =
        CHILD.get_or_init(|| Some((std::env::var_os(ROOT)?.into(), std::env::var(STAGE).ok()?)))
        && requested == stage
    {
        pause_at_checkpoint(&root.join(stage));
    }
}

pub(crate) fn repair_hit(creation: CreationId, step: &str) {
    hit(&format!("repair-{creation}-{step}"));
}

pub(crate) fn handover(path: &Path, stage: &str) {
    let Some(root) = std::env::var_os(ROOT) else {
        return;
    };
    let root = Path::new(&root);
    if path == root.join("worker") {
        hit(&format!("worker-file-{stage}"));
    }
    if path == root.join("lead") {
        hit(&format!("lead-file-{stage}"));
    }
    if path == root.join("state/owner.credential") {
        hit(&format!("owner-file-{stage}"));
    }
}

pub(crate) fn before_save(entry: &Entry<'_>) -> Option<&'static str> {
    let tag = match entry {
        Entry::Reserve { .. } => "reserve",
        Entry::Head { .. } => "head",
        Entry::Ack { .. } => "ack",
        Entry::Trim { .. } => "trim-begin",
        Entry::TrimDone(_) => "trim-done",
        Entry::Cut { .. } => "cut-begin",
        Entry::CutDone { .. } => "cut-done",
        Entry::Create { .. } => "create",
        Entry::Remove(_) => "remove-begin",
        Entry::RemoveDone(_) => "remove-done",
        Entry::Enrol { .. } | Entry::LocalEnrol { .. } => "enrol",
        Entry::Grant { .. } => "grant",
        Entry::Revoke { .. } => "revoke",
        Entry::Rekey { .. } => "rekey",
        Entry::Run(_) => "run",
        Entry::Keys { .. } => "keys",
        Entry::Lead { .. } | Entry::LocalLead { .. } => "lead",
        Entry::Complete => "complete",
        _ => return None,
    };
    hit(&format!("{tag}-before"));
    Some(tag)
}

pub(crate) fn after_save(tag: Option<&str>) {
    if let Some(tag) = tag {
        hit(&format!("{tag}-after"));
    }
}

const RESTART: &[&str] = &[
    "state-locked",
    "journal-read",
    "state-replayed",
    "store-claimed",
    "witness-checked",
    "journal-activated",
    "run-before",
    "run-after",
    "authority-loaded",
];

fn start(root: &Path, synced: bool, cut: bool) -> (Owner, LocalEntrances) {
    start_with_store(
        root,
        TestStore {
            store: MarkdownStore::new(root.join("store"), synced),
            cut,
        },
    )
}

fn start_with_store(root: &Path, store: impl Store + 'static) -> (Owner, LocalEntrances) {
    Owner::create_local(
        store,
        Limits::default(),
        root.join("state"),
        LocalStartup {
            owner_address: format!("unix:{}/o", root.display()),
            lead_address: format!("unix:{}/l", root.display()),
            general_address: format!("unix:{}/s", root.display()),
            lead_handover: root.join("lead"),
            mechanism_name: "key".into(),
        },
        Arc::new(Key),
    )
    .unwrap()
}

fn kill(root: &Path, synced: bool, operation: &str, stage: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "durable_kill::durable_checkpoint_child"])
        .env(ROOT, root)
        .env(OPERATION, operation)
        .env(STAGE, stage)
        .env(SYNCED, synced.to_string());
    kill_after_checkpoint(&mut child, &root.join(stage));
}

fn sample() -> SendRequest<'static> {
    SendRequest::note("history", "sample", b"after opening")
}

fn verify(
    operation: &str,
    steps: &[&str],
    check_operation: impl Fn(&Path, &Owner, &LocalEntrances, &ParticipantId, &str),
) {
    for synced in [false, true] {
        for (index, step) in steps.iter().enumerate() {
            let scratch = SocketDir::new().unwrap();
            let root = scratch.path();
            let (board, entrances) = start(root, synced, false);
            let lead = board.lead();
            let kept = lead
                .send(SendRequest::note("lead", "prior", b"unchanged"))
                .unwrap();
            for id in ["first", "second"] {
                lead.send(SendRequest::note("history", id, id.as_bytes()))
                    .unwrap();
            }
            let participant = entrances
                .enrol("worker", Grant::default(), root.join("worker"), None)
                .unwrap();
            if operation == "rekey" {
                let current = board.status(Some(&participant), None);
                std::fs::write(
                    root.join("fingerprint"),
                    current.participants[0].fingerprint.as_ref().unwrap(),
                )
                .unwrap();
            }
            drop(lead);
            drop(entrances);
            drop(board);

            kill(root, synced, operation, step);
            kill(root, synced, "restart", RESTART[index % RESTART.len()]);
            let (resumed, entrances) = start(root, synced, false);

            let GetReply::Message(prior) = resumed.lead().get("lead", kept.position.get()).unwrap()
            else {
                panic!("lost a reported record at {operation} {step}");
            };
            assert_eq!(prior.receipt, kept, "{operation} {step}");
            assert_eq!(prior.body.as_ref(), b"unchanged");
            check_operation(root, &resumed, &entrances, &participant, step);
            assert!(
                resumed
                    .status(None, None)
                    .channels
                    .iter()
                    .all(|channel| !channel.stopped)
            );
        }
    }
}

#[test]
fn send_journal_crashes_preserve_the_receipt_at_the_same_position() {
    verify(
        "send",
        &[
            "reserve-before",
            "reserve-after",
            "append-before",
            "append-after",
            "head-before",
            "head-after",
        ],
        |_, board, _, _, step| {
            let retry = board.lead().send(sample()).unwrap();
            assert_eq!(retry.position.get(), 3, "{step}");
            assert_eq!(
                retry.duplicate,
                step.starts_with("head") || step == "append-after",
                "{step}"
            );
        },
    );
}

#[test]
fn acknowledgement_journal_crashes_preserve_exactly_saved_progress() {
    verify(
        "ack",
        &["ack-before", "ack-after"],
        |_, board, _, _, step| {
            let reply = board.lead().read(&["history"], 1, Duration::ZERO).unwrap();
            assert_eq!(
                reply.messages[0].receipt.position.get(),
                if step == "ack-before" { 1 } else { 2 }
            );
        },
    );
}

#[test]
fn trim_journal_crashes_preserve_exactly_the_saved_floor() {
    verify(
        "trim",
        &[
            "trim-begin-before",
            "trim-begin-after",
            "trim-store-before",
            "trim-store-after",
            "trim-done-before",
            "trim-done-after",
        ],
        |_, board, _, _, step| {
            let lead = board.lead();
            let first = lead.get("history", 1).unwrap();
            assert_eq!(
                matches!(first, GetReply::Trimmed { before: 2, .. }),
                step != "trim-begin-before"
            );
            assert!(
                matches!(lead.get("history", 2).unwrap(), GetReply::Message(message) if message.body.as_ref() == b"second")
            );
        },
    );
}

#[test]
fn cut_crashes_do_not_change_counted_history_or_resurrect_an_append() {
    verify(
        "cut",
        &[
            "cut-begin-before",
            "cut-begin-after",
            "cut-trim-before",
            "cut-trim-after",
            "cut-settled",
            "cut-done-before",
            "cut-done-after",
        ],
        |_, board, _, _, step| {
            let lead = board.lead();
            assert!(
                matches!(lead.get("history", 2).unwrap(), GetReply::Message(message) if message.body.as_ref() == b"second")
            );
            let receipt = lead.send(sample()).unwrap();
            assert_eq!(receipt.position.get(), 3, "{step}");
            assert!(!receipt.duplicate, "{step}");
        },
    );
}

#[test]
fn creation_journal_crashes_publish_only_the_saved_creation() {
    verify(
        "create",
        &["create-before", "create-after"],
        |_, board, _, _, step| {
            let status = board.status(None, Some(&"created".parse().unwrap()));
            assert_eq!(status.channels.len(), usize::from(step == "create-after"));
        },
    );
}

#[test]
fn removal_journal_crashes_leave_only_unsaved_removals_visible() {
    verify(
        "remove",
        &[
            "remove-begin-before",
            "remove-begin-after",
            "remove-store-before",
            "remove-store-after",
            "remove-done-before",
            "remove-done-after",
        ],
        |_, board, _, _, step| {
            let result = board.lead().get("history", 1).unwrap();
            assert_eq!(
                matches!(result, GetReply::Absent),
                step != "remove-begin-before"
            );
        },
    );
}

#[test]
fn authority_journal_crashes_publish_only_saved_grants() {
    verify(
        "grant",
        &["grant-before", "grant-after"],
        |_, board, _, id, step| {
            let status = board.status(Some(id), None);
            assert_eq!(
                status.participants[0].grant.rules().len(),
                usize::from(step == "grant-after")
            );
        },
    );
}

#[test]
fn authority_journal_crashes_publish_only_saved_revocations() {
    verify(
        "revoke",
        &["revoke-before", "revoke-after"],
        |_, board, _, id, step| {
            assert_eq!(board.handle(id).is_err(), step == "revoke-after");
            if step == "revoke-after" {
                board.revoke(id).unwrap();
                assert!(board.handle(id).is_err());
            }
        },
    );
}

#[test]
fn authority_journal_crashes_publish_only_saved_key_replacements() {
    use crate::{HandoverFile, LocalClient};
    use malak_layers::Transport;
    use malak_proto::{Entrance, KeyPair};
    use malak_transport_unix::UnixTransport;

    verify(
        "rekey",
        &[
            "worker-file-prepared",
            "worker-file-keys",
            "rekey-before",
            "rekey-after",
            "worker-file-temporary",
            "worker-file-published",
            "worker-file-directory",
        ],
        |root, board, entrances, id, step| {
            let before = std::fs::read_to_string(root.join("fingerprint")).unwrap();
            let status = board.status(Some(id), None);
            let saved = status.participants[0].fingerprint.as_deref().unwrap();
            let should_keep_old = matches!(
                step,
                "worker-file-prepared" | "worker-file-keys" | "rekey-before"
            );
            assert_eq!(saved == before, should_keep_old, "{step}");
            let path = root.join("worker");
            let handover = HandoverFile::read(&path).unwrap();
            let on_disk = crate::authority::fingerprint(
                &KeyPair::public_for(handover.private_key()).unwrap(),
            );
            let address = handover.address().to_owned();
            let unix = Arc::new(UnixTransport::bind(&address).unwrap());
            let attach = |file, allowed| {
                std::thread::scope(|tasks| {
                    let server = tasks.spawn(|| {
                        let link = unix.accept(&address).unwrap();
                        let result = entrances
                            .open(Entrance::General, unix.clone(), link)
                            .and_then(|link| link.attach_participant());
                        assert_eq!(result.is_ok(), allowed, "{step}");
                    });
                    let client = LocalClient::participant(unix.clone(), file, Some(Arc::new(Key)));
                    assert_eq!(client.is_ok(), allowed, "{step}");
                    server.join().unwrap();
                });
            };

            if on_disk != saved {
                attach(handover, false);
                entrances.rekey(id, saved, &path).unwrap();
                attach(HandoverFile::read(&path).unwrap(), true);
            } else {
                attach(handover, true);
            }
        },
    );
}

#[test]
fn every_startup_boundary_can_be_killed_and_restarted() {
    for synced in [false, true] {
        let scratch = SocketDir::new().unwrap();
        let root = scratch.path();
        let (board, _) = start(root, synced, false);
        let kept = board
            .lead()
            .send(SendRequest::note("lead", "kept", b"before restart"))
            .unwrap();
        drop(board);
        for step in RESTART {
            kill(root, synced, "restart", step);
        }
        let (resumed, _) = start(root, synced, false);
        assert!(
            matches!(resumed.lead().get("lead", 1).unwrap(), GetReply::Message(message) if message.receipt == kept)
        );
    }
}

#[test]
fn a_first_start_killed_at_each_boundary_can_be_repeated() {
    for synced in [false, true] {
        for step in [
            "state-locked",
            "journal-read",
            "first-store-claimed",
            "first-journal-activated",
            "keys-before",
            "keys-after",
            "lead-before",
            "lead-after",
            "lead-file-temporary",
            "lead-file-published",
            "lead-file-directory",
            "owner-file-temporary",
            "owner-file-published",
            "owner-file-directory",
            "complete-before",
            "complete-after",
            "run-before",
            "run-after",
        ] {
            let scratch = SocketDir::new().unwrap();
            let root = scratch.path();
            kill(root, synced, "restart", step);

            let (board, _) = start(root, synced, false);
            let lead = crate::HandoverFile::read(root.join("lead")).unwrap();
            assert!(lead.id().is_lead(), "{step}");
            assert!(
                crate::OwnerCredential::read(root.join("state")).is_ok(),
                "{step}"
            );
            assert_eq!(
                board
                    .lead()
                    .send(SendRequest::note("lead", "first", b"first"))
                    .unwrap()
                    .position
                    .get(),
                1,
                "{step}"
            );
        }
    }
}

#[test]
fn witness_publication_crashes_allow_restart_with_old_or_removed_records() {
    use malak_journal::StateFolder;

    for synced in [false, true] {
        for step in [
            "witness-before-handing",
            "witness-handed",
            "witness-before-append",
            "witness-appended",
            "witness-settled",
            "witness-read-back",
            "witness-confirmed",
        ] {
            let scratch = SocketDir::new().unwrap();
            let root = scratch.path();
            let (board, entrances) = start(root, synced, false);
            let receipt = board.lead().send(sample()).unwrap();
            board.stop(board.status(None, None).run).unwrap();
            drop((board, entrances));
            kill(root, synced, "witness", "witness-confirmed");
            let old = witness_records(root, synced, None).remove(0);
            let inspected = StateFolder::acquire_existing(root.join("state"))
                .unwrap()
                .inspect()
                .unwrap();
            let archive = inspected.archive_payload().unwrap();
            inspected.activate().unwrap().compact(&archive).unwrap();

            kill(root, synced, "witness", step);

            for replacement in [None, Some(&[][..]), Some(std::slice::from_ref(&old))] {
                if let Some(records) = replacement {
                    witness_records(root, synced, Some(records));
                }
                let (board, entrances) = start(root, synced, false);
                let retry = board.lead().send(sample()).unwrap();
                assert_eq!(
                    retry,
                    crate::Receipt {
                        duplicate: true,
                        ..receipt
                    },
                    "{step}"
                );
                board.stop(board.status(None, None).run).unwrap();
                drop((board, entrances));
            }
        }
    }
}

#[test]
fn killing_witness_pruning_keeps_the_right_journal_restartable() {
    for synced in [false, true] {
        for step in ["witness-prune-before", "witness-prune-after"] {
            let scratch = SocketDir::new().unwrap();
            let root = scratch.path();
            let (board, entrances) = start(root, synced, false);
            let receipt = board.lead().send(sample()).unwrap();
            board.stop(board.status(None, None).run).unwrap();
            drop((board, entrances));
            let previous = seed_full_witness_history(root, synced);

            kill(root, synced, "witness", step);

            let records = witness_records(root, synced, None);
            assert!(!records.is_empty());
            if step == "witness-prune-before" {
                assert!(records.iter().any(|record| record == &previous));
                assert!(records.len() > crate::witness::RETAINED);
            } else {
                assert!(!records.iter().any(|record| record == &previous));
                assert_eq!(records.len(), crate::witness::RETAINED);
            }
            for replacement in [None, Some(&[][..]), Some(std::slice::from_ref(&previous))] {
                if let Some(records) = replacement {
                    witness_records(root, synced, Some(records));
                }
                let (board, entrances) = start(root, synced, false);
                assert_eq!(
                    board.lead().send(sample()).unwrap(),
                    crate::Receipt {
                        duplicate: true,
                        ..receipt
                    }
                );
                board.stop(board.status(None, None).run).unwrap();
                drop((board, entrances));
            }
        }
    }
}

fn seed_full_witness_history(root: &Path, synced: bool) -> Record {
    use malak_journal::{Save, StateFolder};

    let (key, _) = replay_records::journal_evidence(root);
    let folder = StateFolder::acquire_existing(root.join("state")).unwrap();
    let mut journal = folder.inspect().unwrap().activate().unwrap();
    let (store, generation) = claim_records(root, synced);
    let channel = crate::witness::CHANNEL;
    let first = read_records(&store, channel)
        .last()
        .map_or(1, |record| record.position.get() + 1);
    let mut oldest = None;
    for (index, number) in (first..).take(crate::witness::RETAINED).enumerate() {
        let creator = ParticipantId::lead();
        let name = format!("witness-seed-{index}").parse().unwrap();
        let at = journal
            .append(
                &Entry::Create {
                    name: &name,
                    creation: CreationId::from_bytes(crate::random().unwrap()),
                    creator: &creator,
                }
                .encode(),
                Save::OnDisk,
            )
            .unwrap();
        journal
            .append(&Entry::WitnessHanded(at).encode(), Save::OnDisk)
            .unwrap();
        let position = Position::try_from(number).unwrap();
        let record = Record {
            position,
            data: crate::witness::encode(&key, at),
        };
        store
            .append(generation, channel, std::slice::from_ref(&record))
            .unwrap();
        journal
            .append(
                &Entry::WitnessPublished { at, position }.encode(),
                Save::OnDisk,
            )
            .unwrap();
        if index == 0 {
            oldest = Some(record);
        }
    }

    let inspected = journal.inspect().unwrap();
    let archive = inspected.archive_payload().unwrap();
    inspected.activate().unwrap().compact(&archive).unwrap();
    oldest.unwrap()
}

fn claim_records(root: &Path, synced: bool) -> (MarkdownStore, u64) {
    let store = MarkdownStore::new(root.join("store"), synced);
    let Claimed::Taken(owner) = store.claim(Claim::Take).unwrap() else {
        panic!("expected an ordinary store claim");
    };
    (store, owner.generation)
}

fn read_records(store: &MarkdownStore, channel: &str) -> Vec<Record> {
    let ReadResult::Records {
        records,
        unparseable: false,
    } = store
        .read(
            channel,
            Position::FIRST,
            ReadLimit {
                records: 64,
                before: None,
            },
        )
        .unwrap()
    else {
        panic!("expected parseable Markdown records for {channel}");
    };
    records
}

fn replace_records(store: &MarkdownStore, generation: u64, channel: &str, records: &[Record]) {
    assert_eq!(
        store.trim(generation, channel, Trim::Keep(1..1)).unwrap(),
        Trimmed::Done
    );
    if !records.is_empty() {
        store.append(generation, channel, records).unwrap();
    }
}

fn witness_records(root: &Path, synced: bool, replace: Option<&[Record]>) -> Vec<Record> {
    let (store, generation) = claim_records(root, synced);
    let channel = crate::witness::CHANNEL;
    if let Some(records) = replace {
        replace_records(&store, generation, channel, records);
    }
    read_records(&store, channel)
}

#[test]
fn durable_checkpoint_child() {
    let Some(directory) = std::env::var_os(ROOT) else {
        return;
    };
    let root = Path::new(&directory);
    let operation = std::env::var(OPERATION).unwrap();
    let synced = std::env::var(SYNCED).unwrap() == "true";
    match operation.as_str() {
        "repair" => repair::apply_child(root, synced, false),
        "repair-owner-confirmed" => repair::apply_child(root, synced, true),
        _ => {}
    }
    let (board, entrances) = start(root, synced, operation == "cut");
    let lead = board.lead();
    let channel = || {
        board
            .status(None, Some(&"history".parse().unwrap()))
            .channels[0]
            .creation
    };
    let worker = || {
        board
            .status(None, None)
            .participants
            .into_iter()
            .find(|entry| entry.name.as_str() == "worker")
            .unwrap()
    };
    match operation.as_str() {
        "send" | "cut" => {
            let _ = lead.send(sample());
        }
        "repair-tail" => {
            let _ = lead.send(repair::pending_send());
        }
        "ack" => {
            lead.read(&["history"], 1, Duration::ZERO).unwrap();
            lead.ack("history", channel(), 1).unwrap();
        }
        "trim" => {
            board.trim("history", channel(), 2).unwrap();
        }
        "create" => {
            board.create_channel("created").unwrap();
        }
        "remove" => {
            board.remove_channel("history", channel()).unwrap();
        }
        "grant" => {
            let mut grant = Grant::default();
            grant.allow("history".parse().unwrap(), [Action::Read]);
            board.replace_grant(&worker().id, grant).unwrap();
        }
        "revoke" => board.revoke(&worker().id).unwrap(),
        "rekey" => {
            let entry = worker();
            entrances
                .rekey(
                    &entry.id,
                    entry.fingerprint.as_deref().unwrap(),
                    root.join("worker"),
                )
                .unwrap();
        }
        "witness" => loop {
            std::thread::park();
        },
        "restart" => {}
        _ => panic!("unknown operation {operation}"),
    }
    panic!("did not reach {}", std::env::var(STAGE).unwrap());
}

struct TestStore {
    store: MarkdownStore,
    cut: bool,
}

impl Store for TestStore {
    fn describe(&self) -> StoreInfo {
        self.store.describe()
    }
    fn claim(&self, request: Claim<'_>) -> Result<Claimed, Error> {
        match self.store.claim(request)? {
            Claimed::Repair {
                ownership,
                channels,
                ..
            } => Ok(Claimed::Repair {
                ownership,
                channels,
                place: malak_layers::StorePlace::Established(b"fixture".to_vec()),
            }),
            result => Ok(result),
        }
    }
    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        let history = channel.starts_with("history/");
        if history {
            hit("append-before");
        }
        if self.cut && history {
            return Err(Error::unavailable("injected append interruption"));
        }
        self.store.append(generation, channel, records)?;
        if history {
            hit("append-after");
        }
        Ok(())
    }
    fn read(
        &self,
        channel: &str,
        position: Position,
        limit: ReadLimit,
    ) -> Result<ReadResult, Error> {
        self.store.read(channel, position, limit)
    }
    fn trim(&self, generation: u64, channel: &str, trim: Trim<'_>) -> Result<Trimmed, Error> {
        let marks = match &trim {
            Trim::Keep(range) if range.is_empty() => {
                Some(("remove-store-before", "remove-store-after"))
            }
            Trim::Keep(range) if range.end == Position::MAX + 1 => {
                Some(("trim-store-before", "trim-store-after"))
            }
            _ => None,
        }
        .filter(|_| channel.starts_with("history/"));
        if let Some((before, _)) = marks {
            hit(before);
        }
        let result = self.store.trim(generation, channel, trim)?;
        if let Some((_, after)) = marks {
            hit(after);
        }
        Ok(result)
    }
}
