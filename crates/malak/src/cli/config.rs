use malak::{Error, Limits, Outcome};
use malak_core::HandoverFile;
use malak_fs::{Directory, check_user_permissions, trusted_parent};
use malak_layers::Store;
use malak_store_markdown::MarkdownStore;
use malak_store_sqlite::SqliteStore;
use nix::fcntl::OFlag;
use nix::unistd::geteuid;
use serde::Deserialize;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub board: Board,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Board {
    pub state: PathBuf,
    pub socket_folder: PathBuf,
    pub name: Option<String>,
    #[serde(rename = "parent")]
    _parent: Option<String>,
    pub transport: Option<String>,
    pub persistence: Option<String>,
    pub mechanism: Option<String>,
    pub lead_account: Option<String>,
    pub store_place_socket: Option<PathBuf>,
    #[serde(default)]
    inbox_limits: InboxLimits,
    #[serde(default)]
    pub sync: bool,
    #[serde(default)]
    pub calls: super::org::calls::settings::Options,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct InboxLimits {
    enabled: bool,
    max_messages: usize,
    max_per_sender: Option<usize>,
    inbox: Vec<InboxOverride>,
}

impl Default for InboxLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            max_messages: 1000,
            max_per_sender: None,
            inbox: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxOverride {
    pattern: String,
    max_messages: Option<usize>,
    max_per_sender: Option<usize>,
}

/// What a command that cannot find a board tells the person to do next.
pub(super) const START_HINT: &str = "start a board with: malak board start";
pub(super) const LEAD_HANDOVER_FILE: &str = "lead.handover";
pub(super) const MARKDOWN_FOLDER: &str = "messages";
pub(super) const SQLITE_FOLDER: &str = "sqlite";

impl Config {
    pub(super) fn read(path: Option<PathBuf>) -> Result<Self, Error> {
        let (path, next) = match path {
            Some(path) => (
                std::path::absolute(path).map_err(io_error)?,
                "check the --config path",
            ),
            None => (default_path()?, START_HINT),
        };
        if let Err(error) = path.symlink_metadata()
            && error.kind() == io::ErrorKind::NotFound
        {
            return Err(Error::refused(format!(
                "no board configuration at {}; {next}",
                path.display()
            )));
        }
        let uid = geteuid().as_raw();
        let (parent, name) =
            trusted_parent(&path, uid).map_err(|error| config_error(&path, io_error(error)))?;
        let name = name
            .to_str()
            .ok_or_else(|| config_error(&path, Error::refused("invalid config filename")))?;
        let file = parent
            .open_file(name, OFlag::O_RDONLY)
            .map_err(|error| config_error(&path, io_error(error)))?;
        check_user_permissions(&file, uid, false)
            .map_err(|error| config_error(&path, io_error(error)))?;
        let mut source = String::new();
        file.take(64 * 1024 + 1)
            .read_to_string(&mut source)
            .map_err(|error| config_error(&path, io_error(error)))?;
        if source.len() > 64 * 1024 {
            return Err(config_error(
                &path,
                Error::refused("configuration exceeds 64 KiB"),
            ));
        }
        let result: Self = toml::from_str(&source)
            .map_err(|_| config_error(&path, Error::refused("invalid board configuration")))?;
        result
            .board
            .validate()
            .map_err(|error| config_error(&path, error))?;
        Ok(result)
    }
}

fn config_error(path: &Path, error: Error) -> Error {
    let reason = format!("Configuration {}: {error}", path.display());
    match error.outcome() {
        Outcome::Refused => Error::refused(reason),
        Outcome::NotSupported => Error::not_supported(reason),
        Outcome::Done | Outcome::Unavailable => Error::unavailable(reason),
    }
}

impl Board {
    pub(super) fn check_handover(&self, path: &Path, handover: &HandoverFile) -> Result<(), Error> {
        let place = handover.address().strip_prefix("unix:");
        let socket_folder = place.and_then(|address| Path::new(address).parent()?.parent());
        let handover_board = place.unwrap_or(handover.address());
        let selected_board = self.socket_folder.display();
        let instruction = "Set MALAK_HANDOVER to a handover for the selected board.";
        if socket_folder == Some(self.socket_folder.as_path()) {
            let credential = super::owner_credential(self).map_err(|_| {
                Error::refused(format!(
                    "could not verify handover {} for the selected board at {selected_board}. Check the selected board’s owner credential or use MALAK_HANDOVER by itself",
                    path.display()
                ))
            })?;
            if credential.board_key() == handover.board_key() {
                return Ok(());
            }
        }
        Err(Error::refused(format!(
            "handover {} for the board at {handover_board} does not match the selected board at {selected_board}. {instruction}",
            path.display()
        )))
    }

    pub(super) fn lead_handover(&self) -> PathBuf {
        self.state.join(LEAD_HANDOVER_FILE)
    }

    pub(super) fn limits(&self) -> Result<Limits, Error> {
        let mut limits = Limits::default();
        let inbox = &self.inbox_limits;
        if inbox.enabled {
            limits.set_default_inbox_limit(inbox.max_messages, inbox.max_per_sender)?;
            for entry in &inbox.inbox {
                limits.limit_inbox(
                    entry.pattern.parse()?,
                    entry.max_messages.unwrap_or(inbox.max_messages),
                    entry.max_per_sender.or(inbox.max_per_sender),
                )?;
            }
        }
        limits.validate()?;
        Ok(limits)
    }

    pub(super) fn store(&self) -> Box<dyn Store> {
        if self.persistence.as_deref() == Some("sqlite") {
            return Box::new(SqliteStore::new(self.state.join(SQLITE_FOLDER)));
        }
        let store = MarkdownStore::new(self.state.join(MARKDOWN_FOLDER), self.sync);
        match &self.store_place_socket {
            Some(socket) => Box::new(store.with_place_socket(socket)),
            None => Box::new(store),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        self.calls.validate()?;
        for (actual, supported) in [(&self.transport, "unix-socket"), (&self.mechanism, "key")] {
            if actual.as_deref().is_some_and(|value| value != supported) {
                return Err(Error::not_supported("configured layer is unavailable"));
            }
        }
        if !matches!(
            self.persistence.as_deref(),
            None | Some("markdown" | "sqlite")
        ) {
            return Err(Error::not_supported("configured store is unavailable"));
        }
        if self.persistence.as_deref() == Some("sqlite") && self.store_place_socket.is_some() {
            return Err(Error::not_supported("SQLite repair is not supported"));
        }
        if let Some(name) = &self.name {
            let _: malak::ChannelName = name.parse()?;
        }
        if !self.state.is_absolute() || !self.socket_folder.is_absolute() {
            return Err(Error::refused("board directories must be absolute"));
        }
        if self
            .store_place_socket
            .as_ref()
            .is_some_and(|socket| !socket.is_absolute() || socket.starts_with(&self.state))
        {
            return Err(Error::refused(
                "store place socket must be absolute and outside the state folder",
            ));
        }
        if self.socket_folder.starts_with(&self.state)
            || self.state.starts_with(&self.socket_folder)
        {
            return Err(Error::refused(
                "state and sockets require separate directories",
            ));
        }
        Ok(())
    }
}

/// Read the board configuration for `board start`. With no `--config` and no
/// default board, first create the default board and its configuration.
/// Return the configuration, its absolute path and any creation notice.
pub(super) fn read_for_start(
    path: Option<PathBuf>,
) -> Result<(Config, PathBuf, Option<String>), Error> {
    let (path, created) = match path {
        Some(path) => (std::path::absolute(path).map_err(io_error)?, None),
        None => (default_path()?, create_default_board()?),
    };
    let config = Config::read(Some(path.clone()))?;
    Ok((config, path, created))
}

/// Create the managed config for a new named lead board.
pub(super) fn create_named_board(folder: &Path) -> Result<(), Error> {
    ensure_folder(folder)?;
    let directory = private_directory(folder)?;
    directory.file().lock().map_err(io_error)?;
    let destination = "board.toml";
    if folder.join(destination).symlink_metadata().is_ok() {
        return Ok(());
    }
    let (file, path) = nix::unistd::mkstemp(&folder.join(".board.XXXXXXXX"))
        .map_err(|error| io_error(error.into()))?;
    let temporary = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("fixed ASCII configuration filename");
    let written = (|| {
        let mut file = std::fs::File::from(file);
        file.write_all(board_paths(folder).as_bytes())?;
        file.sync_all()?;
        directory.rename_to_new(temporary, destination)
    })();
    if written.is_err() {
        let _ = directory.remove(temporary);
    }
    match written {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        result => result.map_err(io_error),
    }
}

fn board_paths(folder: &Path) -> String {
    let state = folder.join("state");
    let sockets = folder.join("sockets");
    format!("[board]\nstate = {state:?}\nsocket_folder = {sockets:?}\n")
}

/// Create `~/.malak/default/` and the configuration that points at it, unless
/// the default configuration already exists.
fn create_default_board() -> Result<Option<String>, Error> {
    let config = default_path()?;
    if config.symlink_metadata().is_ok() {
        return Ok(None);
    }
    let home = std::env::home_dir()
        .filter(|home| home.is_absolute())
        .ok_or_else(|| Error::refused("could not find the home folder for the default board"))?;
    let board = home.join(".malak/default");
    ensure_folder(&board)?;
    let folder = config
        .parent()
        .ok_or_else(|| Error::refused("could not find the per-user configuration folder"))?;
    ensure_folder(folder)?;
    let text = board_paths(&board);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&config)
        .and_then(|mut file| file.write_all(text.as_bytes()));
    match written {
        Ok(()) => Ok(Some(format!(
            "created the default board in {} and its configuration {}",
            board.display(),
            config.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}

/// Create a private folder and any missing folders above it.
fn ensure_folder(path: &Path) -> Result<(), Error> {
    if let Some(parent) = path.parent().filter(|parent| !parent.exists()) {
        ensure_folder(parent)?;
    }
    private_directory(path).map(drop)
}

pub(super) fn default_path() -> Result<PathBuf, Error> {
    let user_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::home_dir().map(|home| home.join(".config")))
        .ok_or_else(|| Error::refused("could not find the per-user configuration folder"))?;
    Ok(user_config.join("malak/board.toml"))
}

pub(super) fn owned_directory(path: &Path) -> Result<Directory, Error> {
    directory(path, false)
}

pub(super) fn private_directory(path: &Path) -> Result<Directory, Error> {
    directory(path, true)
}

fn directory(path: &Path, private: bool) -> Result<Directory, Error> {
    let uid = geteuid().as_raw();
    let (parent, name) = trusted_parent(path, uid).map_err(io_error)?;
    let (child, _) = parent.child_or_create(name).map_err(io_error)?;
    check_owned_directory(child, uid, private).map_err(io_error)
}

/// Open and check a board folder without creating it.
pub(super) fn open_owned_directory(path: &Path, private: bool) -> io::Result<Option<Directory>> {
    let uid = geteuid().as_raw();
    let open = || {
        let (parent, name) = trusted_parent(path, uid)?;
        check_owned_directory(parent.child(name)?, uid, private)
    };
    match open() {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn check_owned_directory(child: Directory, uid: u32, private: bool) -> io::Result<Directory> {
    if child.file().metadata()?.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket folder belongs to another account",
        ));
    }
    check_user_permissions(child.file(), uid, private)?;
    Ok(child)
}

pub(super) fn io_error(error: io::Error) -> Error {
    if error.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32) {
        return Error::refused(error.to_string());
    }
    match error.kind() {
        io::ErrorKind::PermissionDenied
        | io::ErrorKind::InvalidInput
        | io::ErrorKind::InvalidData
        | io::ErrorKind::NotADirectory
        | io::ErrorKind::NotFound => Error::refused(error.to_string()),
        _ => Error::unavailable(error.to_string()),
    }
}
