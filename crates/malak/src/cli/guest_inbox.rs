//! The lead's local references to guest inboxes on other boards.

use super::{
    board_names,
    config::{self, Config, io_error},
    mailbox::REFRESH,
    participant::{GuestRecord, Session, preview_handover},
};
use malak::{ChannelName, CreationId, Error};
use malak_core::HandoverFile;
use malak_fs::Directory;
use nix::fcntl::OFlag;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

const FOLDER: &str = "guest-inboxes";
const REGISTRY: &str = "inboxes.json";
const STAGED: &str = "inboxes.tmp";

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub(super) struct GuestShown(BTreeMap<String, (String, u64)>);

impl GuestShown {
    pub fn is_new(&self, guest: &str, creation: &str, position: u64) -> bool {
        self.0
            .get(guest)
            .is_none_or(|(key, through)| key != creation || position > *through)
    }

    pub fn advance(&mut self, guest: &str, creation: &str, position: u64) {
        self.0
            .insert(guest.to_owned(), (creation.to_owned(), position));
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct GuestInbox {
    pub board: String,
    handover: PathBuf,
    pin: [u8; 32],
    inbox: String,
}

impl GuestInbox {
    pub fn key(&self) -> String {
        let pin = self.pin.map(|byte| format!("{byte:02x}")).concat();
        format!("{pin}/{}", self.inbox)
    }

    fn attach(&self) -> Result<Session, Error> {
        let handover = HandoverFile::read(&self.handover)?;
        if handover.board_key() != &self.pin || handover.id().inbox().as_str() != self.inbox {
            return Err(Error::refused("registered guest handover changed identity"));
        }
        Session::open_guest(handover, REFRESH)
    }

    pub fn read(&self, limit: Option<u64>) -> Vec<GuestRecord> {
        let records = self
            .attach()
            .and_then(|mut session| session.read_guest(&self.inbox, limit, REFRESH));
        // A stopped or revoked guest board cannot interrupt the home board.
        let mut records = records.unwrap_or_default();
        for record in &mut records {
            record
                .label
                .insert("guest_board".into(), Value::String(self.board.clone()));
        }
        records
    }

    pub fn for_creation(&self, creation: CreationId) -> Option<(Session, ChannelName)> {
        let channel: ChannelName = self.inbox.parse().ok()?;
        let mut session = self.attach().ok()?;
        (session.creation_within(channel.clone(), REFRESH).ok()? == creation)
            .then_some((session, channel))
    }

    pub fn acknowledge(&self, creation: &str, position: u64, feedback: bool) -> Result<(), Error> {
        self.attach()?.ack_printed(
            vec![(self.inbox.clone(), creation.to_owned(), position)],
            feedback,
        )
    }
}

pub(super) fn register(config: Option<PathBuf>, board: &str, path: &Path) -> Result<(), Error> {
    let home = Config::read(config)?.board;
    let path = std::path::absolute(path).map_err(io_error)?;
    let handover = HandoverFile::read(&path)?;
    if handover.id().is_lead() || home.check_handover(&path, &handover).is_ok() {
        return Err(Error::refused("use a guest handover for another board"));
    }
    let entry = GuestInbox {
        board: board.to_owned(),
        handover: path,
        pin: *handover.board_key(),
        inbox: handover.id().inbox().to_string(),
    };
    let root = Directory::open(&home.state).map_err(io_error)?;
    let (folder, _) = root.child_or_create(OsStr::new(FOLDER)).map_err(io_error)?;
    save(&folder, entry).map_err(io_error)
}

fn save(folder: &Directory, entry: GuestInbox) -> io::Result<()> {
    folder.file().lock()?;
    let mut entries = load(folder)?;
    entries.retain(|old| old.pin != entry.pin || old.inbox != entry.inbox);
    entries.push(entry);
    let staged = folder.open_file(STAGED, OFlag::O_WRONLY | OFlag::O_CREAT)?;
    staged.set_len(0)?;
    serde_json::to_writer(&staged, &entries)?;
    staged.sync_all()?;
    folder.rename(STAGED, REGISTRY)?;
    folder.sync()
}

fn load(folder: &Directory) -> io::Result<Vec<GuestInbox>> {
    match folder.open_file(REGISTRY, OFlag::O_RDONLY) {
        Ok(file) => serde_json::from_reader(file).map_err(Into::into),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

pub(super) fn for_lead(config: &Option<PathBuf>) -> Vec<GuestInbox> {
    if let Ok(board) = Config::read(config.clone())
        && let Ok(lead) = preview_handover(config.clone())
        && lead.id().is_lead()
    {
        return for_board(&board.board);
    }
    // A handover alone can select a named home board when the caller did not
    // explicitly select a config. Resolve it by socket and pinned board key.
    if config.is_none()
        && let Some(path) = std::env::var_os("MALAK_HANDOVER").map(PathBuf::from)
        && let Ok(lead) = HandoverFile::read(&path)
        && lead.id().is_lead()
        && let Ok(boards) = board_names::list_quiet()
    {
        for board in boards.into_iter().filter_map(|entry| entry.board.ok()) {
            if board.check_handover(&path, &lead).is_ok() {
                return for_board(&board);
            }
        }
    }
    Vec::new()
}

pub(super) fn for_board(board: &config::Board) -> Vec<GuestInbox> {
    folder_for(board)
        .and_then(|folder| load(&folder))
        .unwrap_or_default()
}

fn folder_for(board: &config::Board) -> io::Result<Directory> {
    Directory::open(&board.state)?.child(OsStr::new(FOLDER))
}
