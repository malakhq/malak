//! Stream retained and live mailbox events as printable blocks.

use super::{
    REFRESH, Update, history,
    messages::{RecordBody, add_quote, fetch, read_message},
    model::{Event, Model, Status},
    owner_commands, refresh, render, status_with_workers, stuck, summary,
};
use crate::cli::{
    board::OWNER_FOLDER,
    config::{self, Config, io_error},
    output::{self, now_ms},
    participant::Session,
};
use malak::{Error, ParticipantId};
use malak_fs::Directory;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

pub(super) fn register_lead_watcher(config: Option<PathBuf>) -> Option<Directory> {
    let board = Config::read(config).ok()?.board;
    let door =
        config::open_owned_directory(&board.socket_folder.join(OWNER_FOLDER), true).ok()??;
    // A shared lock allows several watchers. The hook probes with a short
    // exclusive lock, and the kernel releases a watcher's lock on exit.
    door.file().lock_shared().ok()?;
    Some(door)
}

pub(super) fn run(
    model: &mut Model,
    rx: Receiver<Update>,
    mut session: Option<(Session, ParticipantId)>,
    title: &str,
    mut watch: Option<stuck::Watch>,
    mut reader: owner_commands::Client,
    state: &std::path::Path,
) -> Result<(), Error> {
    let mut output = io::stdout().lock();
    let mut loading = true;
    let title = format!("{title} ({})", output::local_zone(now_ms()));
    let mut heading = output::Format::stdout().details(&title, &[]);
    for row in summary::lines(model, now_ms(), usize::MAX) {
        heading.push_str(&row);
        heading.push('\n');
    }
    heading.push_str(render::HISTORY_LABEL);
    heading.push('\n');
    if !write_plain(&mut output, &heading)? {
        return Ok(());
    }
    let mut next_due: Option<u64> = None;
    let mut next_refresh = Instant::now() + REFRESH;
    loop {
        if Instant::now() >= next_refresh {
            refresh(model, status_with_workers(&mut reader, false, state)?)?;
            next_refresh = Instant::now() + REFRESH;
        }
        // The local owner entrance and the board use the same host clock.
        let until_refresh = next_refresh.saturating_duration_since(Instant::now());
        let wait = next_due.map_or(until_refresh, |due| {
            until_refresh.min(Duration::from_millis(due.saturating_sub(now_ms())))
        });
        let update = match rx.recv_timeout(wait) {
            Ok(update) => Some(update),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        match update {
            Some(Update::Event(event)) => {
                // A worker can start sending before the next scheduled roster
                // refresh. Resolve its name before printing an immutable line.
                if watch.is_some()
                    && model.visible(&event)
                    && model.missing_event_participant(&event)
                {
                    refresh(model, status_with_workers(&mut reader, false, state)?)?;
                    next_refresh = Instant::now() + REFRESH;
                }
                let is_new = model.apply(event.clone());
                if let Some(watch) = &mut watch {
                    watch.event(model, &event);
                }
                if !loading && is_new && !print_event(model, &event, session.as_mut(), &mut output)?
                {
                    return Ok(());
                }
            }
            Some(Update::Backfill(address, Some(preview))) => {
                if let Some(watch) = &mut watch {
                    watch.stored(
                        address.clone(),
                        &preview.sender,
                        &preview.kind,
                        preview.at_ms(),
                    );
                }
                model.backfill(address, preview, false);
                continue;
            }
            Some(Update::BackfillDone) => {
                let initial = model.feed().rev().cloned().collect::<Vec<_>>();
                for event in &initial {
                    ensure_preview(model, event, session.as_mut());
                }
                for event in &initial {
                    if !write_plain(&mut output, &render::plain_event(model, event))? {
                        return Ok(());
                    }
                }
                loading = false;
            }
            Some(Update::End(end)) => return end,
            _ => {}
        }
        if let Some(watch) = &mut watch {
            let (blocks, next) = watch.report(model, now_ms());
            next_due = next;
            if !blocks.is_empty() && !write_plain(&mut output, &blocks)? {
                return Ok(());
            }
        }
    }
}

fn print_event(
    model: &mut Model,
    event: &Event,
    session: Option<&mut (Session, ParticipantId)>,
    output: &mut impl Write,
) -> Result<bool, Error> {
    if !model.visible(event) {
        return Ok(true);
    }
    ensure_preview(model, event, session);
    write_plain(output, &render::plain_event(model, event))
}

fn ensure_preview(
    model: &mut Model,
    event: &Event,
    session: Option<&mut (Session, ParticipantId)>,
) {
    if let Some(address) = event.address()
        && !model.requested(&address)
    {
        let preview = session.and_then(|(session, id)| {
            let viewer = model.participant(id.as_str())?;
            fetch(session, viewer, &address).ok().flatten()
        });
        model.fetch_result(address, preview);
    }
}

pub(super) fn write_plain(output: &mut impl Write, text: &str) -> Result<bool, Error> {
    match output
        .write_all(text.as_bytes())
        .and_then(|_| output.flush())
    {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

pub(super) fn backfill(
    status: Status,
    config: Option<PathBuf>,
    reviewed: Option<(malak::ParticipantId, summary::Previews)>,
    tx: &SyncSender<Update>,
) {
    if let Some((mut session, id)) = Session::open_preview(config)
        && let Some(viewer) = status
            .participants
            .iter()
            .find(|person| person.id == id.as_str())
    {
        let mut reviewed = reviewed
            .filter(|(viewer, _)| *viewer == id)
            .map_or_else(summary::Previews::default, |(_, previews)| previews);
        for channel in history::retained(&status, id.as_str()) {
            for address in channel.addresses() {
                let preview = reviewed.take(&address, |address| {
                    read_message(&mut session, address, RecordBody::Full)
                });
                match preview {
                    Ok(Some(mut preview)) => {
                        add_quote(&mut session, viewer, &mut preview);
                        if tx.send(Update::Backfill(address, Some(preview))).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        }
    }
    let _ = tx.send(Update::BackfillDone);
}
