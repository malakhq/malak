//! Throw away changes beyond the SQLite VFS's last successful sync, then reopen.

use super::*;
use malak_testkit::process::{kill_after_checkpoint, pause_at_checkpoint};
use malak_testkit::unix_socket::SocketDir;
use rusqlite::ffi::*;
use std::cell::Cell;
use std::ffi::{c_int, c_void};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;

const CHANNEL: &str = "lead/_00000000000000000000000000000001";
const DATABASE: &str = "records.sqlite3";
const WAL: &str = "records.sqlite3-wal";

#[test]
fn a_power_cut_before_wal_sync_keeps_only_the_synced_prefix() {
    assert_image("during-write", 1);
    assert_image("before-sync", 1);
}

#[test]
fn a_power_cut_after_wal_sync_keeps_the_whole_batch() {
    assert_image("after-sync", 3);
}

#[test]
fn a_batch_reported_synced_survives_a_power_cut() {
    assert_image("after-receipt", 3);
}

#[test]
fn a_failed_wal_sync_does_not_report_durability() {
    assert_image("sync-failed", 1);
}

fn assert_image(phase: &str, count: u64) {
    let scratch = SocketDir::new().unwrap();
    let root = scratch.path();
    kill_after_checkpoint(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "power_cut::power_cut_child",
                "--ignored",
                "--nocapture",
            ])
            .env("MALAK_SQLITE_CUT", phase)
            .env("MALAK_SQLITE_CUT_ROOT", root),
        &root.join("checkpoint"),
    );

    let store = SqliteStore::new(root.join("stable"));
    store.claim(Claim::Take).unwrap();
    let result = store
        .read(
            CHANNEL,
            Position::FIRST,
            ReadLimit {
                records: 10,
                before: None,
            },
        )
        .unwrap();
    let ReadResult::Records {
        records,
        unparseable: false,
    } = result
    else {
        panic!("SQLite could not reopen the stable image");
    };
    assert_eq!(records, (1..=count).map(record).collect::<Vec<_>>());
}

#[test]
#[ignore = "runs in the power cut subprocess"]
fn power_cut_child() {
    let phase = std::env::var("MALAK_SQLITE_CUT").unwrap();
    let root = PathBuf::from(std::env::var_os("MALAK_SQLITE_CUT_ROOT").unwrap());
    let stable = root.join("stable");
    fs::DirBuilder::new().mode(0o700).create(&stable).unwrap();
    let store = SqliteStore::new(root.join("live"));
    let Claimed::Taken(owner) = store.claim(Claim::Take).unwrap() else {
        unreachable!();
    };
    store
        .append(owner.generation, CHANNEL, &[record(1)])
        .unwrap();

    // The WAL is already open and this batch is too small to trigger an
    // auto-checkpoint. SQLite keeps both file handles live for the test.
    let (db, wal) = {
        let mut active = store.lock().unwrap();
        let connection = &database::active(&mut active).unwrap().db;
        (
            file_pointer(connection, SQLITE_FCNTL_FILE_POINTER),
            file_pointer(connection, SQLITE_FCNTL_JOURNAL_POINTER),
        )
    };
    // SAFETY: The child owns this SQLite connection and uses it on this
    // thread only. The callbacks and original files outlive the append.
    unsafe {
        save_image(db, &stable.join(DATABASE));
        save_image(wal, &stable.join(WAL));
        let original = (*wal).pMethods;
        let hook = WalHook {
            methods: sqlite3_io_methods {
                xWrite: Some(wal_write),
                xSync: Some(wal_sync),
                ..*original
            },
            original,
            phase: &phase,
            checkpoint: root.join("checkpoint"),
            stable_wal: stable.join(WAL),
            writes: Cell::new(0),
        };
        (*wal).pMethods = &hook.methods;
        let appended = store.append(owner.generation, CHANNEL, &[record(2), record(3)]);
        (*wal).pMethods = original;

        match phase.as_str() {
            "after-receipt" => appended.unwrap(),
            "sync-failed" => assert!(appended.is_err()),
            _ => panic!("SQLite did not reach the requested VFS checkpoint"),
        }
        pause_at_checkpoint(&hook.checkpoint);
    }
}

// The first field is the method table SQLite holds a pointer to.
#[repr(C)]
struct WalHook<'a> {
    methods: sqlite3_io_methods,
    original: *const sqlite3_io_methods,
    phase: &'a str,
    checkpoint: PathBuf,
    stable_wal: PathBuf,
    writes: Cell<usize>,
}

unsafe extern "C" fn wal_write(
    file: *mut sqlite3_file,
    buffer: *const c_void,
    count: c_int,
    offset: sqlite3_int64,
) -> c_int {
    // SAFETY: SQLite calls through our installed methods on this live WAL.
    unsafe {
        let hook = &*(*file).pMethods.cast::<WalHook<'_>>();
        let result = (*hook.original).xWrite.unwrap()(file, buffer, count, offset);
        hook.writes.set(hook.writes.get() + 1);
        if result == SQLITE_OK && hook.phase == "during-write" && hook.writes.get() == 1 {
            pause_at_checkpoint(&hook.checkpoint);
        }
        result
    }
}

unsafe extern "C" fn wal_sync(file: *mut sqlite3_file, flags: c_int) -> c_int {
    // SAFETY: SQLite calls through our installed methods on this live WAL.
    unsafe {
        let hook = &*(*file).pMethods.cast::<WalHook<'_>>();
        if flags & 0xf != SQLITE_SYNC_FULL {
            return SQLITE_IOERR_FSYNC;
        }
        match hook.phase {
            "sync-failed" => return SQLITE_IOERR_FSYNC,
            "before-sync" => pause_at_checkpoint(&hook.checkpoint),
            _ => {}
        }
        let result = (*hook.original).xSync.unwrap()(file, flags);
        if result == SQLITE_OK {
            save_image(file, &hook.stable_wal);
            if hook.phase == "after-sync" {
                pause_at_checkpoint(&hook.checkpoint);
            }
        }
        result
    }
}

fn file_pointer(connection: &rusqlite::Connection, operation: c_int) -> *mut sqlite3_file {
    let mut file: *mut sqlite3_file = std::ptr::null_mut();
    // SAFETY: These controls write one file pointer through the supplied address.
    let status = unsafe {
        sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            operation,
            (&mut file as *mut *mut sqlite3_file).cast(),
        )
    };
    assert_eq!(status, SQLITE_OK);
    assert!(!file.is_null());
    file
}

unsafe fn save_image(file: *mut sqlite3_file, path: &Path) {
    // SAFETY: The caller has the live VFS file on the connection's only thread.
    // Reading through xRead avoids opening another descriptor for the live DB.
    unsafe {
        let mut size = 0;
        let methods = &*(*file).pMethods;
        assert_eq!(methods.xFileSize.unwrap()(file, &mut size), SQLITE_OK);
        let length = c_int::try_from(size).unwrap();
        let mut bytes = vec![0u8; length.try_into().unwrap()];
        assert_eq!(
            methods.xRead.unwrap()(file, bytes.as_mut_ptr().cast(), length, 0),
            SQLITE_OK,
        );
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }
}

fn record(position: u64) -> Record {
    Record {
        position: position.try_into().unwrap(),
        data: position.to_be_bytes().to_vec().into(),
    }
}
