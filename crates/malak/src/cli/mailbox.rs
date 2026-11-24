use super::{
    board_group, board_names,
    client_transport::{link_failed, link_io_error, reconnect},
    config::{self, io_error},
    output::{self, now_ms},
    owner, owner_commands, owner_for_board,
    participant::{Session, check_preview_board, preview_handover, wait_for_inbox},
    worker_launch,
};
use malak::{Error, ParticipantId};
use messages::{LivePreviews, PreviewRequest, RecordBody, preview_worker, read_message};
use model::{Address, Event, Model, Preview, Status};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

mod activity;
mod history;
mod interactive;
mod messages;
mod model;
mod multiboard;
mod next;
mod orders;
mod plain;
pub(in crate::cli) use plain::LEAD_TICK;
mod questions;
mod render;
mod screen;
mod sender;
mod stuck;
mod summary;
mod terminal;
mod text;
mod trace;
mod view;
mod waiting;
mod who;

pub(super) use next::run as next;
pub(super) use trace::{id as trace_id, run as trace};
pub(super) use waiting::run as waiting;

pub(super) fn worker_next(
    config: Option<PathBuf>,
    name: &str,
    timeout: Option<u32>,
) -> Result<(), Error> {
    let id = preview_handover(config.clone())?.id().clone();
    let wrong_worker = || Error::refused("use --for NAME for your own worker inbox");
    if id.is_lead() {
        return Err(wrong_worker());
    }
    let status = owner_commands::status(&mut owner(config.clone())?, &[("id", id.as_str())])?;
    let own_worker = status["participants"].as_array().is_some_and(|people| {
        people.iter().any(|person| {
            person["id"] == id.as_str() && person["name"] == name && person["revoked"] == false
        })
    });
    if !own_worker {
        return Err(wrong_worker());
    }
    wait_for_inbox(config, id.inbox().to_string(), timeout)
}

const REFRESH: Duration = Duration::from_secs(1);

type SnapshotRequest = SyncSender<Result<Status, Error>>;

struct LiveBoard {
    previews: PreviewDoor,
    snapshots: Sender<SnapshotRequest>,
}

#[derive(Clone)]
enum PreviewDoor {
    Single(Option<PathBuf>),
    Discovered(PathBuf),
}

impl PreviewDoor {
    fn open(&self) -> Option<(Session, ParticipantId)> {
        match self {
            Self::Single(config) => Session::open_preview(config.clone()),
            Self::Discovered(config) => Session::open_mailbox_preview(config.clone()),
        }
    }
}

enum Update {
    Event(Event),
    Status(Status),
    Preview(Address, Result<Option<Preview>, Error>),
    PreviewsUnavailable,
    Backfill(Address, Option<Preview>),
    Recent(Option<String>, Vec<history::Record>),
    BackfillDone,
    History(BTreeMap<String, u64>),
    End(Result<(), Error>),
}

pub(super) fn run(
    config: Option<PathBuf>,
    name: Option<String>,
    plain: bool,
    roster: bool,
    open_questions: bool,
    json: bool,
    review_orders: bool,
) -> Result<(), Error> {
    if config.is_none()
        && name.is_none()
        && !plain
        && !roster
        && !open_questions
        && !json
        && !review_orders
        && output::Format::stdout().interactive()
    {
        return interactive::run_boards();
    }
    check_preview_board(config.clone())?;
    if review_orders {
        return orders::run(config, plain);
    }
    if open_questions {
        return questions::run(config, plain, json);
    }
    watch(config, name, plain, roster, None)
}

fn watch(
    config: Option<PathBuf>,
    name: Option<String>,
    plain: bool,
    roster: bool,
    initial_trace: Option<String>,
) -> Result<(), Error> {
    let lead_watch = plain && name.as_deref() == Some("lead");
    let settings = Arc::new(config::Config::read(config.clone())?.board);
    let (mut watcher, _) = owner_for_board(&settings)?;
    let tail = owner_commands::Tail::start(&mut watcher, None)?;
    let (mut reader, _) = owner_for_board(&settings)?;
    let initial = status_with_workers(&mut reader, roster, &settings.state)?;
    let title = name.as_ref().map_or_else(
        || "Mailbox activity".into(),
        |name| format!("Mailbox activity for {name}"),
    );
    let mut model = Model::new(initial.clone(), name);
    if !model.has_selection() {
        return Err(Error::refused("no participant by that name"));
    }
    let (tx, rx) = mpsc::sync_channel(128);
    let plain = plain || !output::Format::stdout().interactive();
    let events = tx.clone();
    let tail_settings = Arc::clone(&settings);
    thread::Builder::new()
        .name("mailbox-tail".into())
        .spawn(move || {
            let end = follow_tail(&tail_settings, watcher, tail, None, |event| {
                events.send(Update::Event(event)).is_ok()
            });
            let _ = events.send(Update::End(end));
        })
        .map_err(io_error)?;
    if plain && !roster {
        let watch = (model.selected_id() == Some("lead") && lead_watch).then(|| {
            model.enable_lead_watch();
            stuck::Watch::new(&initial)
        });
        let _watch_registration = watch
            .as_ref()
            .and_then(|_| plain::register_lead_watcher(&settings));
        let mut session = Session::open_preview(config.clone());
        let reviewed = session.as_mut().map(|(session, viewer)| {
            model.viewer = Some(viewer.to_string());
            let previews = summary::load(&mut model, &initial, viewer.as_str(), |address| {
                read_message(session, address, RecordBody::Full)
            });
            (viewer.clone(), previews)
        });
        let previews = session.map(|(session, id)| {
            LivePreviews::new(PreviewDoor::Single(config.clone()), session, id)
        });
        thread::Builder::new()
            .name("mailbox-backfill".into())
            .spawn(move || plain::backfill(initial, config, reviewed, &tx))
            .map_err(io_error)?;
        return plain::run(&mut model, rx, previews, &title, watch, reader, &settings);
    }
    let snapshots = tx.clone();
    let (trace_snapshots, requests) = mpsc::channel::<SnapshotRequest>();
    thread::Builder::new()
        .name("mailbox-status".into())
        .spawn(move || {
            let mut reader = Some(reader);
            loop {
                let request = if !roster {
                    match requests.recv_timeout(REFRESH) {
                        Ok(request) => Some(request),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                } else {
                    thread::sleep(REFRESH);
                    None
                };
                let current = live_status(&mut reader, roster, &settings);
                if let Some(request) = request {
                    let reply = current.clone().and_then(|status| {
                        status.ok_or_else(|| Error::unavailable("mailbox status link dropped"))
                    });
                    let _ = request.send(reply);
                }
                let update = match current {
                    Ok(Some(status)) => Update::Status(status),
                    Ok(None) => continue,
                    Err(error) => {
                        // The activity view ends with its tail. When the board
                        // stops, this separate connection can close before the
                        // tail thread has handed the STOP event to the view.
                        if roster {
                            let _ = snapshots.send(Update::End(Err(error)));
                        }
                        break;
                    }
                };
                if snapshots.send(update).is_err() {
                    break;
                }
            }
        })
        .map_err(io_error)?;
    if roster {
        let selected = model.selected_id().map(str::to_owned);
        let history_config = config.clone();
        thread::Builder::new()
            .name("mailbox-history".into())
            .spawn(move || {
                let history = who::backfill(&initial, selected.as_deref(), history_config);
                let _ = tx.send(Update::History(history));
            })
            .map_err(io_error)?;
        if output::Format::stdout().interactive() {
            interactive::run(&mut model, rx, None, None, None)
        } else {
            roster_snapshot(&mut model, rx)
        }
    } else {
        let path = match &config {
            Some(path) => std::path::absolute(path).map_err(io_error)?,
            None => config::default_path()?,
        };
        let entry = board_group::Entry::read(path);
        let name = board_names::name(&entry);
        entry.board?;
        let waiting = model
            .due_inboxes()
            .into_iter()
            .filter_map(|inbox| inbox.addresses().next())
            .collect();
        let door = PreviewDoor::Single(config);
        let previews = live_previews(initial, waiting, door.clone(), tx)?;
        let board = LiveBoard {
            previews: door,
            snapshots: trace_snapshots,
        };
        interactive::run(
            &mut model,
            rx,
            Some(previews),
            Some((&name, board)),
            initial_trace,
        )
    }
}

fn live_previews(
    status: Status,
    waiting: Vec<Address>,
    door: PreviewDoor,
    tx: SyncSender<Update>,
) -> Result<SyncSender<PreviewRequest>, Error> {
    let history_door = door.clone();
    let history_tx = tx.clone();
    thread::Builder::new()
        .name("mailbox-backfill".into())
        .spawn(move || history::backfill(status, waiting, history_door, &history_tx))
        .map_err(io_error)?;
    preview_worker(tx, door)
}

fn read_tail(
    watcher: &mut owner_commands::Client,
    tail: &mut owner_commands::Tail,
    mut observe: impl FnMut(Event) -> bool,
) -> Result<(), Error> {
    while let Some(value) = tail.next(watcher)? {
        let mut event: Event = serde_json::from_value(value).map_err(invalid)?;
        event.at_ms = event.at_ms.or_else(|| Some(now_ms()));
        if !observe(event) {
            break;
        }
    }
    Ok(())
}

fn follow_tail(
    settings: &config::Board,
    mut watcher: owner_commands::Client,
    mut tail: owner_commands::Tail,
    mut resumed: Option<&mut dyn FnMut() -> bool>,
    mut observe: impl FnMut(Event) -> bool,
) -> Result<(), Error> {
    loop {
        let mut listening = true;
        let result = read_tail(&mut watcher, &mut tail, |event| {
            listening = observe(event);
            listening
        });
        match result {
            Ok(()) if resumed.is_some() && listening => {}
            Err(error) if link_failed(&error) => {}
            result => return result,
        }
        drop(watcher);
        (watcher, tail) = reconnect(Instant::now() + Duration::from_secs(30), |_| {
            let (mut watcher, _) = owner_for_board(settings).map_err(link_io_error)?;
            let tail = owner_commands::Tail::start(&mut watcher, None)?;
            Ok((watcher, tail))
        })?;
        if resumed.as_mut().is_some_and(|resumed| !resumed()) {
            return Ok(());
        }
    }
}

fn roster_snapshot(model: &mut Model, rx: Receiver<Update>) -> Result<(), Error> {
    while let Ok(update) = rx.recv() {
        match update {
            Update::Event(event) => model.observe(&event),
            Update::Status(status) => refresh(model, status)?,
            Update::History(history) => {
                model.set_history(history);
                return who::print(model);
            }
            Update::End(end) => return end,
            _ => {}
        }
    }
    Ok(())
}

fn refresh(model: &mut Model, status: Status) -> Result<(), Error> {
    if status.run != model.run {
        return Err(Error::unavailable("board restarted"));
    }
    model.update(status);
    Ok(())
}

fn status(client: &mut owner_commands::Client, roster: bool) -> Result<Status, Error> {
    decode_status(owner_commands::status(client, &[])?, roster)
}

fn status_with_workers(
    client: &mut owner_commands::Client,
    roster: bool,
    state: &Path,
) -> Result<Status, Error> {
    let mut snapshot = status(client, roster)?;
    if let Ok(Some(launches)) = worker_launch::open(state) {
        for person in &mut snapshot.participants {
            if let Ok(id) = person.id.parse::<ParticipantId>()
                && !id.is_lead()
            {
                person.launcher_state = worker_launch::state(&launches, &id).ok().flatten();
            }
        }
    }
    Ok(snapshot)
}

fn live_status(
    reader: &mut Option<owner_commands::Client>,
    roster: bool,
    settings: &config::Board,
) -> Result<Option<Status>, Error> {
    let result = (|| {
        if reader.is_none() {
            *reader = Some(owner_for_board(settings)?.0);
        }
        status_with_workers(reader.as_mut().unwrap(), roster, &settings.state)
    })();
    match result {
        Err(error) if link_failed(&error) => {
            *reader = None;
            Ok(None)
        }
        result => result.map(Some),
    }
}

pub(super) fn board_counts(client: &mut owner_commands::Client) -> Result<(usize, u64), Error> {
    let status = status(client, false)?;
    let participants = status
        .participants
        .iter()
        .filter(|person| !person.revoked)
        .count();
    let waiting = status
        .channels
        .iter()
        .find(|channel| channel.name == "lead")
        .map_or(0, |channel| {
            let due = channel.due_positions("lead");
            due.end.saturating_sub(due.start)
        });
    Ok((participants, waiting))
}

fn decode_status(reply: serde_json::Value, roster: bool) -> Result<Status, Error> {
    if reply["channels"].as_array().is_some_and(|channels| {
        channels
            .iter()
            .any(|channel| channel.get("readers").is_none())
    }) || roster && reply.get("started_at_ms").is_none()
    {
        return Err(Error::unavailable(
            "board is older than this mailbox client. Restart the board with this version of malak",
        ));
    }
    serde_json::from_value(reply).map_err(invalid)
}

fn invalid(_: serde_json::Error) -> Error {
    Error::unavailable("invalid owner mailbox data")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn boards_with_the_older_status_shape_get_upgrade_instructions() {
        let older = json!({"channels": [{"name": "lead"}]});
        let error = decode_status(older, false).err().unwrap();

        assert!(
            error
                .to_string()
                .contains("Restart the board with this version")
        );
    }
}
