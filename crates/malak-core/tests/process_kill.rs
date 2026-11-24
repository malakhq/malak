//! Kill and restart the board at observable durable store boundaries.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_core::{GetReply, Limits, Owner, Position, SendRequest};
use malak_layers::{
    Claim, Claimed, Error, ReadLimit, ReadResult, Record, Store, StoreInfo, Trim, Trimmed,
};
use malak_store_markdown::MarkdownStore;
use malak_testkit::unix_socket::SocketDir;
use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD_ROOT: &str = "MALAK_STORE_CHECKPOINT_CHILD";
const BODY: &[u8] = b"persisted before the process died";

fn message() -> SendRequest<'static> {
    SendRequest::note("lead", "will-retry", BODY)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    Append,
    Trim,
    Remove,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Trim => "trim",
            Self::Remove => "remove",
        }
    }

    fn read(name: &str) -> Self {
        match name {
            "append" => Self::Append,
            "trim" => Self::Trim,
            "remove" => Self::Remove,
            _ => panic!("unknown checkpoint operation"),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Boundary {
    Before,
    After,
}

impl Boundary {
    fn name(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

fn kill_at(root: &Path, operation: Operation, boundary: Boundary, synced: bool) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "store_checkpoint_child"])
        .env(CHILD_ROOT, root)
        .env("MALAK_CHECKPOINT_OPERATION", operation.name())
        .env("MALAK_CHECKPOINT_BOUNDARY", boundary.name())
        .env("MALAK_CHECKPOINT_SYNCED", synced.to_string());
    malak_testkit::process::kill_after_checkpoint(&mut command, &root.join("checkpoint"));
}

fn restart(root: &Path, synced: bool) -> Owner {
    Owner::restart_durable(
        MarkdownStore::new(root.join("records"), synced),
        Limits::default(),
        root.join("state"),
    )
    .unwrap()
}

#[test]
fn a_whole_append_is_adopted_after_the_board_is_killed_before_its_head_save() {
    for synced in [false, true] {
        let root = SocketDir::new().unwrap();
        kill_at(root.path(), Operation::Append, Boundary::After, synced);
        let owner = restart(root.path(), synced);

        let lead = owner.lead();
        let retry = lead.send(message()).unwrap();
        assert!(retry.duplicate);
        assert_eq!(retry.position.get(), 1);
        assert!(matches!(lead.get("lead", 1).unwrap(),
            GetReply::Message(message) if message.body.as_ref() == BODY && message.receipt.creation == retry.creation));
    }
}

#[test]
fn a_kill_before_store_append_leaves_the_position_retryable() {
    for synced in [false, true] {
        let root = SocketDir::new().unwrap();
        kill_at(root.path(), Operation::Append, Boundary::Before, synced);
        let owner = restart(root.path(), synced);

        let retry = owner.lead().send(message()).unwrap();
        assert!(!retry.duplicate);
        assert_eq!(retry.position.get(), 1);
    }
}

#[test]
fn killing_trim_at_either_store_boundary_preserves_the_remaining_history() {
    for (synced, boundary) in cases() {
        let root = SocketDir::new().unwrap();
        kill_at(root.path(), Operation::Trim, boundary, synced);
        let owner = restart(root.path(), synced);

        let lead = owner.lead();
        assert!(matches!(
            lead.get("lead", 1).unwrap(),
            GetReply::Trimmed { before: 2, .. }
        ));
        assert!(matches!(lead.get("lead", 2).unwrap(),
            GetReply::Message(message) if message.body.as_ref() == BODY));
    }
}

#[test]
fn killing_removal_at_either_store_boundary_leaves_the_name_reusable() {
    for (synced, boundary) in cases() {
        let root = SocketDir::new().unwrap();
        kill_at(root.path(), Operation::Remove, boundary, synced);
        let owner = restart(root.path(), synced);

        let lead = owner.lead();
        assert!(matches!(lead.get("tasks", 1).unwrap(), GetReply::Absent));
        let retry = lead
            .send(SendRequest::note("tasks", "fresh", b"new creation"))
            .unwrap();
        assert_eq!(retry.position.get(), 1);
    }
}

fn cases() -> impl Iterator<Item = (bool, Boundary)> {
    [false, true]
        .into_iter()
        .flat_map(|synced| [Boundary::Before, Boundary::After].map(|boundary| (synced, boundary)))
}

#[test]
fn store_checkpoint_child() {
    let Some(path) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let root = Path::new(&path);
    let operation = Operation::read(&std::env::var("MALAK_CHECKPOINT_OPERATION").unwrap());
    let boundary = match std::env::var("MALAK_CHECKPOINT_BOUNDARY").unwrap().as_str() {
        "before" => Boundary::Before,
        "after" => Boundary::After,
        _ => panic!("unknown checkpoint boundary"),
    };
    let synced = std::env::var("MALAK_CHECKPOINT_SYNCED").unwrap() == "true";
    let owner = Owner::create_durable(
        PauseStore {
            store: MarkdownStore::new(root.join("records"), synced),
            checkpoint: root.join("checkpoint"),
            operation,
            boundary,
        },
        Limits::default(),
        root.join("state"),
    )
    .unwrap();
    let lead = owner.lead();
    match operation {
        Operation::Append => {
            let _ = lead.send(message());
        }
        Operation::Trim => {
            let old = lead.send(SendRequest::note("lead", "old", b"old")).unwrap();
            lead.send(message()).unwrap();
            let _ = owner.trim("lead", old.creation, 2);
        }
        Operation::Remove => {
            let old = lead
                .send(SendRequest::note("tasks", "old", b"old"))
                .unwrap();
            let _ = owner.remove_channel("tasks", old.creation);
        }
    }
    loop {
        std::thread::park();
    }
}

struct PauseStore {
    store: MarkdownStore,
    checkpoint: PathBuf,
    operation: Operation,
    boundary: Boundary,
}

impl PauseStore {
    fn pause(&self, operation: Operation, boundary: Boundary, channel: &str) {
        if operation == self.operation && boundary == self.boundary && channel != "_witness" {
            malak_testkit::process::pause_at_checkpoint(&self.checkpoint);
        }
    }
}

impl Store for PauseStore {
    fn describe(&self) -> StoreInfo {
        self.store.describe()
    }

    fn claim(&self, request: Claim<'_>) -> Result<Claimed, Error> {
        self.store.claim(request)
    }

    fn append(&self, generation: u64, channel: &str, records: &[Record]) -> Result<(), Error> {
        self.pause(Operation::Append, Boundary::Before, channel);
        self.store.append(generation, channel, records)?;
        self.pause(Operation::Append, Boundary::After, channel);
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
        let operation = match &trim {
            Trim::Keep(range) if range.is_empty() => Operation::Remove,
            _ => Operation::Trim,
        };
        self.pause(operation, Boundary::Before, channel);
        let trimmed = self.store.trim(generation, channel, trim)?;
        self.pause(operation, Boundary::After, channel);
        Ok(trimmed)
    }
}
