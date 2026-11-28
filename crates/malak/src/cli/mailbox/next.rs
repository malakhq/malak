//! Wait for one lead event without streaming mailbox history.

use super::{
    PreviewDoor, REFRESH, follow_tail, live_status,
    messages::{self, LivePreviews, RecordBody},
    model::{Address, Channel, Event, Model, Participant, Status, inbox_recipient},
    now_ms, owner_commands, owner_for_board, plain, render, status, stuck,
};
use crate::cli::{
    client_transport::link_failed,
    config::{Config, io_error},
    guest_inbox,
    participant::{check_preview_board, message_block},
};
use malak::Error;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

mod state;
use state::{Shown, Store};

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
    let _ = crate::cli::org::lifecycle::on_command(config.clone());
    let settings = Arc::new(Config::read(config.clone())?.board);
    let (mut reader, _) = owner_for_board(&settings)?;
    let shown = Store::open(&settings.state)?;
    shown.update(|shown, first| {
        if first {
            // Begin following questions and orders here on the first run.
            // Lead mail has its own acknowledgement cursor.
            shown.start(&status(&mut reader, false)?.channels);
        }
        Ok(())
    })?;
    let (mut watcher, _) = owner_for_board(&settings)?;
    // Subscribe before catching up so messages during the snapshot also reach the tail.
    let tail = owner_commands::Tail::start(&mut watcher, None)?;
    let initial = status(&mut reader, false)?;
    let mut reader = Some(reader);
    let _tick = plain::TickMark::start(&settings);
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
    let mut view = View::open(&config, &initial, started);
    // Attach the original preview Seat before advertising readiness.
    // The auxiliary calls read must not widen the gap before that attachment.
    let mut _registration = plain::register_lead_watcher(&settings);
    if print_calls(&config, &shown, &mut output) {
        return Ok(());
    }
    if view.catch_up(&initial, &shown, &mut output)? {
        return Ok(());
    }
    if print_guest(&config, &shown, &mut output)? {
        return Ok(());
    }
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
            if view.model.viewer.as_deref() == Some("lead") {
                let _ = crate::cli::org::lifecycle::on_command(config.clone());
            }
            if let Some(snapshot) = live_status(&mut reader, false, &settings)? {
                if resuming {
                    let mut reconnected = View::open(&config, &snapshot, started);
                    if reconnected.catch_up(&snapshot, &shown, &mut output)? {
                        return Ok(());
                    }
                    view = reconnected;
                    _registration = plain::register_lead_watcher(&settings);
                    resuming = false;
                } else if snapshot.run == view.model.run {
                    view.model.update(snapshot.clone());
                    match view.print_waiting(&shown, &snapshot, &mut output) {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(error) if link_failed(&error) => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            if print_calls(&config, &shown, &mut output) {
                return Ok(());
            }
            if print_guest(&config, &shown, &mut output)? {
                return Ok(());
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
                if event.channel.as_deref() == Some("calls")
                    && print_calls(&config, &shown, &mut output)
                {
                    return Ok(());
                }
                if view.model.viewer.as_deref() == Some("lead") {
                    let _ = crate::cli::org::lifecycle::on_command(config.clone());
                }
                if view.is_lead_event(&event) {
                    if view.model.missing_event_participant(&event)
                        && let Some(snapshot) = live_status(&mut reader, false, &settings)?
                    {
                        view.model.update(snapshot);
                    }
                    if view.print_live(&shown, &event, &mut output)? {
                        return Ok(());
                    }
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

fn print_calls(config: &Option<PathBuf>, store: &Store, output: &mut impl Write) -> bool {
    // This auxiliary snapshot never owns the watcher's lifetime. Keep the
    // last shown calls on any failure; the original tail alone reconnects
    // or reports why watching ended. Silence also preserves its diagnostic.
    let mut snapshot = || -> Result<bool, Error> {
        let reader = crate::cli::org::seats::Reader::open(config.clone())?;
        let calls = crate::cli::org::calls::watch(&reader)?;
        store.update(|shown, _| {
            let pending: Vec<_> = calls
                .into_iter()
                .filter(|call| {
                    shown.calls.get(&call["link"].to_string())
                        != Some(&crate::cli::org::calls::signature(call))
                })
                .collect();
            if pending.is_empty() {
                return Ok(false);
            }
            let result = crate::cli::org::calls::result(pending);
            plain::write_plain(output, &format!("Calls waiting: {result}\n"))?;
            for call in result["items"].as_array().into_iter().flatten() {
                shown.calls.insert(
                    call["link"].to_string(),
                    crate::cli::org::calls::signature(call),
                );
            }
            Ok(true)
        })
    };
    snapshot().unwrap_or(false)
}

fn print_guest(
    config: &Option<PathBuf>,
    store: &Store,
    output: &mut impl Write,
) -> Result<bool, Error> {
    for guest in guest_inbox::for_lead(config) {
        let records = guest.read(None);
        if records.is_empty() {
            continue;
        }
        if store.update(|shown, _| {
            for record in records {
                let key = format!(
                    "{}/{}",
                    guest.key(),
                    record.label["channel"].as_str().unwrap_or("")
                );
                let (creation, position) = record.address()?;
                if shown.guests.is_new(&key, creation, position) {
                    if plain::write_plain(output, &message_block(&record.label, &record.body)?)? {
                        shown.guests.advance(&key, creation, position);
                    }
                    return Ok(true);
                }
            }
            Ok(false)
        })? {
            return Ok(true);
        }
    }
    Ok(false)
}

struct View {
    model: Model,
    stuck: stuck::Watch,
    previews: Option<LivePreviews>,
}

impl View {
    fn open(config: &Option<PathBuf>, snapshot: &Status, started: u64) -> Self {
        let mut model = Model::new(snapshot.clone(), Some("lead".into()));
        model.enable_lead_watch();
        let stuck = stuck::Watch::starting_after(snapshot, Some(started));
        let door = PreviewDoor::Single(config.clone());
        let preview = door.open();
        if let Some((_, id)) = &preview {
            model.viewer = Some(id.to_string());
        }
        Self {
            model,
            stuck,
            previews: preview.map(|(session, id)| LivePreviews::new(door, session, id)),
        }
    }

    fn catch_up(
        &mut self,
        snapshot: &Status,
        shown: &Store,
        output: &mut impl Write,
    ) -> Result<bool, Error> {
        if self.print_waiting(shown, snapshot, output)? {
            return Ok(true);
        }
        if let Some(previews) = &mut self.previews
            && let Some(viewer) = snapshot
                .participants
                .iter()
                .find(|person| person.id == previews.id.as_str())
        {
            seed_work(snapshot, viewer, previews, &mut self.stuck)?;
        }
        Ok(false)
    }

    fn is_lead_event(&self, event: &Event) -> bool {
        event.event == "stored"
            && event.id.as_deref() != Some("lead")
            && matches!(
                event.channel.as_deref(),
                Some("lead" | "questions" | "orders")
            )
            && self.model.visible(event)
    }

    fn print_live(
        &mut self,
        store: &Store,
        event: &Event,
        output: &mut impl Write,
    ) -> Result<bool, Error> {
        let Some(address) = event.address() else {
            return Ok(false);
        };
        store.update(|shown, _| {
            if address.position <= shown.through(&address.channel, &address.creation) {
                return Ok(false);
            }
            plain::ensure_preview(&mut self.model, event, self.previews.as_mut());
            if plain::write_plain(output, &render::plain_event(&self.model, event))? {
                if address.channel == "lead" {
                    shown.mail_shown(&address, now_ms());
                } else {
                    shown.advance(&address);
                }
            }
            Ok(true)
        })
    }

    fn print_waiting(
        &mut self,
        store: &Store,
        snapshot: &Status,
        output: &mut impl Write,
    ) -> Result<bool, Error> {
        let Some(previews) = &mut self.previews else {
            return Ok(false);
        };
        let Some(viewer) = snapshot
            .participants
            .iter()
            .find(|person| person.id == previews.id.as_str())
        else {
            return Ok(false);
        };
        let mut history = History {
            model: &mut self.model,
            previews,
            viewer,
            output,
        };
        store.update(|shown, _| history.catch_up(shown, &snapshot.channels))
    }
}

struct History<'a, W> {
    model: &'a mut Model,
    previews: &'a mut LivePreviews,
    viewer: &'a Participant,
    output: &'a mut W,
}

impl<W: Write> History<'_, W> {
    fn catch_up(&mut self, shown: &mut Shown, channels: &[Channel]) -> Result<bool, Error> {
        for name in ["lead", "questions", "orders"] {
            let Some(channel) = channels
                .iter()
                .find(|channel| channel.name == name && self.viewer.can_get(name))
            else {
                continue;
            };
            let through = shown.through(name, &channel.creation);
            if name == "lead" {
                if self.print_mail(shown, channel, through)? {
                    return Ok(true);
                }
                continue;
            }
            for pos in channel.floor.max(through.saturating_add(1))..channel.head {
                let address = channel.address(pos);
                let printed = self.print(&address)?;
                if printed != Some(false) {
                    shown.advance(&address);
                }
                if printed.is_some() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn print_mail(
        &mut self,
        shown: &mut Shown,
        channel: &Channel,
        through: u64,
    ) -> Result<bool, Error> {
        let due = channel.due_positions("lead");
        if due.is_empty() {
            return Ok(false);
        }
        let mut printed = false;
        if let Some(repeat_through) = shown.reminder_due(now_ms()) {
            let end = due.end.min(repeat_through.saturating_add(1));
            for pos in due.start..end {
                match self.print(&channel.address(pos))? {
                    Some(false) => return Ok(true),
                    Some(true) => printed = true,
                    None => {}
                }
            }
            // Admit later mail to the following reminder only after it has had
            // a full hour. The two cursors bound storage independently of inbox size.
            shown.reminded(now_ms());
        }
        for pos in due.start.max(through.saturating_add(1))..due.end {
            let address = channel.address(pos);
            match self.print(&address)? {
                Some(false) => return Ok(true),
                Some(true) => {
                    shown.mail_shown(&address, now_ms());
                    printed = true;
                }
                None => shown.advance(&address),
            }
        }
        Ok(printed)
    }

    fn print(&mut self, address: &Address) -> Result<Option<bool>, Error> {
        let Some(preview) = self.previews.fetch(self.viewer, address)? else {
            return Ok(None);
        };
        if preview.sender == "lead" {
            return Ok(None);
        }
        let event = Event::historical(address, &preview);
        self.model.fetch_result(address.clone(), Some(preview));
        plain::write_plain(self.output, &render::plain_event(self.model, &event)).map(Some)
    }
}

fn seed_work(
    snapshot: &Status,
    viewer: &Participant,
    previews: &mut LivePreviews,
    watch: &mut stuck::Watch,
) -> Result<(), Error> {
    for channel in messages::readable_channels(snapshot.channels.iter(), viewer) {
        if let Some(worker) =
            inbox_recipient(&snapshot.participants, &channel.name).filter(|worker| !worker.revoked)
        {
            for pos in channel.due_positions(&worker.id) {
                let address = channel.address(pos);
                if let Some(preview) = previews.read_with_body(&address, RecordBody::Omit)? {
                    watch.stored(address, &preview.sender, &preview.kind, preview.at_ms());
                }
            }
        }
    }
    Ok(())
}
