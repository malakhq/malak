use super::super::{board_names, config, output};
use super::{layout::Kind, recipe_data};
use malak::Error;
use malak_core::HandoverFile;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn show(
    local: &board_names::LocalBoard,
    verbose: bool,
    status: Option<&str>,
    detail: Option<(&str, &str)>,
) -> Result<(), Error> {
    let mut fields = Vec::new();
    let mut states = BTreeSet::new();
    let mut sockets = BTreeSet::new();
    let mut contents = Contents::default();
    for entry in local.configs() {
        fields.push(("Config", entry.path.to_string_lossy()));
        let Ok(board) = &entry.board else { continue };
        if states.insert(&board.state) {
            fields.push(("State", board.state.to_string_lossy()));
            contents.scan(&board.state, board, verbose);
            if verbose {
                fields.push((
                    "Lead handover",
                    board.lead_handover().to_string_lossy().into_owned().into(),
                ));
            }
        }
        if sockets.insert(&board.socket_folder) {
            fields.push(("Sockets", board.socket_folder.to_string_lossy()));
            contents.scan(&board.socket_folder, board, verbose);
        }
    }
    let counts = contents.counts();
    if !counts.is_empty() {
        fields.push(("Contains", Cow::Borrowed(&counts)));
    }
    fields.extend(status.map(|text| ("Status", Cow::Borrowed(text))));
    fields.extend(detail.map(|(label, text)| (label, Cow::Borrowed(text))));
    let fields: Vec<_> = fields
        .iter()
        .map(|(label, value)| (*label, value.as_ref()))
        .collect();
    output::details(
        &format!("Board data for {}", board_names::name(local.primary())),
        &fields,
    )?;
    for path in contents.paths {
        output::details("Handover", &[("Path", &path.to_string_lossy())])?;
    }
    Ok(())
}

pub(super) fn external_handover(boards: &[board_names::LocalBoard]) -> Option<PathBuf> {
    let path = std::path::absolute(std::env::var_os("MALAK_HANDOVER")?).ok()?;
    let handover = HandoverFile::read(&path).ok()?;
    let mut matched = false;
    for entry in boards.iter().flat_map(board_names::LocalBoard::configs) {
        let Ok(board) = &entry.board else { continue };
        if path.starts_with(&board.state) || path.starts_with(&board.socket_folder) {
            return None;
        }
        matched |= board.check_handover(&path, &handover).is_ok();
    }
    matched.then_some(path)
}

#[derive(Default)]
struct Contents {
    handovers: usize,
    demos: usize,
    paths: Vec<PathBuf>,
}

impl Contents {
    fn scan(&mut self, folder: &Path, board: &config::Board, verbose: bool) {
        if config::open_owned_directory(folder, false)
            .ok()
            .flatten()
            .is_some()
        {
            self.walk(folder, board, verbose);
        }
    }

    fn walk(&mut self, folder: &Path, board: &config::Board, verbose: bool) {
        let Ok(entries) = fs::read_dir(folder) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                if folder == board.state
                    && entry.file_name().to_str().is_some_and(|name| {
                        matches!(
                            recipe_data::state_child(name),
                            Some(Kind::RecipeFolder(recipe_data::Folder::Demo))
                        )
                    })
                {
                    self.demos += 1;
                }
                self.walk(&path, board, verbose);
            } else if kind.is_file()
                && path != board.lead_handover()
                && let Ok(handover) = HandoverFile::read(&path)
                && board.check_handover(&path, &handover).is_ok()
            {
                self.handovers += usize::from(!handover.id().is_lead());
                if verbose {
                    self.paths.push(path);
                }
            }
        }
    }

    fn counts(&self) -> String {
        [
            (self.handovers, "worker handover"),
            (self.demos, "demo workspace"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, noun)| format!("{count} {noun}{}", if count == 1 { "" } else { "s" }))
        .collect::<Vec<_>>()
        .join(", ")
    }
}
