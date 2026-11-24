use super::recipe_data;
use crate::cli::board::{GENERAL_FOLDER, LEAD_FOLDER, LOG_FILE, OWNER_FOLDER, SOCKET_FILE};
use crate::cli::board_version::RECORD;
use crate::cli::config::{LEAD_HANDOVER_FILE, MARKDOWN_FOLDER, SQLITE_FOLDER};
use crate::cli::mailbox::LEAD_TICK;
use malak_core::OwnerCredential;
use malak_journal::StateFolder;
use malak_store_markdown::MarkdownStore;
use malak_store_sqlite::SqliteStore;
use nix::sys::stat::SFlag;
use std::ffi::OsStr;

/// The exact file names and nesting created by board start, the journal and
/// the built-in stores. Recognizing an entry also requires its expected type.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    StateTree,
    SocketTree,
    OwnerDoor,
    Door,
    Markdown,
    Sqlite,
    RecipeFolder(recipe_data::Folder),
    TrustedFile,
    PrivateFile,
    Socket,
}

impl Kind {
    pub(super) fn child(self, name: &OsStr) -> Option<Self> {
        match (self, name.to_str()?) {
            (Self::SocketTree, OWNER_FOLDER) => Some(Self::OwnerDoor),
            (Self::SocketTree, LEAD_FOLDER | GENERAL_FOLDER) => Some(Self::Door),
            (Self::Door | Self::OwnerDoor, SOCKET_FILE) => Some(Self::Socket),
            (Self::OwnerDoor, RECORD | LEAD_TICK) => Some(Self::PrivateFile),
            (Self::StateTree, MARKDOWN_FOLDER) => Some(Self::Markdown),
            (Self::StateTree, SQLITE_FOLDER) => Some(Self::Sqlite),
            (Self::StateTree, OwnerCredential::FILE_NAME | LEAD_HANDOVER_FILE | LOG_FILE) => {
                Some(Self::PrivateFile)
            }
            (Self::StateTree, file) if StateFolder::manages_file(file) => Some(Self::PrivateFile),
            (Self::StateTree, name) => recipe_data::state_child(name),
            (Self::Markdown, file) if MarkdownStore::manages_file(file) => Some(Self::PrivateFile),
            (Self::Sqlite, file) if SqliteStore::manages_file(file) => Some(Self::PrivateFile),
            (Self::RecipeFolder(folder), name) => folder.child(name),
            _ => None,
        }
    }

    pub(super) fn file_type(self) -> SFlag {
        match self {
            Self::StateTree
            | Self::SocketTree
            | Self::OwnerDoor
            | Self::Door
            | Self::Markdown
            | Self::Sqlite
            | Self::RecipeFolder(_) => SFlag::S_IFDIR,
            Self::TrustedFile | Self::PrivateFile => SFlag::S_IFREG,
            Self::Socket => SFlag::S_IFSOCK,
        }
    }

    pub(super) fn private(self) -> bool {
        if let Self::RecipeFolder(folder) = self {
            return folder.private();
        }
        matches!(
            self,
            Self::StateTree | Self::OwnerDoor | Self::Markdown | Self::Sqlite | Self::PrivateFile
        )
    }
}
