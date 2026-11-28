//! Board rows, aggregate counts and selection-scoped message rows.

use super::{RowKey, View, rows, shorten};
use crate::cli::{
    mailbox::{
        model::{Lane, Model, Question},
        multiboard::{BoardEntry, BoardId, State},
        questions, summary, text,
        trace::LiveTrace,
    },
    output::display,
};

const BOARD_COLUMNS: usize = 35;
const OWNER_COLUMNS: usize = 3;
const CHILD_INDENT: usize = 2;

pub(super) enum Entry<'a> {
    All(Counts),
    Board(&'a BoardEntry, Counts),
    Person(&'a BoardEntry, &'a Model, Lane<'a>),
    Archive(&'a BoardEntry, usize),
}

impl Entry<'_> {
    pub fn key(&self) -> RowKey {
        match self {
            Self::All(_) => RowKey::AllBoards,
            Self::Board(board, _) => RowKey::Board(board.id.clone()),
            Self::Person(board, _, lane) => {
                RowKey::Participant(lane.participant.id.clone()).with_board(&board.id)
            }
            Self::Archive(board, _) => RowKey::Archive.with_board(&board.id),
        }
    }

    pub fn lane(&self) -> Option<&Lane<'_>> {
        match self {
            Self::Person(_, _, lane) => Some(lane),
            _ => None,
        }
    }

    pub fn line(
        &self,
        view: &View,
        now: u64,
        width: usize,
        active: bool,
        letter: Option<char>,
    ) -> String {
        let navigation = view.boards.as_ref().unwrap();
        let marker = rows::marker(active, letter);
        let columns = width.saturating_sub(text::width(&marker));
        match self {
            Self::All(counts) => format!(
                "{marker}{}",
                columns_line("All boards", Some(counts), columns)
            ),
            Self::Board(board, counts) => {
                let opened = navigation.opened.contains(&board.id);
                let arrow = if opened && board.running().is_some() {
                    "▾"
                } else {
                    "▸"
                };
                let prefix = if active || letter.is_some() {
                    marker
                } else {
                    format!("{arrow} ")
                };
                let name = display(&board.name);
                let line = match &board.state {
                    State::Running(running) if running.older => {
                        noted(&name, "● ⚠", "older build, restart", columns)
                    }
                    State::Running(_) => columns_line(&format!("{name} ●"), Some(counts), columns),
                    State::Issue(issue) => {
                        noted(&name, issue.mark(), &display(issue.label()), columns)
                    }
                };
                format!("{prefix}{}", shorten(&line, columns))
            }
            Self::Person(_, model, lane) => {
                let marker_width = text::width(&marker);
                let row_width = board_columns(width.saturating_sub(marker_width)) + marker_width;
                let content_width = row_width.saturating_sub(OWNER_COLUMNS + CHILD_INDENT);
                let line = rows::participant_line(
                    lane,
                    model,
                    now,
                    content_width,
                    active,
                    letter,
                    rows::participant_last_fits(content_width + OWNER_COLUMNS),
                );
                let (prefix, rest) = line.split_at(text::fitting_bytes(&line, marker_width));
                format!("{prefix}{:CHILD_INDENT$}{rest}", "")
            }
            Self::Archive(board, count) => {
                let arrow = if navigation.archives.contains(&board.id) {
                    "▾"
                } else {
                    "▸"
                };
                shorten(&format!("{marker}  {arrow} Archived ({count})"), width)
            }
        }
    }
}

#[derive(Default)]
pub(super) struct Counts {
    lead: u64,
    waiting: u64,
    workers: usize,
    questions: usize,
    owner: usize,
    orders: usize,
    questions_pending: bool,
    pub question_inbox: bool,
    orders_pending: bool,
}

impl Counts {
    fn add(&mut self, model: &Model, now: u64) {
        let lead = model.lead_waiting();
        let relay = model.relay_waiting();
        self.lead = self.lead.saturating_add(lead);
        self.waiting = self.waiting.saturating_add(lead).saturating_add(relay);
        for (_, waiting, _) in model.waiting_inboxes() {
            self.workers += 1;
            self.waiting = self.waiting.saturating_add(waiting);
        }
        if model.question_inbox_viewer().is_some() {
            self.question_inbox = true;
        } else if let Some(questions) = model.open_questions() {
            self.questions += questions.len();
            self.owner += questions.iter().filter(|q| q.for_owner()).count();
        } else {
            self.questions_pending |= model.has_retained(questions::CHANNEL);
        }
        if let Some(orders) = model.orders_in_force(now) {
            self.orders += orders;
        } else {
            self.orders_pending |= model.has_retained("orders");
        }
    }

    pub fn labels(&self) -> Vec<String> {
        let questions = if self.questions_pending {
            "questions loading…".into()
        } else if self.question_inbox && self.questions == 0 {
            summary::inbox_label()
        } else {
            format!("questions {}", self.questions)
        };
        let orders = if self.orders_pending {
            "loading…".into()
        } else {
            self.orders.to_string()
        };
        vec![
            format!("lead {}", self.lead),
            questions,
            format!("orders {orders}"),
            format!("inboxes {} waiting", self.workers),
        ]
    }
}

fn noted(name: &str, mark: &str, reason: &str, width: usize) -> String {
    let reason = reason.replace(
        "file owner or permissions do not match the access policy",
        "permissions",
    );
    let label = format!("{name} {mark} {reason}");
    if text::width(&label) <= width {
        return label;
    }
    // At split-pane widths, keep the cause visible if a diagnostic also
    // contains a path or a redundant owner-door prefix.
    let reason = reason
        .rsplit_once(": ")
        .map_or(reason.as_str(), |(_, cause)| cause);
    let note = format!("{mark} {reason}");
    let name_width = width.saturating_sub(text::width(&note) + 1).max(width / 4);
    format!("{} {}", shorten(name, name_width), note)
}

fn columns_line(name: &str, counts: Option<&Counts>, width: usize) -> String {
    let (waiting, owner) = counts.map_or_else(
        || ("waiting".into(), "⚑".into()),
        |counts| (counts.waiting.to_string(), counts.owner.to_string()),
    );
    let suffix = format!(" {waiting:>7} {owner:>2}");
    let width = board_columns(width);
    let available = width.saturating_sub(text::width(&suffix));
    let name = shorten(name, available);
    let fill = available - text::width(&name);
    format!("{name}{:fill$}{suffix}", "")
}

fn board_columns(width: usize) -> usize {
    // Board and participant counts share this width when the panes stack.
    width.min(BOARD_COLUMNS)
}

pub(super) fn heading(width: usize) -> String {
    format!(
        "  {}",
        columns_line("BOARDS", None, width.saturating_sub(2))
    )
}

pub(super) fn counts<'a>(boards: impl IntoIterator<Item = &'a BoardEntry>, now: u64) -> Counts {
    let mut counts = Counts::default();
    for board in boards {
        if let Some(running) = board.running() {
            counts.add(&running.model, now);
        }
    }
    counts
}

pub(super) fn left<'a>(
    entries: &[&'a BoardEntry],
    view: &View,
    trace: Option<(&BoardId, &LiveTrace)>,
    now: u64,
) -> Vec<Entry<'a>> {
    let navigation = view.boards.as_ref().unwrap();
    let mut rows = Vec::new();
    if entries.len() > 1 {
        rows.push(Entry::All(counts(entries.iter().copied(), now)));
    }
    for &board in entries {
        rows.push(Entry::Board(board, counts([board], now)));
        if let Some(running) = board.running()
            && navigation.opened.contains(&board.id)
        {
            let model = &running.model;
            let mut lanes = model.lanes_by_activity();
            if let Some((_, trace)) = trace.filter(|(id, _)| *id == &board.id) {
                rows::retain_trace_participants(&mut lanes, model, trace);
            }
            let (current, archived): (Vec<_>, Vec<_>) = lanes
                .into_iter()
                .partition(|lane| !lane.participant.revoked);
            rows.extend(
                current
                    .into_iter()
                    .map(|lane| Entry::Person(board, model, lane)),
            );
            rows.push(Entry::Archive(board, archived.len()));
            if navigation.archives.contains(&board.id) {
                rows.extend(
                    archived
                        .into_iter()
                        .map(|lane| Entry::Person(board, model, lane)),
                );
            }
        }
    }
    rows
}

pub(super) fn messages<'a>(
    entries: &[&'a BoardEntry],
    view: &View,
    trace: Option<(&BoardId, &'a LiveTrace)>,
) -> Vec<rows::Row<'a>> {
    let selected = &view.boards.as_ref().unwrap().selected;
    let tagged = selected.is_none() && entries.len() > 1;
    let mut messages = Vec::new();
    for &entry in entries {
        if selected.as_ref().is_some_and(|id| *id != entry.id) {
            continue;
        }
        let Some(running) = entry.running() else {
            continue;
        };
        let trace = trace
            .filter(|(id, _)| *id == &entry.id)
            .map(|(_, trace)| trace);
        for mut row in rows::messages(&running.model, view.tab, view.filter.as_deref(), trace) {
            row.attach_board(entry, tagged);
            messages.push(row);
        }
    }
    rows::sort(&mut messages);
    messages
}

pub(super) fn owner_question(entries: &[&BoardEntry], width: usize) -> Option<String> {
    let mut count = 0;
    let (board, newest) = entries
        .iter()
        .flat_map(|board| {
            board.running().into_iter().flat_map(|running| {
                running
                    .model
                    .open_questions()
                    .into_iter()
                    .flatten()
                    .filter(|question| question.for_owner())
                    .map(|question| (*board, question))
            })
        })
        .inspect(|_| count += 1)
        .max_by_key(|(_, question): &(&BoardEntry, &Question)| question.order())?;
    let noun = if count == 1 { "question" } else { "questions" };
    let prefix = format!(
        "⚑ {count} owner {noun} · newest on {}: ",
        display(&board.name)
    );
    let body = text::first_line(&newest.body, width.saturating_sub(text::width(&prefix) + 2));
    Some(format!("{prefix}\"{body}\""))
}

pub(super) fn filter_heading(view: &View, model: &Model, now: u64, width: usize) -> Option<String> {
    let id = view.filter.as_deref()?;
    let age = rows::participant_age(
        model
            .lanes()
            .iter()
            .find(|lane| lane.participant.id == id)
            .and_then(|lane| lane.active_ms),
        now,
    );
    let suffix = format!(" · last {age}");
    let available = width.saturating_sub(text::width(&suffix));
    let heading = shorten(
        &format!("{} · @{}", view.tab.title(), display(model.name(id))),
        available,
    );
    Some(format!("{heading}{suffix}"))
}
