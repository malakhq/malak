//! Drive terminal input and draw mailbox pages from live updates.

use super::{
    LiveBoard, Update,
    messages::PreviewRequest,
    model::{Address, Model, Preview},
    multiboard::{BoardId, Boards},
    refresh, screen, summary,
    terminal::RestoreTerminal,
    trace, view, who,
};
use crate::cli::{config::io_error, output::now_ms};
use malak::Error;
use ratatui::crossterm::{
    ExecutableCommand,
    event::{
        self, EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    terminal::DisableLineWrap,
};
use std::io::{self, Write};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::Duration;

pub(super) fn run(
    model: &mut Model,
    rx: Receiver<Update>,
    previews: Option<SyncSender<PreviewRequest>>,
    board: Option<(&str, LiveBoard)>,
    initial_trace: Option<String>,
) -> Result<(), Error> {
    let roster = board.is_none();
    let (mut terminal, _restore) = setup(!roster)?;
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut scroll = 0usize;
    let mut view = view::View::default();
    if !roster {
        view.filter = model.take_selection();
    }
    let mut trace = match initial_trace {
        Some(id) => {
            view.tab = view::Tab::Trace;
            Some(trace::LiveTrace::open(
                &board.as_ref().unwrap().1,
                id,
                false,
            )?)
        }
        None => None,
    };
    let mut updates = BoardUpdates::new(rx, previews);
    model.previews_enabled = updates.previews.is_some();
    if !roster {
        model.enable_activity();
    }
    let mut last_draw = None;
    loop {
        let Some(batch) = updates.drain(
            model,
            &mut view.startup,
            &mut view.stopped_at,
            trace.as_mut(),
            roster,
        )?
        else {
            return Ok(());
        };
        updates.request_preview(model, &view.startup, view.visible.preview.as_ref());
        let mut redraw = batch.redraw;
        redraw |= trace.as_mut().is_some_and(trace::LiveTrace::refresh);
        let now = view.stopped_at.unwrap_or_else(now_ms);
        let second = now / 1000;
        if redraw || last_draw != Some(second) {
            updates.update_summary(model);
            scroll = scroll.min(model.people().len().saturating_sub(1));
            terminal
                .draw(|frame| {
                    if let Some((name, _)) = &board {
                        screen::draw(frame, model, name, now, &mut view, color, trace.as_ref());
                    } else {
                        who::screen(frame, model, scroll);
                    }
                })
                .map_err(io_error)?;
            last_draw = Some(second);
        }
        // Poll for keys between batches, but drain queued history without
        // waiting for a key between batches.
        let wait = if batch.more_updates && updates.events.is_some() {
            Duration::ZERO
        } else {
            Duration::from_millis(75)
        };
        match input(wait, !roster)? {
            Input::Idle => {}
            Input::Quit => return Ok(()),
            Input::Resize => last_draw = None,
            Input::Key(code) => {
                if roster {
                    match code {
                        KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                        KeyCode::Up => scroll = scroll.saturating_add(1),
                        KeyCode::Down => scroll = scroll.saturating_sub(1),
                        KeyCode::PageUp => scroll = scroll.saturating_add(8),
                        KeyCode::PageDown => scroll = scroll.saturating_sub(8),
                        KeyCode::Home => scroll = 0,
                        _ => continue,
                    }
                } else {
                    let action = view.key(code);
                    match action {
                        view::Action::Quit => return Ok(()),
                        view::Action::None => continue,
                        view::Action::Redraw => {}
                        view::Action::ReturnFromTrace => trace = None,
                        view::Action::Trace(id) => {
                            let (_, board) = board.as_ref().expect("Activity has a board");
                            trace = Some(trace::LiveTrace::open(
                                board,
                                id,
                                view.stopped_at.is_some(),
                            )?);
                        }
                        view::Action::Copy(ref text) | view::Action::CopyTrace(ref text) => {
                            copy(&mut view, &action, text)?;
                        }
                    }
                }
                last_draw = None;
            }
        }
    }
}

pub(super) fn run_boards() -> Result<(), Error> {
    let mut boards = Boards::new()?;
    let (mut terminal, _restore) = setup(true)?;
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut view = view::View::for_boards();
    let mut trace: Option<(BoardId, trace::LiveTrace)> = None;
    let mut last_draw = None;
    loop {
        let (mut redraw, pending) = boards.drain(
            trace.as_mut().map(|(id, trace)| (&*id, trace)),
            view.visible
                .board
                .as_ref()
                .zip(view.visible.preview.as_ref()),
        );
        if trace
            .as_ref()
            .is_some_and(|(id, _)| boards.get(id).and_then(|board| board.running()).is_none())
        {
            trace = None;
            redraw = true;
        }
        redraw |= trace.as_mut().is_some_and(|(_, trace)| trace.refresh());
        let now = now_ms();
        let second = now / 1000;
        if redraw || last_draw != Some(second) {
            let entries = boards.ordered();
            terminal
                .draw(|frame| {
                    screen::draw_boards(
                        frame,
                        &entries,
                        now,
                        &mut view,
                        color,
                        trace.as_ref().map(|(id, trace)| (id, trace)),
                    );
                })
                .map_err(io_error)?;
            last_draw = Some(second);
        }
        let wait = if pending {
            Duration::ZERO
        } else {
            Duration::from_millis(75)
        };
        let code = match input(wait, true)? {
            Input::Idle => continue,
            Input::Quit => return Ok(()),
            Input::Resize => {
                last_draw = None;
                continue;
            }
            Input::Key(code) => code,
        };
        let can_expand = view
            .boards
            .as_ref()
            .unwrap()
            .selected
            .as_ref()
            .and_then(|id| boards.get(id))
            .and_then(|board| board.running())
            .is_some();
        let action = view
            .board_key(code, can_expand)
            .unwrap_or_else(|| view.key(code));
        match action {
            view::Action::Quit => return Ok(()),
            view::Action::None => continue,
            view::Action::Redraw => {}
            view::Action::ReturnFromTrace => trace = None,
            view::Action::Trace(id) => {
                if let Some(board_id) = &view.visible.board
                    && let Some(running) = boards.get(board_id).and_then(|board| board.running())
                {
                    trace = Some((
                        board_id.clone(),
                        trace::LiveTrace::open(&running.board, id, false)?,
                    ));
                }
            }
            view::Action::Copy(ref text) | view::Action::CopyTrace(ref text) => {
                copy(&mut view, &action, text)?;
            }
        }
        last_draw = None;
    }
}

fn setup(live: bool) -> Result<(ratatui::DefaultTerminal, RestoreTerminal), Error> {
    let mut restore = RestoreTerminal { live_view: false };
    let terminal = ratatui::try_init().map_err(io_error)?;
    if live {
        // Disable wrapping before drawing, including during terminal resize.
        restore.live_view = true;
        io::stdout()
            .execute(DisableLineWrap)
            .and_then(|out| out.execute(EnableMouseCapture))
            .map_err(io_error)?;
    }
    Ok((terminal, restore))
}

enum Input {
    Idle,
    Quit,
    Resize,
    Key(KeyCode),
}

fn input(wait: Duration, mouse: bool) -> Result<Input, Error> {
    if !event::poll(wait).map_err(io_error)? {
        return Ok(Input::Idle);
    }
    Ok(match event::read().map_err(io_error)? {
        TerminalEvent::Key(key) if key.kind != KeyEventKind::Release => {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                Input::Quit
            } else {
                Input::Key(key.code)
            }
        }
        TerminalEvent::Mouse(event) if mouse => match event.kind {
            MouseEventKind::ScrollUp => Input::Key(KeyCode::Up),
            MouseEventKind::ScrollDown => Input::Key(KeyCode::Down),
            _ => Input::Idle,
        },
        TerminalEvent::Resize(_, _) => Input::Resize,
        _ => Input::Idle,
    })
}

fn copy(view: &mut view::View, action: &view::Action, text: &str) -> Result<(), Error> {
    let mut out = io::stdout().lock();
    view::copy_text(&mut out, text).map_err(io_error)?;
    out.flush().map_err(io_error)?;
    view.copied = matches!(action, view::Action::Copy(_));
    view.trace_copied = matches!(action, view::Action::CopyTrace(_));
    Ok(())
}

pub(super) struct BoardUpdates {
    events: Option<Receiver<Update>>,
    previews: Option<SyncSender<PreviewRequest>>,
    preview_pending: bool,
    review_viewer: Option<String>,
}

pub(super) struct UpdateBatch {
    pub redraw: bool,
    pub more_updates: bool,
}

impl BoardUpdates {
    pub fn new(events: Receiver<Update>, previews: Option<SyncSender<PreviewRequest>>) -> Self {
        Self {
            events: Some(events),
            previews,
            preview_pending: false,
            review_viewer: None,
        }
    }

    pub fn update_summary(&self, model: &mut Model) {
        if let Some(viewer) = &self.review_viewer {
            summary::update(model, viewer);
        }
    }

    pub fn drain(
        &mut self,
        model: &mut Model,
        startup: &mut view::Startup,
        stopped_at: &mut Option<u64>,
        mut trace: Option<&mut trace::LiveTrace>,
        roster: bool,
    ) -> Result<Option<UpdateBatch>, Error> {
        let mut redraw = false;
        let mut more_updates = true;
        for _ in 0..128 {
            let Some(rx) = &self.events else {
                more_updates = false;
                break;
            };
            match rx.try_recv() {
                Ok(Update::Event(event)) => {
                    if roster {
                        model.observe(&event);
                    } else {
                        let stop = event.at_ms.filter(|_| event.is_stop());
                        if let Some(trace) = trace.as_ref()
                            && let Some(address) = event.address()
                        {
                            trace.stored(address);
                        }
                        model.apply(event);
                        if let Some(at) = stop {
                            *stopped_at = Some(at);
                            if let Some(trace) = trace.as_mut() {
                                trace.stop();
                            }
                            self.events = None;
                            self.previews = None;
                            model.previews_enabled = false;
                            redraw = true;
                            break;
                        }
                    }
                }
                Ok(Update::Status(status)) => refresh(model, status)?,
                Ok(Update::Preview(address, preview)) => {
                    match preview {
                        Ok(preview) => model.fetch_result(address, preview),
                        Err(_) => model.failed_preview(address),
                    }
                    self.preview_pending = false;
                }
                Ok(Update::PreviewsUnavailable) => {
                    model.previews_enabled = false;
                    self.previews = None;
                }
                Ok(Update::Backfill(address, preview)) => fold_retained(model, address, preview),
                Ok(Update::Recent(viewer, recent)) => {
                    self.review_viewer = viewer;
                    for (address, preview) in recent {
                        fold_retained(model, address, preview);
                    }
                    startup.history_ready = true;
                    redraw = true;
                    break;
                }
                Ok(Update::BackfillDone) => startup.history_complete = true,
                Ok(Update::History(history)) => model.set_history(history),
                Ok(Update::End(end)) => return end.map(|()| None),
                Err(TryRecvError::Empty) => {
                    more_updates = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => return Ok(None),
            }
            redraw = true;
        }
        Ok(Some(UpdateBatch {
            redraw,
            more_updates,
        }))
    }

    pub fn request_preview(
        &mut self,
        model: &mut Model,
        startup: &view::Startup,
        selected: Option<&Address>,
    ) {
        if startup.history_ready
            && !self.preview_pending
            && let Some(previews) = &self.previews
            && let Some(viewer) = &self.review_viewer
        {
            self.preview_pending =
                request_preview(model, previews, selected, viewer, startup.history_complete);
        }
    }
}

fn fold_retained(model: &mut Model, address: Address, preview: Option<Preview>) {
    if let Some(preview) = preview {
        model.backfill(address, preview, true);
    } else {
        model.fetch_result(address, None);
    }
}

fn request_preview(
    model: &mut Model,
    sender: &SyncSender<PreviewRequest>,
    selected: Option<&Address>,
    viewer: &str,
    history_complete: bool,
) -> bool {
    selected
        .filter(|address| !model.requested(address))
        .cloned()
        .or_else(|| model.next_preview())
        .or_else(|| {
            model
                .due_inboxes()
                .into_iter()
                .filter_map(|inbox| inbox.addresses().next())
                .find(|address| !model.requested(address))
        })
        .or_else(|| {
            if history_complete {
                summary::missing(model, viewer)
            } else {
                None
            }
        })
        .zip(model.participant(viewer))
        .is_some_and(|(address, viewer)| {
            sender
                .try_send(PreviewRequest {
                    address,
                    viewer: viewer.clone(),
                })
                .is_ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::mailbox::model::tests::status;
    use std::sync::mpsc;

    #[test]
    fn a_failed_preview_is_requested_only_once() {
        let mut model = Model::new(status(), None);
        model.previews_enabled = true;
        let address = model.due_inboxes()[0].addresses().next().unwrap();
        let (events, receive) = mpsc::channel();
        let (previews, requests) = mpsc::sync_channel(1);
        let mut updates = BoardUpdates::new(receive, Some(previews));
        let mut startup = view::Startup::default();
        events
            .send(Update::Recent(Some("lead".into()), vec![]))
            .unwrap();
        updates
            .drain(&mut model, &mut startup, &mut None, None, false)
            .unwrap();

        updates.request_preview(&mut model, &startup, Some(&address));
        assert_eq!(requests.try_recv().unwrap().address, address);
        events
            .send(Update::Preview(
                address.clone(),
                Err(Error::unavailable("participant call rate exceeded")),
            ))
            .unwrap();
        for _ in 0..32 {
            updates
                .drain(&mut model, &mut startup, &mut None, None, false)
                .unwrap();
            updates.request_preview(&mut model, &startup, Some(&address));
        }

        assert!(matches!(
            model.preview(&address),
            super::super::model::PreviewState::Unavailable
        ));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    }
}
