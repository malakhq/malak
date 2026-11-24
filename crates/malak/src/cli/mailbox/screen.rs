use super::{
    model::{Lane, Model},
    multiboard::{BoardEntry, BoardId, State},
    summary, text,
    trace::LiveTrace,
    view::{Pane, RowKey, Tab, View, Visible},
};
use crate::cli::output::display;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

mod boards;
mod frame;
mod messages;
mod rows;
use frame::{draw_footer, draw_help, footer, header, owner_question, pack};
use messages::message_page;
use rows::{
    PARTICIPANT_INDENT, PARTICIPANTS, archive_line, participant_columns, participant_line, shorten,
};

struct Palette {
    focus: Style,
    bar: Style,
    rule: Style,
    question: Style,
}

impl Palette {
    fn new(color: bool) -> Self {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let focus = if color { bold.fg(Color::Cyan) } else { bold };
        Self {
            focus,
            bar: focus.add_modifier(Modifier::REVERSED),
            rule: Style::default().add_modifier(Modifier::DIM),
            question: if color { bold.fg(Color::Yellow) } else { bold },
        }
    }
}

const JUMP_KEYS: &str = "abcdeghijklmnoprstuvwxyz";
const STACK_BELOW: u16 = 100;

pub(super) fn draw(
    frame: &mut Frame<'_>,
    model: &Model,
    board: &str,
    now: u64,
    view: &mut View,
    color: bool,
    trace: Option<&LiveTrace>,
) {
    let trace = trace.filter(|_| view.tab == Tab::Trace);
    let mut participants = model.lanes_by_activity();
    if let Some(trace) = trace {
        rows::retain_trace_participants(&mut participants, model, trace);
    }
    let (people, archive) = participant_rows(&participants, view.archive_open);
    let left = Left::Participants(model, &people);
    let keys: Vec<_> = people.iter().map(ParticipantRow::key).collect();
    view.cursor(Pane::Participants).sync(keys.iter());
    let stopped = view.stopped_at.is_some();
    let partial = stopped && !view.startup.history_complete;
    let workers = model
        .due_inboxes()
        .into_iter()
        .filter(|inbox| inbox.is_worker())
        .count();
    let labels = partial
        .then(|| "history partial".to_owned())
        .into_iter()
        .chain(summary::compact(model, now, workers, stopped))
        .collect();
    let mut messages = rows::messages(model, view.tab, view.filter.as_deref(), trace);
    rows::sort(&mut messages);
    draw_page(
        frame,
        view,
        now,
        color,
        Page {
            left,
            keys,
            messages,
            label: board.to_owned(),
            labels,
            model: Some(model),
            trace,
            archive,
            opening: false,
            partial,
            question: None,
            empty: None,
        },
    );
}

pub(super) fn draw_boards(
    frame: &mut Frame<'_>,
    entries: &[&BoardEntry],
    now: u64,
    view: &mut View,
    color: bool,
    trace: Option<(&BoardId, &LiveTrace)>,
) {
    let trace = trace.filter(|_| view.tab == Tab::Trace);
    let navigation = view.boards.as_mut().unwrap();
    let present = |id: &BoardId| entries.iter().any(|board| board.id == *id);
    navigation.opened.retain(present);
    navigation.archives.retain(present);
    let left = boards::left(entries, view, trace, now);
    let keys: Vec<_> = left.iter().map(boards::Entry::key).collect();
    view.sync_boards(&keys);
    let navigation = view.boards.as_ref().unwrap();
    let selected = entries
        .iter()
        .copied()
        .find(|board| navigation.selected.as_ref() == Some(&board.id));
    let running = selected.and_then(BoardEntry::running);
    let models = entries
        .iter()
        .copied()
        .filter(|board| selected.is_none_or(|current| current.id == board.id));
    view.startup.history_ready = models
        .clone()
        .any(|entry| entry.running().is_some_and(|run| run.startup.history_ready));
    view.startup.history_complete = models.clone().all(|entry| {
        entry
            .running()
            .is_none_or(|run| run.startup.history_complete)
    });
    let label = selected.map_or_else(
        || format!("{} boards", entries.len()),
        |entry| entry.name.clone(),
    );
    let archive =
        running.is_some() && selected.is_some_and(|board| navigation.opened.contains(&board.id));
    let opening = running.is_some();
    let empty = selected
        .and_then(|board| match &board.state {
            State::Issue(issue) => Some(issue.label()),
            State::Running(_) => None,
        })
        .or_else(|| entries.is_empty().then_some("No boards"));
    let active_trace = trace.filter(|(id, _)| selected.is_none_or(|board| board.id == **id));
    let messages = boards::messages(entries, view, active_trace);
    let labels = if empty.is_some() {
        Vec::new()
    } else {
        boards::counts(models, now).labels()
    };
    draw_page(
        frame,
        view,
        now,
        color,
        Page {
            left: Left::Boards(&left),
            keys,
            messages,
            label,
            labels,
            model: running.map(|run| &run.model),
            trace: active_trace.map(|(_, trace)| trace),
            archive,
            opening,
            partial: false,
            question: boards::owner_question(
                entries,
                usize::from(frame.area().width.saturating_sub(4)),
            ),
            empty,
        },
    );
}

enum Left<'a> {
    Participants(&'a Model, &'a [ParticipantRow<'a>]),
    Boards(&'a [boards::Entry<'a>]),
}

impl Left<'_> {
    fn lane(&self, index: usize) -> Option<&Lane<'_>> {
        match self {
            Self::Participants(_, rows) => rows.get(index).and_then(ParticipantRow::lane),
            Self::Boards(rows) => rows.get(index).and_then(boards::Entry::lane),
        }
    }
}

struct Page<'a> {
    left: Left<'a>,
    keys: Vec<RowKey>,
    messages: Vec<rows::Row<'a>>,
    label: String,
    labels: Vec<String>,
    model: Option<&'a Model>,
    trace: Option<&'a LiveTrace>,
    archive: bool,
    opening: bool,
    partial: bool,
    question: Option<String>,
    empty: Option<&'a str>,
}

fn draw_page(frame: &mut Frame<'_>, view: &mut View, now: u64, color: bool, page: Page<'_>) {
    let area = frame.area();
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    if width == 0 || height == 0 {
        view.visible = Visible::default();
        return;
    }
    let palette = Palette::new(color);
    let Page {
        left,
        keys: people_keys,
        messages,
        label,
        labels,
        model,
        trace,
        archive,
        opening,
        partial,
        question,
        empty,
    } = page;
    let board_mode = view.boards.is_some();
    let message_cursor = view.cursor(Pane::Messages);
    let message_keys = messages.iter().map(|row| &row.key);
    message_cursor.sync(message_keys.clone());
    let new_above = message_cursor.new_above(message_keys);

    let selected = messages.get(view.cursor(Pane::Messages).index);
    let mut visible = match view.focus {
        Pane::Participants => {
            let selected = left.lane(view.cursor(Pane::Participants).index);
            Visible {
                open: selected.is_some() || opening,
                copy: selected
                    .filter(|lane| lane.exists)
                    .map(|lane| lane.inbox.clone()),
                ..Visible::default()
            }
        }
        Pane::Messages => Visible {
            open: selected.is_some(),
            copy: selected.and_then(rows::Row::copy_text),
            trace: selected.and_then(rows::Row::trace).map(str::to_owned),
            trace_copy: selected.and_then(rows::Row::copy_trace),
            board: selected
                .and_then(|row| row.board)
                .map(|board| board.id.clone()),
            preview: selected.and_then(rows::Row::address).cloned(),
            new_above,
            ..Visible::default()
        },
    };
    visible.archive = archive;

    let count = match view.focus {
        Pane::Participants => people_keys.len(),
        Pane::Messages => messages.len(),
    };
    let keys = footer(view, count, &visible, width);
    let footer_height = if height > 2 {
        (keys.len() + 1).min(height - 2)
    } else {
        height - 1
    };
    let footer_y = area.bottom() - footer_height as u16;
    let stopped = view.stopped_at.is_some();
    let mut header = header(
        labels,
        &label,
        now,
        stopped,
        width,
        &palette,
        question.as_deref(),
    );
    let budget = usize::from(footer_y - area.y);
    if header.len() > budget {
        let bottom = header.pop();
        header.truncate(budget.saturating_sub(1).max(1));
        if budget >= 2 {
            header.extend(bottom);
        }
    }
    if !board_mode
        && header.len() + 2 < budget
        && let Some(model) = model
    {
        header.extend(owner_question(model, width, &palette));
    }
    let header_height = header.len() as u16;
    let header_area = Rect::new(area.x, area.y, area.width, header_height);
    frame.render_widget(Paragraph::new(header), header_area);
    let top = header_area.bottom();
    let body = Rect::new(area.x, top, area.width, footer_y.saturating_sub(top));
    let panes = Panes::new(body, view.focus, board_mode.then_some(people_keys.len()));
    if let Some(divider) = panes.divider {
        let mut seam = vec![Line::styled("│", palette.rule); usize::from(divider.height)];
        if let Some(top) = seam.first_mut() {
            *top = Line::styled("┬", palette.rule);
        }
        frame.render_widget(Paragraph::new(seam), divider);
    }

    let (people_lines, people) =
        participant_page(&left, view, &people_keys, panes.people, now, &palette);
    frame.render_widget(Paragraph::new(people_lines), panes.people);
    let filter_heading = model.and_then(|model| {
        boards::filter_heading(view, model, now, usize::from(panes.messages.width))
    });
    let message_body = right_heading(
        frame,
        view,
        trace,
        panes.messages,
        &palette,
        filter_heading,
        new_above,
    );
    let page = if messages.is_empty() || message_body.height == 0 {
        let label = empty.map_or_else(
            || empty_label(view, trace, partial).to_owned(),
            |issue| format!("  {}", display(issue)),
        );
        frame.render_widget(
            Paragraph::new(Line::styled(label, palette.rule)),
            message_body,
        );
        Vec::new()
    } else {
        let (lines, jumps) = message_page(view, &messages, message_body, now, color, &palette);
        frame.render_widget(Paragraph::new(lines), message_body);
        jumps
    };
    visible.jumps = match view.focus {
        Pane::Participants => people,
        Pane::Messages => page,
    };
    view.visible = visible;
    draw_footer(
        frame,
        view,
        keys,
        footer_y,
        panes.divider.map(|rect| rect.x),
        &palette,
    );
    if view.help {
        draw_help(frame, body, &palette, board_mode);
    }
}

enum ParticipantRow<'a> {
    Person(&'a Lane<'a>),
    Archive(usize),
}

impl<'a> ParticipantRow<'a> {
    fn key(&self) -> RowKey {
        match self {
            Self::Person(lane) => RowKey::Participant(lane.participant.id.clone()),
            Self::Archive(_) => RowKey::Archive,
        }
    }

    fn lane(&self) -> Option<&Lane<'a>> {
        match self {
            Self::Person(lane) => Some(lane),
            Self::Archive(_) => None,
        }
    }
}

fn participant_rows<'a>(lanes: &'a [Lane<'a>], expanded: bool) -> (Vec<ParticipantRow<'a>>, bool) {
    let mut rows = Vec::new();
    rows.extend(
        lanes
            .iter()
            .filter(|lane| !lane.participant.revoked)
            .map(ParticipantRow::Person),
    );
    let archived = lanes.iter().filter(|lane| lane.participant.revoked);
    let count = lanes.len() - rows.len();
    if count > 0 {
        rows.push(ParticipantRow::Archive(count));
        if expanded {
            rows.extend(archived.map(ParticipantRow::Person));
        }
    }
    (rows, count > 0)
}

struct Panes {
    people: Rect,
    messages: Rect,
    divider: Option<Rect>,
}

impl Panes {
    fn new(body: Rect, focus: Pane, board_rows: Option<usize>) -> Self {
        if body.width >= STACK_BELOW {
            let left = if board_rows.is_some() {
                (body.width / 4).max(34)
            } else {
                body.width / 4
            };
            let people = Rect::new(body.x, body.y, left, body.height);
            let divider = Rect::new(people.right(), body.y, 1, body.height);
            let messages = Rect::new(divider.right(), body.y, body.width - left - 1, body.height);
            Self {
                people,
                messages,
                divider: Some(divider),
            }
        } else {
            let left = match focus {
                Pane::Participants if board_rows.is_some() => body.height.div_ceil(2),
                Pane::Participants => body.height / 2,
                Pane::Messages => board_rows
                    .map_or(5, |rows| u16::try_from(rows + 2).unwrap_or(u16::MAX).max(3))
                    .min(body.height / 2),
            };
            let people = Rect::new(body.x, body.y, body.width, left);
            let messages = Rect::new(body.x, people.bottom(), body.width, body.height - left);
            Self {
                people,
                messages,
                divider: None,
            }
        }
    }
}

fn participant_page(
    left: &Left<'_>,
    view: &mut View,
    keys: &[RowKey],
    area: Rect,
    now: u64,
    palette: &Palette,
) -> (Vec<Line<'static>>, Vec<(char, RowKey)>) {
    let width = usize::from(area.width);
    let room = usize::from(area.height.saturating_sub(2));
    let focused = view.focus == Pane::Participants;
    let show_last = rows::participant_last_fits(width);
    let mut lines = vec![
        Line::styled("─".repeat(width), palette.rule),
        Line::styled(
            match left {
                Left::Participants(..) => format!(
                    "{PARTICIPANT_INDENT}{}",
                    participant_columns(
                        PARTICIPANTS,
                        "",
                        show_last.then_some("last"),
                        "waiting",
                        width.saturating_sub(PARTICIPANT_INDENT.len()),
                    )
                ),
                Left::Boards(_) => boards::heading(width),
            },
            if focused {
                palette.focus
            } else {
                Style::default()
            },
        ),
    ];
    let mut jumps = Vec::new();
    if keys.is_empty() {
        let empty = if view.boards.is_some() {
            "  No boards"
        } else {
            "  No participants"
        };
        lines.push(Line::styled(empty, palette.rule));
    } else if room > 0 {
        let cursor = view.cursor(Pane::Participants);
        let overflow = room >= 2 && keys.len() > room;
        let rows = (room - usize::from(overflow)).min(keys.len());
        cursor.first = cursor
            .first
            .clamp(cursor.index.saturating_sub(rows - 1), cursor.index)
            .min(keys.len() - rows);
        let first = cursor.first;
        let index = cursor.index;
        let mut letters = JUMP_KEYS.chars();
        for (i, key) in keys.iter().enumerate().skip(first).take(rows) {
            let letter = focused.then(|| letters.next()).flatten();
            if let Some(letter) = letter {
                jumps.push((letter, key.clone()));
            }
            let active = focused && i == index;
            let letter = letter.filter(|_| view.jumping);
            let line = match left {
                Left::Participants(model, people) => match &people[i] {
                    ParticipantRow::Person(lane) => {
                        participant_line(lane, model, now, width, active, letter, show_last)
                    }
                    ParticipantRow::Archive(count) => {
                        archive_line(*count, view.archive_open, width, active, letter)
                    }
                },
                Left::Boards(boards) => boards[i].line(view, now, width, i == index, letter),
            };
            lines.push(Line::styled(
                line,
                if active {
                    palette.bar
                } else {
                    Style::default()
                },
            ));
        }
        if overflow {
            let direction = if first == 0 {
                "↓"
            } else if first + rows == keys.len() {
                "↑"
            } else {
                "↑/↓"
            };
            lines.push(Line::styled(
                format!("  … {} more  ({direction})", keys.len() - rows),
                palette.rule,
            ));
        }
    }
    (lines, jumps)
}

fn right_heading(
    frame: &mut Frame<'_>,
    view: &View,
    trace: Option<&LiveTrace>,
    area: Rect,
    palette: &Palette,
    filter_heading: Option<String>,
    new_above: Option<usize>,
) -> Rect {
    let width = usize::from(area.width);
    let mut tabs = Vec::new();
    for tab in Tab::ALL {
        if !tabs.is_empty() {
            tabs.push(Span::raw(" "));
        }
        if tab == view.tab {
            tabs.push(Span::styled(format!("[{}]", tab.title()), palette.bar));
        } else {
            tabs.push(Span::raw(tab.title()));
        }
    }
    let focus = if view.focus == Pane::Messages {
        palette.focus
    } else {
        palette.rule
    };
    let mut lines = Vec::new();
    if area.x > frame.area().x {
        lines.push(Line::styled("─".repeat(width), palette.rule));
    }
    let mut rule = new_above.map_or_else(String::new, |count| format!("── {count} new · g live "));
    if text::width(&rule) > width {
        rule.clear();
    }
    rule.push_str(&"─".repeat(width - text::width(&rule)));
    lines.extend([Line::from(tabs), Line::styled(rule, focus)]);
    if let Some(heading) = filter_heading {
        lines.push(Line::styled(heading, focus));
    }
    if let Some(trace) = trace {
        let count = trace.messages.len();
        let noun = if count == 1 { "message" } else { "messages" };
        let title = format!("Trace {} · {count} {noun}", trace.id);
        lines.extend(
            text::wrapped(&title, width)
                .into_iter()
                .map(|line| Line::styled(line, focus)),
        );
        lines.extend(
            text::wrapped(
                "Board-generated, inherited by replies, or sender-supplied.",
                width,
            )
            .into_iter()
            .map(|line| Line::styled(line, palette.rule)),
        );
        if let Some(error) = &trace.error {
            lines.extend(
                text::wrapped(&display(error), width)
                    .into_iter()
                    .map(Line::raw),
            );
        }
    }
    let used = (lines.len() as u16).min(area.height);
    frame.render_widget(Paragraph::new(lines), area);
    Rect::new(area.x, area.y + used, area.width, area.height - used)
}

fn empty_label(view: &View, trace: Option<&LiveTrace>, partial: bool) -> &'static str {
    if partial {
        return "  History partially loaded";
    }
    if view.tab == Tab::Trace {
        return match trace {
            None => "  Press t on a message to open its trace",
            Some(trace) if trace.complete && view.filter.is_some() => {
                "  No messages for this participant"
            }
            Some(trace) if trace.complete => "  Waiting for a message…",
            Some(trace) if trace.error.is_none() => "  Loading trace…",
            _ => "",
        };
    }
    if !view.ready() || !view.startup.history_complete {
        return match view.tab {
            Tab::Activity => "  Loading activity…",
            Tab::Questions => "  Loading questions…",
            Tab::Orders => "  Loading orders…",
            _ => "  Loading waiting messages…",
        };
    }
    match view.tab {
        Tab::Activity => "  No activity",
        Tab::Questions => "  No open questions",
        Tab::Orders => "  No orders recorded",
        _ => "  No waiting messages",
    }
}

fn detail_line(line: String, palette: &Palette) -> Line<'static> {
    Line::from(vec![Span::styled("  ┆ ", palette.rule), Span::raw(line)])
}

#[cfg(test)]
mod tests;
