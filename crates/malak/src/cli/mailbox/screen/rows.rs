use super::super::{
    activity::Delivery,
    model::{
        Address, Event, EventOrder, Lane, MessageOrder, Model, Preview, PreviewState, QuestionKind,
    },
    multiboard::BoardEntry,
    orders, questions, render, text,
    trace::LiveTrace,
    view::{RowKey, Tab},
    waiting,
};
use crate::cli::{
    output::{self, clock, display},
    presentation,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::{cmp::Reverse, collections::HashSet};

pub(super) fn retain_trace_participants(
    lanes: &mut Vec<Lane<'_>>,
    model: &Model,
    trace: &LiveTrace,
) {
    let mut members = HashSet::new();
    for (address, message) in &trace.messages {
        members.insert(message.sender.as_str());
        if let Some(recipient) = model.message_recipient(&address.channel) {
            members.insert(recipient.id.as_str());
        }
    }
    lanes.retain(|lane| members.contains(lane.participant.id.as_str()));
}

pub(super) fn messages<'a>(
    model: &'a Model,
    tab: Tab,
    filter: Option<&str>,
    trace: Option<&'a LiveTrace>,
) -> Vec<Row<'a>> {
    let mut rows: Vec<_> = match tab {
        Tab::Activity => model
            .activity()
            .map(|(key, event)| {
                let key = RowKey::activity(key, event);
                let at = event.at_ms.unwrap_or_default();
                let sent_at = match &key {
                    RowKey::Message(address) => {
                        model.resolved_preview(address).flatten().map(|p| p.sent_at)
                    }
                    _ => None,
                }
                .unwrap_or([at / 1000, at % 1000 * 1_000_000]);
                Row {
                    key,
                    model,
                    board: None,
                    tagged: false,
                    sent_at,
                    source: Source::Event(event),
                    context: Context::Activity,
                }
            })
            .collect(),
        Tab::Questions => model
            .open_questions()
            .into_iter()
            .flatten()
            .map(|question| {
                let address = Address {
                    channel: "questions".into(),
                    creation: question.creation.clone(),
                    position: question.position,
                };
                Row {
                    source: Source::Preview(
                        question.sender.as_str(),
                        question.at_ms,
                        model.preview(&address),
                    ),
                    key: RowKey::Message(address),
                    model,
                    board: None,
                    tagged: false,
                    sent_at: question.sent_at,
                    context: Context::Question(question.kind),
                }
            })
            .collect(),
        Tab::Orders => orders::live(model)
            .into_iter()
            .flat_map(|order| {
                let position = order.address.position;
                let posted = Row::message(model, order.address, order.message, Context::Order);
                std::iter::once(posted).chain(order.decisions.into_iter().map(
                    move |(address, message)| {
                        Row::message(model, address, message, Context::Decision(position))
                    },
                ))
            })
            .collect(),
        Tab::Waiting => waiting::live(model)
            .map(|(address, message)| Row::message(model, address, message, Context::Waiting))
            .collect(),
        Tab::Trace => trace
            .into_iter()
            .flat_map(|trace| &trace.messages)
            .map(|(address, message)| Row::message(model, address.clone(), message, Context::Trace))
            .collect(),
    };
    if let Some(id) = filter {
        rows.retain(|row| {
            row.address().is_some_and(|address| {
                model.message_involves(address, row.sender().unwrap_or_default(), id)
            })
        });
    }
    rows
}

pub(super) fn sort(rows: &mut [Row<'_>]) {
    rows.sort_unstable_by(|a, b| {
        (
            a.group(),
            Reverse(a.order()),
            a.board.map(|board| &board.id),
        )
            .cmp(&(
                b.group(),
                Reverse(b.order()),
                b.board.map(|board| &board.id),
            ))
    });
}

#[derive(Clone, Copy)]
enum Context {
    Activity,
    Question(QuestionKind),
    Order,
    Decision(u64),
    Waiting,
    Trace,
}

enum Source<'a> {
    Event(&'a Event),
    Preview(&'a str, u64, PreviewState<'a>),
}

pub(super) struct Row<'a> {
    pub key: RowKey,
    model: &'a Model,
    pub board: Option<&'a BoardEntry>,
    tagged: bool,
    sent_at: [u64; 2],
    source: Source<'a>,
    context: Context,
}

pub(super) struct Presentation {
    pub active: bool,
    pub open: bool,
    pub letter: Option<char>,
    pub color: bool,
}

impl<'a> Row<'a> {
    fn message(model: &'a Model, address: Address, preview: &'a Preview, context: Context) -> Self {
        Self {
            key: RowKey::Message(address),
            model,
            board: None,
            tagged: false,
            sent_at: preview.sent_at,
            source: Source::Preview(
                &preview.sender,
                preview.at_ms(),
                PreviewState::Ready(preview),
            ),
            context,
        }
    }

    pub fn attach_board(&mut self, board: &'a BoardEntry, tagged: bool) {
        self.key = self.key.clone().with_board(&board.id);
        self.board = Some(board);
        self.tagged = tagged;
    }

    fn local_key(&self) -> &RowKey {
        match &self.key {
            RowKey::OnBoard(_, key) => key.as_ref(),
            key => key,
        }
    }

    pub fn address(&self) -> Option<&Address> {
        match self.local_key() {
            RowKey::Message(address) => Some(address),
            _ => None,
        }
    }

    fn group(&self) -> bool {
        matches!(self.context, Context::Question(QuestionKind::Agent))
    }

    fn order(&self) -> MessageOrder<'_> {
        match self.local_key() {
            RowKey::Message(address) => address.message_order(self.sent_at),
            RowKey::Event((_, EventOrder::Live(sequence))) => (self.sent_at, "", *sequence),
            _ => (self.sent_at, "", 0),
        }
    }

    fn sender(&self) -> Option<&str> {
        match self.source {
            Source::Event(event) => event.id.as_deref(),
            Source::Preview(sender, _, _) => Some(sender),
        }
    }

    fn preview(&self) -> PreviewState<'a> {
        match self.source {
            Source::Preview(_, _, preview) => preview,
            Source::Event(_) => self.address().map_or(PreviewState::Unavailable, |address| {
                self.model.preview(address)
            }),
        }
    }

    pub fn trace(&self) -> Option<&str> {
        match self.preview() {
            PreviewState::Ready(preview) => preview.trace.as_deref(),
            _ => None,
        }
    }

    pub fn sender_line(&self, columns: usize) -> Option<String> {
        let actor = match self.source {
            Source::Event(event) if event.event == "owner" => "Owner",
            _ => self.model.name(self.sender()?),
        };
        let trace = match self.source {
            Source::Event(event) => event.sender_trace.as_ref(),
            Source::Preview(..) => None,
        };
        trace
            .or_else(|| self.preview().sender_trace())
            .map(|trace| trace.line(actor, columns))
    }

    pub fn copy_text(&self) -> Option<String> {
        let address = self.address()?;
        let address = format!(
            "{} {} {}",
            address.channel, address.creation, address.position
        );
        Some(match self.board_arg() {
            Some(board) => format!("--board {board} --reply-to {address}"),
            None => address,
        })
    }

    pub fn copy_trace(&self) -> Option<String> {
        let board = self.board_arg()?;
        Some(format!(
            "malak --board {board} mailbox --trace {}",
            self.trace()?
        ))
    }

    fn board_arg(&self) -> Option<String> {
        let name = &self.board.filter(|_| self.tagged)?.name;
        if name
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_.-".contains(&ch))
        {
            Some(name.clone())
        } else {
            Some(format!("'{}'", name.replace('\'', "'\\''")))
        }
    }

    pub fn header(&self, now: u64, width: usize, style: Presentation) -> Line<'static> {
        let prefix = marker(style.active, style.letter);
        let toggle = if style.open { " ▾" } else { " ▸" };
        let inner = width.saturating_sub(text::width(&prefix) + 2);
        let (status, hue) = self.status(now);
        let status = shorten(&status, inner);
        let status_width = text::width(&status);
        let heading = self.title(now, inner.saturating_sub(status_width + 2));
        let fill = inner.saturating_sub(text::width(&heading) + status_width);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let bar = if style.active {
            bold.add_modifier(Modifier::REVERSED)
        } else {
            bold
        };
        let status_style = if style.color { bold.fg(hue) } else { bold };

        Line::from(vec![
            Span::styled(format!("{prefix}{heading}{:fill$}", ""), bar),
            Span::styled(status, status_style),
            Span::styled(toggle, bar),
        ])
    }

    fn status(&self, now: u64) -> (String, Color) {
        if let Some(address) = self.address() {
            return match self.model.delivery(address) {
                Delivery::Waiting => ("waiting".into(), Color::Yellow),
                Delivery::Read(at) => (format!("read {}", short_age(at, now)), Color::Yellow),
                Delivery::Acknowledged(None) => ("acknowledged".into(), Color::Green),
                Delivery::Acknowledged(Some(at)) => {
                    (format!("acknowledged {}", short_age(at, now)), Color::Green)
                }
                Delivery::Revoked => ("revoked".into(), Color::Red),
            };
        }
        let Source::Event(event) = self.source else {
            unreachable!("message requires an address")
        };
        if event.operation.as_deref() == Some("REVOKE") && event.outcome.as_deref() == Some("done")
        {
            ("revoked".into(), Color::Red)
        } else {
            (
                display(event.outcome.as_deref().unwrap_or(&event.event)),
                Color::Yellow,
            )
        }
    }

    fn title(&self, now: u64, width: usize) -> String {
        let model = self.model;
        let Some(address) = self.address() else {
            let Source::Event(event) = self.source else {
                unreachable!("message requires an address")
            };
            let event = render::activity_event(model, event);
            return self.tag().map_or_else(
                || shorten(&event, width),
                |tag| {
                    format!(
                        "{}{tag}",
                        shorten(&event, width.saturating_sub(text::width(&tag)))
                    )
                },
            );
        };
        let (at, kind) = match self.source {
            Source::Event(event) => (event.at_ms, event.kind.as_deref().unwrap_or("message")),
            Source::Preview(_, at, PreviewState::Ready(preview)) => {
                (Some(at), preview.kind.as_str())
            }
            Source::Preview(_, at, _) => (Some(at), "message"),
        };
        let label = match self.context {
            Context::Question(QuestionKind::Owner) => "owner question",
            Context::Question(QuestionKind::Agent) => "question",
            Context::Order => "order",
            Context::Decision(_) => "decision",
            _ => kind,
        };
        let mut label = format!("{} #{}", display(label), address.position);
        if let Some(tag) = self.tag() {
            label.push_str(&tag);
        }
        if matches!(self.context, Context::Question(_))
            && let Some(at) = at
        {
            label = format!("{label} · {}", short_age(at, now));
        }
        let time = clock(at);
        let sender = self
            .sender()
            .map_or_else(|| "board".into(), |id| render::participant(model.name(id)));
        let recipient = match self.context {
            Context::Question(_) | Context::Order => String::new(),
            _ => model.message_recipient(&address.channel).map_or_else(
                || display(&address.channel),
                |person| render::participant(&person.name),
            ),
        };
        heading(&time, &sender, &recipient, &label, width)
    }

    fn tag(&self) -> Option<String> {
        let board = self.board.filter(|_| self.tagged)?;
        Some(format!(
            " · [BOARD: {}]",
            display(&board.name).to_uppercase()
        ))
    }

    pub fn body(&self, width: usize, limit: Option<usize>, now: u64) -> Vec<String> {
        let model = self.model;
        let Some(address) = self.address() else {
            let Source::Event(event) = self.source else {
                unreachable!("message requires an address")
            };
            return limit.map_or_else(
                || text::wrapped(&render::activity_event(model, event), width),
                |_| Vec::new(),
            );
        };
        let preview = self.preview();
        let hint;
        let body = match preview {
            PreviewState::Ready(preview) => {
                if let Some(body) = &preview.body {
                    &body.text
                } else if let Some(bytes) = preview.cut_bytes {
                    hint = render::fetch_hint(address, bytes);
                    &hint
                } else {
                    preview
                        .text
                        .as_deref()
                        .unwrap_or("Body preview unavailable")
                }
            }
            PreviewState::Pending => "Loading body preview…",
            PreviewState::Unavailable => "Body preview unavailable",
            PreviewState::AnswersInInbox => questions::INBOX_HINT,
        };
        let metadata = match self.context {
            Context::Order => {
                if let PreviewState::Ready(preview) = preview {
                    let end = orders::order_end(preview);
                    let state = if now < end { "in force" } else { "ended" };
                    Some(format!(
                        "{state} · until {}",
                        output::timestamp(Some(end), now)
                    ))
                } else {
                    None
                }
            }
            Context::Decision(order) => Some(format!("Under order #{order}")),
            _ => None,
        };
        if let Some(limit) = limit {
            let scoped = match self.context {
                Context::Order => orders::scope_lines(body).collect::<Vec<_>>().join("\n"),
                Context::Decision(_) => orders::decision_lines(body).collect::<Vec<_>>().join("\n"),
                _ => return text::activity_preview(body, width, limit),
            };
            let excerpt = format!("{}\n{scoped}", metadata.as_deref().unwrap_or_default());
            return text::activity_preview(&excerpt, width, limit);
        }

        let mut lines = Vec::new();
        for line in metadata.iter().map(String::as_str).chain(body.lines()) {
            lines.extend(text::activity_wrap(&display(line), width).map(str::to_owned));
        }
        let heads = if matches!(self.context, Context::Trace) {
            match preview {
                PreviewState::Ready(p) => p.reply_to.as_ref().map_or_else(String::new, |reply| {
                    format!(
                        "Reply to #{} {} (creation {})",
                        reply.position,
                        display(&reply.channel),
                        display(&reply.creation)
                    )
                }),
                _ => String::new(),
            }
        } else {
            render::header_details(model, address, preview, true)
        };
        if !heads.is_empty() {
            lines.extend(text::activity_wrap(heads.trim(), width).map(str::to_owned));
        }
        if let PreviewState::Ready(preview) = preview
            && let Some(quote) = render::quote_line(model, preview)
        {
            lines.extend(text::activity_wrap(quote.trim_start(), width).map(str::to_owned));
        }
        lines
    }

    pub fn reply_label(&self, width: usize, full: bool) -> Vec<String> {
        let Some(address) = self.address() else {
            return Vec::new();
        };
        if full {
            let inbox = self
                .sender()
                .and_then(|id| presentation::reply_inbox(&address.channel, id))
                .map(|inbox| format!("Reply inbox  {inbox}"));
            let address = render::reply_address(address, width);
            return address
                .lines()
                .chain(inbox.as_deref())
                .chain(self.trace())
                .flat_map(|line| text::wrapped(line, width))
                .collect();
        }
        let suffix = format!(
            " {} {}",
            shorten(&display(&address.creation), 9),
            address.position
        );
        let channel = shorten(
            &display(&address.channel),
            width.saturating_sub(6 + text::width(&suffix)),
        );
        super::pack(
            std::iter::once(format!("reply {channel}{suffix}"))
                .chain(self.trace().map(|id| format!("trace {}", shorten(id, 9)))),
            width,
            " · ",
        )
    }
}

pub(super) fn participant_line(
    lane: &Lane<'_>,
    model: &Model,
    now: u64,
    width: usize,
    active: bool,
    letter: Option<char>,
    show_last: bool,
) -> String {
    let prefix = marker(active, letter);
    let name = render::participant(&lane.participant.name);
    let mut suffix = model
        .name_number(lane.participant)
        .map_or_else(String::new, |n| format!(" ({n})"));
    if let Some(state) = lane.participant.launcher_state {
        suffix.push(' ');
        suffix.push_str(state);
    }
    let count = if lane.participant.revoked {
        "–".into()
    } else {
        lane.waiting.to_string()
    };
    let age = show_last.then(|| participant_age(lane.active_ms, now));
    format!(
        "{prefix}{}",
        participant_columns(
            &name,
            &suffix,
            age.as_deref(),
            &count,
            width.saturating_sub(text::width(&prefix))
        )
    )
}

pub(super) fn archive_line(
    count: usize,
    open: bool,
    width: usize,
    active: bool,
    letter: Option<char>,
) -> String {
    let prefix = marker(active, letter);
    let arrow = if open { '▾' } else { '▸' };
    shorten(&format!("{prefix}{arrow} Archived ({count})"), width)
}

pub(super) const PARTICIPANTS: &str = "PARTICIPANTS";
pub(super) const PARTICIPANT_INDENT: &str = "  ";
const LAST_WIDTH: usize = 5;
const WAITING_WIDTH: usize = 7;
const COLUMN_GAP: usize = 2;

pub(super) fn participant_last_fits(width: usize) -> bool {
    let header_width = PARTICIPANT_INDENT.len() + PARTICIPANTS.len();
    width >= header_width + LAST_WIDTH + WAITING_WIDTH + 2 * COLUMN_GAP
}

pub(super) fn participant_age(at: Option<u64>, now: u64) -> String {
    at.map_or_else(|| "–".into(), |at| short_age(at, now))
}

pub(super) fn participant_columns(
    name: &str,
    suffix: &str,
    age: Option<&str>,
    count: &str,
    width: usize,
) -> String {
    let tail = match age {
        Some(age) => format!("{age:>LAST_WIDTH$}  {count:>WAITING_WIDTH$}"),
        None => format!("{count:>WAITING_WIDTH$}"),
    };
    let room = width.saturating_sub(text::width(&tail) + COLUMN_GAP);
    let suffix = if text::width(suffix) < room {
        suffix
    } else {
        ""
    };
    let name = shorten(name, room.saturating_sub(text::width(suffix)));
    let pad = width.saturating_sub(text::width(&name) + text::width(suffix) + text::width(&tail));
    format!("{name}{suffix}{:pad$}{tail}", "")
}

pub(super) fn marker(active: bool, letter: Option<char>) -> String {
    let marker = if active { '▶' } else { ' ' };
    letter.map_or_else(
        || format!("{marker} "),
        |letter| format!("{letter} {marker} "),
    )
}

fn heading(time: &str, sender: &str, recipient: &str, label: &str, width: usize) -> String {
    let room = width.saturating_sub(text::width(label) + 3);
    if room < 2 {
        return shorten(label, width);
    }
    let route = if recipient.is_empty() { 0 } else { 3 };
    let names_width = text::width(sender) + text::width(recipient) + route;
    let timed = |time: &str| room >= text::width(time) + 2 + names_width;
    let time = if timed(time) {
        format!("{time}  ")
    } else if time.len() >= 5 && timed(&time[..5]) {
        format!("{}  ", &time[..5])
    } else {
        String::new()
    };
    let room = room.saturating_sub(text::width(&time) + route);
    if recipient.is_empty() || room < 4 {
        return format!("{time}{} · {label}", shorten(sender, room + route));
    }
    let sender_room = text::width(sender).min(room - text::width(recipient).min(room / 2));
    format!(
        "{time}{} → {} · {label}",
        shorten(sender, sender_room),
        shorten(recipient, room - sender_room)
    )
}

pub(super) fn shorten(name: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text::width(name) <= width {
        return name.to_owned();
    }
    let end = text::fitting_bytes(name, width - 1);
    format!("{}…", &name[..end])
}

fn short_age(at: u64, now: u64) -> String {
    let minutes = now.saturating_sub(at) / 60_000;
    match minutes {
        0 => "now".into(),
        1..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h", minutes / 60),
        _ => format!("{}d", minutes / 1440),
    }
}
