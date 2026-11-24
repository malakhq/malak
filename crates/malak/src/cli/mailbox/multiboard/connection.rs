//! One independent lifecycle per discovered configuration.

use super::{BoardId, Issue};
use crate::cli::{
    board_group,
    board_version::BoardVersion,
    client_transport::ClientTransport,
    config::{self, Config, io_error},
    mailbox::{
        LiveBoard, PreviewDoor, REFRESH, SnapshotRequest, Update, live_previews,
        messages::PreviewRequest,
        model::{Model, Status},
        read_tail, status_with_workers,
    },
    owner_commands::{Client, Tail},
    owner_for_board_quiet,
};
use malak::{Error, Outcome};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread,
    time::Instant,
};

pub(super) enum Observation {
    Issue(Issue),
    Running(Box<Connected>),
}

pub(super) struct Connected {
    pub board: LiveBoard,
    pub initial: Status,
    pub updates: Receiver<Update>,
    pub previews: SyncSender<PreviewRequest>,
    pub older: bool,
}

#[derive(Default)]
struct Links {
    closed: bool,
    open: Vec<Weak<ClientTransport>>,
}

pub(super) struct Watch {
    links: Arc<Mutex<Links>>,
    stop: Option<SyncSender<()>>,
    pub observations: Receiver<Observation>,
}

impl Watch {
    pub fn start(path: BoardId) -> Result<Self, Error> {
        let (send, observations) = mpsc::sync_channel(1);
        let (stop, stopped) = mpsc::sync_channel(0);
        let links = Arc::new(Mutex::new(Links::default()));
        let active = Arc::clone(&links);
        thread::Builder::new()
            .name("mailbox-board".into())
            .spawn(move || watch(&path, &send, &stopped, &active))
            .map_err(io_error)?;
        Ok(Self {
            links,
            stop: Some(stop),
            observations,
        })
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.take();
        let mut links = self.links.lock().unwrap();
        links.closed = true;
        for transport in links.open.iter().filter_map(Weak::upgrade) {
            transport.close_unix_link();
        }
    }
}

fn watch(
    path: &Path,
    send: &SyncSender<Observation>,
    stopped: &Receiver<()>,
    links: &Mutex<Links>,
) {
    loop {
        let fail = |error: Error| Issue::Unreadable(error.to_string());
        let issue = match Config::read(Some(path.to_owned())) {
            Ok(config) => {
                match board_group::is_running(&config.board) {
                    Ok(false) => Issue::Stopped,
                    Ok(true) => {
                        let issue = run(path, &config.board, send, stopped, links)
                            .err()
                            .unwrap_or(Issue::Connecting);
                        // A disappearing socket or closing owner door is normal
                        // during STOP. Recheck the same lock used by board start.
                        if matches!(board_group::is_running(&config.board), Ok(false)) {
                            Issue::Stopped
                        } else {
                            issue
                        }
                    }
                    Err(error) => fail(error),
                }
            }
            Err(error) => fail(error),
        };
        if send.send(Observation::Issue(issue)).is_err() {
            break;
        }
        if stopped.recv_timeout(REFRESH) == Err(RecvTimeoutError::Disconnected) {
            break;
        }
    }
}

struct Connection {
    client: Client,
    transport: Arc<ClientTransport>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.transport.close_unix_link();
    }
}

fn connect(
    settings: &config::Board,
    links: &Mutex<Links>,
) -> Result<(Connection, BoardVersion), Error> {
    let (client, version, transport) = owner_for_board_quiet(settings)?;
    let connection = Connection { client, transport };
    let mut links = links.lock().unwrap();
    if links.closed {
        return Err(Error::unavailable("mailbox board was removed"));
    }
    links.open.retain(|link| link.strong_count() != 0);
    links.open.push(Arc::downgrade(&connection.transport));
    Ok((connection, version))
}

fn run(
    path: &Path,
    settings: &config::Board,
    send: &SyncSender<Observation>,
    stopped: &Receiver<()>,
    links: &Mutex<Links>,
) -> Result<(), Issue> {
    let owner_error = |error: Error| {
        let prefix = match error.outcome() {
            Outcome::Refused => "owner door refused",
            _ => "owner door unavailable",
        };
        Issue::Unreadable(format!("{prefix}: {error}"))
    };
    let (mut watcher, version) = connect(settings, links).map_err(owner_error)?;
    let older = version.mismatch().is_some();
    let mut tail = Tail::start(&mut watcher.client, None).map_err(|error| {
        if older {
            Issue::Older
        } else {
            owner_error(error)
        }
    })?;
    let (mut reader, _) = connect(settings, links).map_err(owner_error)?;
    let snapshot = |reader: &mut Connection| {
        status_with_workers(&mut reader.client, false, &settings.state).map_err(|error| {
            if older || error.to_string().contains("older than this mailbox") {
                Issue::Older
            } else {
                owner_error(error)
            }
        })
    };
    let initial = snapshot(&mut reader)?;
    let (tx, updates) = mpsc::sync_channel(128);
    let (finished, tail_end) = mpsc::sync_channel(1);
    let events = tx.clone();
    let tail_transport = Arc::clone(&watcher.transport);
    let _tail_link = CloseLink(tail_transport);
    thread::Builder::new()
        .name("mailbox-board-tail".into())
        .spawn(move || {
            let end = read_tail(&mut watcher.client, &mut tail, |event| {
                events.send(Update::Event(event)).is_ok()
            });
            drop(watcher);
            let _ = finished.send(end);
        })
        .map_err(|error| owner_error(io_error(error)))?;
    let (snapshots, requests) = mpsc::channel::<SnapshotRequest>();
    let door = PreviewDoor::Discovered(PathBuf::from(path));
    let waiting = Model::new(initial.clone(), None)
        .due_inboxes()
        .into_iter()
        .filter_map(|inbox| inbox.addresses().next())
        .collect();
    let previews =
        live_previews(initial.clone(), waiting, door.clone(), tx.clone()).map_err(owner_error)?;
    let board = LiveBoard {
        previews: door,
        snapshots,
    };
    send.send(Observation::Running(Box::new(Connected {
        board,
        initial,
        updates,
        previews,
        older,
    })))
    .map_err(|_| Issue::Stopped)?;

    snapshot_until(&mut reader, snapshot, &requests, tx, tail_end, stopped)
}

struct CloseLink(Arc<ClientTransport>);

impl Drop for CloseLink {
    fn drop(&mut self) {
        self.0.close_unix_link();
    }
}

fn snapshot_until(
    reader: &mut Connection,
    snapshot: impl Fn(&mut Connection) -> Result<Status, Issue>,
    requests: &Receiver<SnapshotRequest>,
    updates: SyncSender<Update>,
    tail_end: Receiver<Result<(), Error>>,
    stopped: &Receiver<()>,
) -> Result<(), Issue> {
    let mut next = Instant::now() + REFRESH;
    let mut waiting = Vec::<SnapshotRequest>::new();
    loop {
        if matches!(stopped.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
            return Ok(());
        }
        if let Ok(end) = tail_end.try_recv() {
            return end
                .map_err(|error| Issue::Unreadable(format!("owner event stream ended: {error}")));
        }
        let remaining = next.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            match requests.recv_timeout(remaining) {
                // Trace needs a snapshot taken after the request so it can
                // include stores that happened since the last refresh.
                Ok(request) => {
                    waiting.push(request);
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        let current = snapshot(reader)?;
        for request in waiting.drain(..) {
            let _ = request.send(Ok(current.clone()));
        }
        if updates.send(Update::Status(current)).is_err() {
            return Ok(());
        }
        next = Instant::now() + REFRESH;
    }
}
