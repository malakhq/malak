//! Discover local boards and keep each board's live state independent.

use super::{
    LiveBoard, REFRESH,
    interactive::BoardUpdates,
    model::{Address, Model},
    summary,
    trace::LiveTrace,
    view::Startup,
};
use crate::cli::{board_list, board_names, config};
use malak::Error;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
};

mod connection;
use connection::{Observation, Watch};

pub(super) type BoardId = Arc<Path>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Issue {
    Connecting,
    Stopped,
    Unreadable(String),
    Older,
}

impl Issue {
    pub fn label(&self) -> &str {
        match self {
            Self::Connecting => "connecting…",
            Self::Stopped => "stopped",
            Self::Unreadable(reason) => reason,
            Self::Older => "older build, restart",
        }
    }

    pub fn mark(&self) -> &str {
        match self {
            Self::Connecting => "…",
            Self::Stopped => "○",
            Self::Unreadable(_) => "✕",
            Self::Older => "● ⚠",
        }
    }
}

pub(super) struct BoardEntry {
    pub id: BoardId,
    pub name: String,
    pub state: State,
    watch: Option<Watch>,
}

pub(super) enum State {
    Issue(Issue),
    Running(Box<Running>),
}

pub(super) struct Running {
    pub board: LiveBoard,
    pub model: Model,
    pub startup: Startup,
    pub older: bool,
    updates: BoardUpdates,
    stopped_at: Option<u64>,
}

impl From<connection::Connected> for Running {
    fn from(connected: connection::Connected) -> Self {
        let mut model = Model::new(connected.initial, None);
        model.enable_activity();
        model.previews_enabled = true;
        Self {
            board: connected.board,
            model,
            startup: Startup::default(),
            older: connected.older,
            updates: BoardUpdates::new(connected.updates, Some(connected.previews)),
            stopped_at: None,
        }
    }
}

impl BoardEntry {
    fn new(path: PathBuf, name: String) -> Self {
        let id = BoardId::from(path);
        let (watch, issue) = match Watch::start(id.clone()) {
            Ok(watch) => (Some(watch), Issue::Connecting),
            Err(error) => (None, Issue::Unreadable(error.to_string())),
        };
        Self {
            id,
            name,
            state: State::Issue(issue),
            watch,
        }
    }

    #[cfg(test)]
    pub fn ready_fixture(name: &str, model: Model) -> Self {
        let path = PathBuf::from(format!("/{name}/board.toml"));
        let (snapshots, _) = mpsc::channel();
        let (_, events) = mpsc::sync_channel(1);
        Self {
            id: BoardId::from(path.clone()),
            name: name.into(),
            watch: None,
            state: State::Running(Box::new(Running {
                board: LiveBoard {
                    previews: super::PreviewDoor::Discovered(path),
                    snapshots,
                },
                model,
                startup: Startup {
                    history_ready: true,
                    history_complete: true,
                },
                older: false,
                updates: BoardUpdates::new(events, None),
                stopped_at: None,
            })),
        }
    }

    pub fn running(&self) -> Option<&Running> {
        match &self.state {
            State::Running(running) => Some(running),
            State::Issue(_) => None,
        }
    }

    fn issue(&mut self, issue: Issue) -> bool {
        if matches!(&self.state, State::Issue(current) if *current == issue) {
            return false;
        }
        self.state = State::Issue(issue);
        true
    }

    fn drain(&mut self, trace: Option<&mut LiveTrace>, preview: Option<&Address>) -> (bool, bool) {
        let mut redraw = false;
        if let Some(observation) = self
            .watch
            .as_ref()
            .and_then(|watch| watch.observations.try_recv().ok())
        {
            match observation {
                Observation::Issue(issue) => redraw = self.issue(issue),
                Observation::Running(connected) => {
                    self.state = State::Running(Box::new(Running::from(*connected)));
                    redraw = true;
                }
            }
        }
        let State::Running(running) = &mut self.state else {
            return (redraw, false);
        };
        let batch = running.updates.drain(
            &mut running.model,
            &mut running.startup,
            &mut running.stopped_at,
            trace,
            false,
        );
        if running.stopped_at.is_some() {
            return (self.issue(Issue::Stopped), false);
        }
        match batch {
            Ok(Some(batch)) => {
                running
                    .updates
                    .request_preview(&mut running.model, &running.startup, preview);
                if batch.redraw {
                    summary::update(&mut running.model);
                }
                (redraw || batch.redraw, batch.more_updates)
            }
            Ok(None) => (redraw, false),
            Err(error) => (self.issue(Issue::Unreadable(error.to_string())), false),
        }
    }
}

pub(super) struct Boards {
    changes: Receiver<BTreeMap<PathBuf, String>>,
    _stop: Sender<()>,
    entries: BTreeMap<BoardId, BoardEntry>,
}

impl Boards {
    pub fn new() -> Result<Self, Error> {
        board_list::directory()?;
        let (send, changes) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::channel::<()>();
        thread::Builder::new()
            .name("mailbox-discovery".into())
            .spawn(move || {
                let mut previous = None;
                loop {
                    if let Ok(entries) = board_names::list_quiet() {
                        let names: BTreeMap<_, _> = entries
                            .into_iter()
                            .map(|entry| {
                                let name = board_names::name(&entry);
                                (entry.path, name)
                            })
                            .collect();
                        if previous.as_ref() != Some(&names) {
                            if send.send(names.clone()).is_err() {
                                return;
                            }
                            previous = Some(names);
                        }
                    }
                    if stopped.recv_timeout(REFRESH) == Err(RecvTimeoutError::Disconnected) {
                        return;
                    }
                }
            })
            .map_err(config::io_error)?;
        Ok(Self {
            changes,
            _stop: stop,
            entries: BTreeMap::new(),
        })
    }

    pub fn get(&self, id: &BoardId) -> Option<&BoardEntry> {
        self.entries.get(id)
    }

    pub fn ordered(&self) -> Vec<&BoardEntry> {
        let mut entries: Vec<_> = self.entries.values().collect();
        entries.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        entries
    }

    pub fn drain(
        &mut self,
        mut trace: Option<(&BoardId, &mut LiveTrace)>,
        preview: Option<(&BoardId, &Address)>,
    ) -> (bool, bool) {
        let mut redraw = false;
        let mut pending = false;
        if let Ok(names) = self.changes.try_recv() {
            self.entries
                .retain(|path, _| names.contains_key(path.as_ref()));
            for (path, name) in names {
                if let Some(entry) = self.entries.get_mut(path.as_path()) {
                    entry.name = name;
                } else {
                    let entry = BoardEntry::new(path, name);
                    self.entries.insert(entry.id.clone(), entry);
                }
            }
            redraw = true;
        }
        for (id, entry) in &mut self.entries {
            let trace = trace
                .as_mut()
                .filter(|(key, _)| *key == id)
                .map(|(_, trace)| &mut **trace);
            let preview = preview
                .filter(|(key, _)| *key == id)
                .map(|(_, address)| address);
            let (changed, more) = entry.drain(trace, preview);
            redraw |= changed;
            pending |= more;
        }
        (redraw, pending)
    }
}
