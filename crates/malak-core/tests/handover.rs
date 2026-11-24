//! Tests of local handover custody and file contents.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use malak_core::{HandoverFile, HandoverTarget, HandoverWrite, Outcome, ParticipantId};
use malak_proto::{Binding, Entrance};
use malak_testkit::unix_socket::SocketDir;
use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::PathBuf;
use zeroize::Zeroizing;

struct Fixture {
    root: SocketDir,
    directory: PathBuf,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = SocketDir::new().unwrap();
        let directory = root.path().join("private");
        DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let path = directory.join("worker");
        Self {
            root,
            directory,
            path,
        }
    }

    fn prepare(&self, mode: HandoverWrite) -> HandoverTarget {
        HandoverTarget::prepare(&self.path, uid(), mode).unwrap()
    }
}

fn uid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn handover(key: u8) -> HandoverFile {
    HandoverFile::new(
        Binding {
            version: 1,
            entrance: Entrance::General,
        },
        "unix:/local/socket".into(),
        [7; 32],
        "member01".parse::<ParticipantId>().unwrap(),
        "key".into(),
        Zeroizing::new(b"proof context".to_vec()),
        Zeroizing::new([key; 32]),
    )
    .unwrap()
}

#[test]
fn a_private_local_handover_round_trips() {
    let fixture = Fixture::new();
    fixture
        .prepare(HandoverWrite::New)
        .write(&handover(21))
        .unwrap();

    let read = HandoverFile::read(&fixture.path).unwrap();

    assert_eq!(read.binding().entrance, Entrance::General);
    assert_eq!(read.binding().version, 1);
    assert_eq!(read.address(), "unix:/local/socket");
    assert_eq!(read.id().as_str(), "member01");
    assert_eq!(read.mechanism(), "key");
    assert_eq!(read.mechanism_data(), b"proof context");
    assert_eq!(read.board_key(), &[7; 32]);
    assert_eq!(read.private_key(), &[21; 32]);
    assert_eq!(
        fixture.path.metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn enrolment_does_not_overwrite_an_existing_handover() {
    let fixture = Fixture::new();
    let target = fixture.prepare(HandoverWrite::New);
    fixture
        .prepare(HandoverWrite::New)
        .write(&handover(1))
        .unwrap();

    let error = target.write(&handover(2)).unwrap_err();

    assert_eq!(error.outcome(), Outcome::Refused);
    assert_eq!(
        HandoverFile::read(&fixture.path).unwrap().private_key(),
        &[1; 32]
    );
}

#[test]
fn replacing_a_handover_is_atomic_for_existing_readers() {
    let fixture = Fixture::new();
    fixture
        .prepare(HandoverWrite::New)
        .write(&handover(1))
        .unwrap();
    let mut prior = fs::File::open(&fixture.path).unwrap();
    let before = fs::read(&fixture.path).unwrap();

    fixture
        .prepare(HandoverWrite::Replace)
        .write(&handover(2))
        .unwrap();

    assert_eq!(
        HandoverFile::read(&fixture.path).unwrap().private_key(),
        &[2; 32]
    );
    let mut still = Vec::new();
    std::io::Read::read_to_end(&mut prior, &mut still).unwrap();
    assert_eq!(still, before);
}

#[test]
fn a_folder_swap_keeps_the_original_handover_destination() {
    let fixture = Fixture::new();
    let target = fixture.prepare(HandoverWrite::New);
    let original = fixture.root.path().join("moved");
    fs::rename(&fixture.directory, &original).unwrap();
    fs::create_dir(&fixture.directory).unwrap();

    target.write(&handover(7)).unwrap();

    assert!(!fixture.path.exists());
    assert_eq!(
        HandoverFile::read(original.join("worker"))
            .unwrap()
            .private_key(),
        &[7; 32]
    );
}

#[test]
fn a_world_writable_ancestor_disqualifies_the_handover_path() {
    let fixture = Fixture::new();
    let shared = fixture.root.path().join("shared");
    let private = shared.join("private");
    fs::create_dir_all(&private).unwrap();
    let path = private.join("worker");
    HandoverTarget::prepare(&path, uid(), HandoverWrite::New).unwrap();
    fs::set_permissions(&shared, Permissions::from_mode(0o777)).unwrap();

    let result = HandoverTarget::prepare(&path, uid(), HandoverWrite::New);

    assert_eq!(result.err().unwrap().outcome(), Outcome::Refused);
    assert!(!path.exists());
}

#[test]
fn a_symbolic_link_cannot_redirect_a_handover() {
    let fixture = Fixture::new();
    let link = fixture.root.path().join("other");
    symlink(&fixture.directory, &link).unwrap();

    assert!(HandoverTarget::prepare(link.join("worker"), uid(), HandoverWrite::New).is_err());
}

#[test]
fn an_existing_symbolic_link_cannot_be_used_for_a_rekey() {
    let fixture = Fixture::new();
    symlink(fixture.root.path().join("external"), &fixture.path).unwrap();

    assert!(HandoverTarget::prepare(&fixture.path, uid(), HandoverWrite::Replace).is_err());
}

#[test]
fn a_shared_handover_is_refused_before_reading_private_bytes() {
    let fixture = Fixture::new();
    fixture
        .prepare(HandoverWrite::New)
        .write(&handover(1))
        .unwrap();
    fs::set_permissions(&fixture.path, Permissions::from_mode(0o644)).unwrap();

    assert!(HandoverFile::read(&fixture.path).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn a_writable_ancestor_acl_disqualifies_the_handover_path() {
    let fixture = Fixture::new();
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow add_file"])
        .arg(&fixture.directory)
        .status()
        .unwrap();
    assert!(status.success());

    assert!(HandoverTarget::prepare(&fixture.path, uid(), HandoverWrite::New).is_err());
}

#[test]
fn a_named_credential_acl_grants_no_third_account_access() {
    use malak_fs::{AccountAccess, check_credential, share_with};
    use nix::unistd::User;

    let fixture = Fixture::new();
    fixture
        .prepare(HandoverWrite::New)
        .write(&handover(11))
        .unwrap();
    let file = fs::File::open(&fixture.path).unwrap();
    let reader = User::from_name("nobody").unwrap().unwrap().uid.as_raw();

    share_with(&file, reader, AccountAccess::Read).unwrap();

    assert!(check_credential(&file, uid(), reader).is_ok());
    assert!(check_credential(&file, uid(), reader ^ 1).is_err());
    assert!(HandoverFile::read(&fixture.path).is_ok());
}

#[test]
fn a_named_directory_acl_allows_search_only_for_its_recipient() {
    use malak_fs::{AccountAccess, Directory, can_traverse, share_with};
    use nix::unistd::User;

    let fixture = Fixture::new();
    let folder = Directory::open(&fixture.directory).unwrap();
    let reader = User::from_name("nobody").unwrap().unwrap().uid.as_raw();

    share_with(folder.file(), reader, AccountAccess::Search).unwrap();

    assert!(can_traverse(&folder, reader).is_ok());
    assert!(can_traverse(&folder, uid()).is_ok());
    assert!(can_traverse(&folder, reader ^ 1).is_err());
}
