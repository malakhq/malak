//! Real process interruptions of the Markdown set-aside and rewrite.

use crate::{MarkdownStore, format};
use bytes::Bytes;
use malak_layers::{Claim, Claimed, KeptRecord, Record, Store, Trim, Trimmed};
use malak_testkit::process::{kill_after_checkpoint, pause_at_checkpoint};
use malak_testkit::unix_socket::SocketDir;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const ROOT: &str = "MALAK_MARKDOWN_REPAIR_KILL_ROOT";
const STEP: &str = "MALAK_MARKDOWN_REPAIR_KILL_STEP";
const CHANNEL: &str = "history";

fn target(step: &str) -> Option<&'static Path> {
    static CHILD: OnceLock<Option<(PathBuf, String)>> = OnceLock::new();
    CHILD
        .get_or_init(|| Some((std::env::var_os(ROOT)?.into(), std::env::var(STEP).ok()?)))
        .as_ref()
        .filter(|(_, wanted)| wanted == step)
        .map(|(root, _)| root.as_path())
}

pub(super) fn checkpoint(step: &str) {
    if let Some(root) = target(step) {
        pause_at_checkpoint(&root.join(step));
    }
}

pub(super) fn torn(file: &mut fs::File, bytes: &[u8], step: &str) -> io::Result<()> {
    if let Some(root) = target(step) {
        file.write_all(&bytes[..bytes.len() / 2])?;
        pause_at_checkpoint(&root.join(step));
    }
    Ok(())
}

fn records() -> [Record; 2] {
    [(1, b"kept".as_slice()), (2, b"set aside".as_slice())].map(|(at, body)| Record {
        position: at.try_into().unwrap(),
        data: Bytes::from_static(body),
    })
}

fn encoded(records: &[Record]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for record in records {
        format::encode(record, &mut bytes);
    }
    bytes
}

fn repair(path: &Path) -> Trimmed {
    let records = records();
    let original = encoded(&records);
    let kept = encoded(&records[..1]);
    let store = MarkdownStore::new(path, true);
    let Claimed::Repair { ownership, .. } = store.claim(Claim::Repair).unwrap() else {
        panic!("expected exclusive repair claim");
    };
    store
        .trim(
            ownership.generation,
            CHANNEL,
            Trim::Repair {
                plan: [19; 32],
                input: Sha256::digest(original).into(),
                keep: &[KeptRecord {
                    position: records[0].position,
                    digest: Sha256::digest(kept).into(),
                }],
            },
        )
        .unwrap()
}

fn backup(store: &Path) -> Option<PathBuf> {
    let mut backups = fs::read_dir(store).unwrap().filter_map(|entry| {
        let path = entry.unwrap().path();
        let name = path.file_name()?.to_str()?;
        (name.starts_with(".repair-") && !name.ends_with(".next")).then_some(path)
    });
    let backup = backups.next();
    assert!(backups.next().is_none());
    backup
}

#[test]
fn a_killed_rewrite_keeps_the_whole_original_and_reuses_the_first_backup() {
    let records = records();
    let original = encoded(&records);
    let kept = encoded(&records[..1]);
    for step in [
        "backup-torn",
        "backup-written",
        "backup-synced",
        "backup-installed",
        "backup-directory-synced",
        "replacement-torn",
        "replacement-written",
        "replacement-synced",
        "replacement-installed",
        "replacement-directory-synced",
    ] {
        let root = SocketDir::new().unwrap();
        let path = root.path().join("store");
        let channel = path.join(format::file_name(CHANNEL).unwrap());
        let store = MarkdownStore::new(&path, true);
        let Claimed::Taken(owner) = store.claim(Claim::Take).unwrap() else {
            panic!("expected exclusive store claim");
        };
        store.append(owner.generation, CHANNEL, &records).unwrap();
        drop(store);
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "repair_kill::markdown_repair_child"])
            .env(ROOT, root.path())
            .env(STEP, step);

        kill_after_checkpoint(&mut child, &root.path().join(step));

        let active = fs::read(&channel).unwrap();
        assert!(active == original || active == kept, "{step}");
        let prior = backup(&path).map(|file| {
            assert_eq!(fs::read(&file).unwrap(), original, "{step}");
            fs::metadata(file).unwrap().ino()
        });
        if active == kept {
            assert!(prior.is_some(), "{step}");
        }
        if step == "backup-torn" {
            assert!(prior.is_none());
        }
        assert!(matches!(
            repair(&path),
            Trimmed::Done | Trimmed::AlreadyRewritten
        ));
        assert_eq!(fs::read(&channel).unwrap(), kept);
        let saved = backup(&path).unwrap();
        let inode = fs::metadata(&saved).unwrap().ino();
        assert!(prior.is_none_or(|prior| prior == inode), "{step}");
        fs::write(&channel, &original).unwrap();

        assert_eq!(repair(&path), Trimmed::Done);
        assert_eq!(fs::metadata(&saved).unwrap().ino(), inode, "{step}");
        assert_eq!(fs::read(saved).unwrap(), original, "{step}");
    }
}

#[test]
fn markdown_repair_child() {
    let Some(root) = std::env::var_os(ROOT) else {
        return;
    };
    let result = repair(&Path::new(&root).join("store"));
    panic!("rewrite returned {result:?} before its kill checkpoint");
}
