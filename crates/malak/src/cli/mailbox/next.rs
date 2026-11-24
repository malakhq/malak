//! Wait for one lead event without streaming mailbox history.

use super::{
    PreviewDoor, REFRESH, follow_tail, live_status,
    messages::{self, LivePreviews, RecordBody},
    model::{Address, Event, Model, Participant, QuestionKind, Status, inbox_recipient},
    now_ms, owner_commands, owner_for_board, plain, questions, render, status, stuck,
};
use crate::cli::{
    config::{Config, io_error},
    participant::{Session, check_preview_board},
};
use malak::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

#[expect(
    clippy::large_enum_variant,
    reason = "the bounded channel carries events without allocating each one"
)]
enum Update {
    Event(Event),
    Resumed,
    End(Result<(), Error>),
}

pub(in crate::cli) fn run(config: Option<PathBuf>, timeout: Option<u32>) -> Result<(), Error> {
    let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs(seconds.into()));
    let started = now_ms();
    check_preview_board(config.clone())?;
    let settings = Arc::new(Config::read(config.clone())?.board);
    let (mut watcher, _) = owner_for_board(&settings)?;
    // Subscribe first so arrivals during the startup snapshot cannot fall between the two.
    let tail = owner_commands::Tail::start(&mut watcher, None)?;
    let (mut reader, _) = owner_for_board(&settings)?;
    let initial = status(&mut reader, false)?;
    let mut reader = Some(reader);
    let mut _registration = plain::register_lead_watcher(&settings);
    let (tx, rx) = mpsc::sync_channel(128);
    let tail_settings = Arc::clone(&settings);
    thread::Builder::new()
        .name("mailbox-next-tail".into())
        .spawn(move || {
            let end = follow_tail(
                &tail_settings,
                watcher,
                tail,
                Some(&mut || tx.send(Update::Resumed).is_ok()),
                |event| tx.send(Update::Event(event)).is_ok(),
            );
            let _ = tx.send(Update::End(end));
        })
        .map_err(io_error)?;

    let mut output = io::stdout().lock();
    let Some(mut view) = View::load(&config, &initial, started, None, &mut output)? else {
        return Ok(());
    };
    let mut baseline = initial;
    let mut resuming = false;
    let mut next_refresh = Instant::now() + REFRESH;
    let mut next_due = None;
    loop {
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            plain::write_plain(&mut output, "No events.\n")?;
            return Ok(());
        }
        if now >= next_refresh {
            if let Some(snapshot) = live_status(&mut reader, false, &settings)? {
                if resuming {
                    _registration = plain::register_lead_watcher(&settings);
                    let Some(reconnected) =
                        View::load(&config, &snapshot, started, Some(&baseline), &mut output)?
                    else {
                        return Ok(());
                    };
                    view = reconnected;
                    baseline = snapshot;
                    resuming = false;
                } else if snapshot.run == view.model.run {
                    view.model.update(snapshot);
                }
            }
            next_refresh = Instant::now() + REFRESH;
        }
        let until = deadline.map_or(next_refresh, |deadline| deadline.min(next_refresh));
        let wait = until.saturating_duration_since(Instant::now());
        let wait = next_due.map_or(wait, |due: u64| {
            wait.min(Duration::from_millis(due.saturating_sub(now_ms())))
        });
        match rx.recv_timeout(wait) {
            Ok(Update::Event(event)) => {
                if view.is_lead_event(&event) {
                    if view.model.missing_event_participant(&event)
                        && let Some(snapshot) = live_status(&mut reader, false, &settings)?
                    {
                        view.model.update(snapshot);
                    }
                    plain::ensure_preview(&mut view.model, &event, view.previews.as_mut());
                    plain::write_plain(&mut output, &render::plain_event(&view.model, &event))?;
                    return Ok(());
                }
                view.model.apply_cursors(&event);
                view.stuck.event(&view.model, &event);
            }
            Ok(Update::Resumed) => {
                resuming = true;
                next_refresh = Instant::now();
                continue;
            }
            Ok(Update::End(end)) => return end,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::unavailable("the lead event stream ended"));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        next_due = None;
        if !resuming && reader.is_some() {
            let (blocks, next) = view.stuck.report(&view.model, now_ms());
            if let Some(block) = blocks
                .split_inclusive("\n\n")
                .next()
                .filter(|s| !s.is_empty())
            {
                plain::write_plain(&mut output, block)?;
                return Ok(());
            }
            next_due = next;
        }
    }
}

struct View {
    model: Model,
    stuck: stuck::Watch,
    previews: Option<LivePreviews>,
}

impl View {
    fn load(
        config: &Option<PathBuf>,
        snapshot: &Status,
        started: u64,
        before: Option<&Status>,
        output: &mut impl Write,
    ) -> Result<Option<Self>, Error> {
        let mut model = Model::new(snapshot.clone(), Some("lead".into()));
        model.enable_lead_watch();
        let mut stuck = stuck::Watch::starting_after(snapshot, Some(started));
        let door = PreviewDoor::Single(config.clone());
        let previews = if let Some((mut session, id)) = door.open() {
            model.viewer = Some(id.to_string());
            if let Some(viewer) = snapshot
                .participants
                .iter()
                .find(|person| person.id == id.as_str())
            {
                if print_waiting(&mut model, snapshot, viewer, &mut session, before, output)? {
                    return Ok(None);
                }
                seed_work(snapshot, viewer, &mut session, &mut stuck)?;
            }
            Some(LivePreviews::new(door, session, id))
        } else {
            None
        };
        Ok(Some(Self {
            model,
            stuck,
            previews,
        }))
    }

    fn is_lead_event(&self, event: &Event) -> bool {
        event.event == "stored"
            && matches!(
                event.channel.as_deref(),
                Some("lead" | "questions" | "orders")
            )
            && self.model.visible(event)
    }
}

fn print_waiting(
    model: &mut Model,
    snapshot: &Status,
    viewer: &Participant,
    session: &mut Session,
    before: Option<&Status>,
    output: &mut impl Write,
) -> Result<bool, Error> {
    let mut waiting = false;
    for channel in messages::readable_channels(snapshot.channels.iter(), viewer) {
        let addresses: Vec<Address> = match channel.name.as_str() {
            "lead" => channel
                .due_positions("lead")
                .map(|pos| channel.address(pos))
                .collect(),
            "questions" if before.is_none() => {
                // Only a reply on questions can close an owner question.
                questions::open(&[channel], session, RecordBody::Omit)?
                    .into_iter()
                    .filter(|question| question.kind == QuestionKind::Owner)
                    .map(|question| channel.address(question.position))
                    .collect()
            }
            "questions" | "orders" if before.is_some() => {
                // The tail is live again. Pick up records stored before resubscription.
                let head = before
                    .and_then(|snapshot| {
                        snapshot.channels.iter().find(|old| {
                            old.name == channel.name && old.creation == channel.creation
                        })
                    })
                    .map_or(channel.floor, |old| channel.floor.max(old.head));
                (head..channel.head)
                    .map(|pos| channel.address(pos))
                    .collect()
            }
            _ => continue,
        };
        for address in addresses {
            if let Some(preview) = messages::fetch(session, viewer, &address)? {
                let event = Event::historical(&address, &preview);
                model.fetch_result(address, Some(preview));
                waiting = true;
                if !plain::write_plain(output, &render::plain_event(model, &event))?
                    || before.is_some()
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(waiting)
}

fn seed_work(
    snapshot: &Status,
    viewer: &Participant,
    session: &mut Session,
    watch: &mut stuck::Watch,
) -> Result<(), Error> {
    for channel in messages::readable_channels(snapshot.channels.iter(), viewer) {
        if let Some(worker) =
            inbox_recipient(&snapshot.participants, &channel.name).filter(|worker| !worker.revoked)
        {
            for pos in channel.due_positions(&worker.id) {
                let address = channel.address(pos);
                if let Some(preview) = messages::read_message(session, &address, RecordBody::Omit)?
                {
                    watch.stored(address, &preview.sender, &preview.kind, preview.at_ms());
                }
            }
        }
    }
    Ok(())
}
