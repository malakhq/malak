use super::Palette;
use crate::cli::{
    mailbox::{
        model::{Model, Question},
        text,
        view::{Pane, Tab, View, Visible},
    },
    output::display,
};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

pub(super) fn footer(view: &View, count: usize, visible: &Visible, width: usize) -> Vec<String> {
    let mut keys = Vec::new();
    if view.help {
        keys.push("?/Esc close");
    } else if view.full_body.is_some() {
        keys.extend(["↑/↓ scroll", "⏎/Esc preview"]);
    } else if view.jumping {
        keys.extend(["letter jump", "Esc/f cancel"]);
    } else {
        keys.push("Tab pane");
        if view.boards.is_some() && view.focus == Pane::Participants {
            keys.extend(["⏎/→ open board", "← close"]);
        }
        keys.push("1–5/[ ] view");
        if count > 1 {
            keys.push("↑/↓ move");
        }
        if visible.open && (view.focus == Pane::Messages || view.boards.is_none()) {
            keys.push(match view.focus {
                Pane::Participants => "⏎ filter",
                Pane::Messages if view.boards.is_some() => "⏎ open message",
                Pane::Messages => "⏎ open",
            });
        }
        if count > 0 {
            keys.push("f jump");
        }
        if visible.archive {
            keys.push(if view.archive_open {
                "a hide archive"
            } else {
                "a show archive"
            });
        }
        if view.focus == Pane::Messages && view.tab == Tab::Activity {
            keys.push(if view.activity_open {
                "e collapse all"
            } else {
                "e expand all"
            });
        }
        if view.can_return() {
            keys.push("Esc return");
        } else if view.filter.is_some() {
            keys.push("Esc clear filter");
        }
        keys.push("? help");
    }
    if !view.jumping && !view.help && visible.new_above.is_some() {
        keys.push("g live");
    }
    if !view.jumping && !view.help {
        if visible.copy.is_some() {
            keys.push(if view.copied { "c copied" } else { "c copy" });
        }
        if visible.trace.is_some() {
            if view.tab != Tab::Trace {
                keys.push("t trace");
            }
            keys.push(if view.trace_copied {
                "y copied"
            } else {
                "y copy trace"
            });
        }
    }
    keys.push("q quit");
    let mut lines = pack(keys.into_iter().map(str::to_owned), width, "  ");
    if view.full_body.is_some() {
        lines.push(String::new());
    }
    lines
}

pub(super) fn draw_footer(
    frame: &mut Frame<'_>,
    view: &View,
    mut keys: Vec<String>,
    top: u16,
    divider: Option<u16>,
    palette: &Palette,
) {
    let area = frame.area();
    let height = area.bottom().saturating_sub(top);
    if height == 0 {
        return;
    }
    let width = usize::from(area.width);
    let mut rule = vec!['═'; width];
    if width > 1 {
        rule[0] = '╞';
        rule[width - 1] = '╡';
    }
    if let Some(divider) = divider {
        rule[usize::from(divider - area.x)] = '╧';
    }
    let mut lines = if height == 1 {
        vec![Line::styled("q quit", palette.rule)]
    } else {
        vec![Line::styled(
            rule.into_iter().collect::<String>(),
            palette.focus,
        )]
    };
    if let Some(body) = &view.full_body
        && body.shown > 0
        && let Some(last) = keys.last_mut()
    {
        *last = format!(
            "lines {}–{} of {}",
            body.offset + 1,
            body.offset + body.shown,
            body.total
        );
    }
    lines.extend(
        keys.into_iter()
            .map(|line| Line::styled(line, palette.rule)),
    );
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, top, area.width, height),
    );
}

pub(super) fn draw_help(frame: &mut Frame<'_>, body: Rect, palette: &Palette, boards: bool) {
    if body.height < 3 || body.width < 4 {
        return;
    }
    let width = body.width.min(54);
    let columns = usize::from(width - 2);
    let lines: Vec<_> = [
        "Tab  Switch panes",
        "1–5 or [ ]  Switch message tabs",
        "↑/↓  Move or scroll the full message",
        "g/Home  Newest live · G/End  Oldest",
        if boards {
            "Enter/→  Open board · ← Close board"
        } else {
            "Enter  Filter by participant or open a message"
        },
        "a  Show or hide archived participants",
        "e  Toggle Activity previews",
        "t  Open a message’s trace",
        "c / y  Copy the reply address / trace ID",
        "f  Label rows for a quick jump",
        "Esc  Close, return, or clear the filter",
    ]
    .into_iter()
    .flat_map(|line| text::wrapped(line, columns))
    .map(Line::raw)
    .collect();
    let height = body.height.min(lines.len() as u16 + 2);
    let overlay = Rect::new(
        body.x + (body.width - width) / 2,
        body.y + (body.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(" Help ")
                .border_style(palette.focus),
        ),
        overlay,
    );
}

pub(super) fn header(
    labels: impl IntoIterator<Item = String>,
    board: &str,
    now: u64,
    stopped: bool,
    width: usize,
    palette: &Palette,
    question: Option<&str>,
) -> Vec<Line<'static>> {
    if width < 5 {
        return vec![Line::styled("Malak", palette.focus)];
    }
    let minutes = now / 60_000 % (24 * 60);
    let time = format!("{:02}:{:02} UTC", minutes / 60, minutes % 60);
    let label = format!(
        " Malak · {} ",
        text::first_line(&display(board), width.saturating_sub(20))
    );
    let right = format!(" {time} ─");
    let inside = width - 2;
    let rule = if text::width(&label) + text::width(&right) < inside {
        let n = inside - text::width(&label) - text::width(&right);
        format!("┌─{label}{}{right}┐", "─".repeat(n - 1))
    } else {
        format!("┌{}┐", "─".repeat(inside))
    };
    let stopped = stopped.then(|| format!("board stopped {time}"));
    let summary = pack(stopped.into_iter().chain(labels), inside - 2, "   ");
    let mut lines = vec![Line::styled(rule, palette.focus)];
    let rows = question
        .into_iter()
        .map(|line| (text::excerpt(line, inside - 2, false), palette.question))
        .chain(
            summary
                .into_iter()
                .map(|line| (line, ratatui::style::Style::default())),
        );
    for (row, style) in rows {
        let fill = inside - 2 - text::width(&row);
        lines.push(Line::from(vec![
            Span::styled("│ ", palette.focus),
            Span::styled(format!("{row}{:fill$}", ""), style),
            Span::styled(" │", palette.focus),
        ]));
    }
    lines.push(Line::styled(
        format!("└{}┘", "─".repeat(inside)),
        palette.focus,
    ));
    lines
}

fn owner_questions(model: &Model) -> impl Iterator<Item = &Question> {
    model
        .open_questions()
        .into_iter()
        .flatten()
        .filter(|q| q.for_owner())
}

pub(super) fn owner_question(
    model: &Model,
    width: usize,
    palette: &Palette,
) -> Option<Line<'static>> {
    let mut count = 0;
    let first = owner_questions(model)
        .inspect(|_| count += 1)
        .max_by_key(|question| question.order())?;
    let prefix = format!(" ▶ {count} for you · #{} ", first.position);
    let excerpt = text::first_line(&first.body, width.saturating_sub(text::width(&prefix)));
    Some(Line::styled(format!("{prefix}{excerpt}"), palette.question))
}

pub(super) fn pack(
    labels: impl IntoIterator<Item = String>,
    width: usize,
    gap: &str,
) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for label in labels {
        let label = text::excerpt(&label, width, false);
        if let Some(row) = rows.last_mut()
            && text::width(row) + text::width(gap) + text::width(&label) <= width
        {
            row.push_str(gap);
            row.push_str(&label);
        } else {
            rows.push(label);
        }
    }
    rows
}
