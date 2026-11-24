use crate::cli::output::{Format, display};
use ratatui::{style::Style, text::Span};
use std::fmt::Write as _;

pub(super) use crate::cli::output::width;

pub(super) fn terminal_width() -> usize {
    Format::stdout().width()
}

pub(super) fn reply_address(
    channel: &str,
    creation: &str,
    position: u64,
    columns: usize,
) -> String {
    let address = format!("{channel} {creation} {position}");
    let label = "Reply address";
    if label.len() + 2 + address.len() <= columns {
        format!("{label}  {address}")
    } else {
        format!("{label}\n{address}")
    }
}

pub(super) fn quote(body: &str) -> String {
    first_line(body, 64)
}

pub(super) fn first_line(body: &str, columns: usize) -> String {
    let mut lines = body.split('\n');
    let line = lines.next().unwrap_or_default().trim_end_matches('\r');
    excerpt(&display(line), columns, lines.next().is_some())
}

/// Fit an excerpt to one row, reserving a cell for the ellipsis when it is cut.
pub(super) fn excerpt(line: &str, columns: usize, continued: bool) -> String {
    if columns == 0 {
        return String::new();
    }
    let line = line.trim_end();
    let cells = width(line);
    if cells <= columns && !continued {
        return line.to_owned();
    }
    let limit = columns - 1;
    let prefix = if cells <= limit {
        line
    } else {
        let end = line
            .char_indices()
            .filter(|(_, ch)| ch.is_whitespace())
            .take_while(|(index, _)| width(&line[..*index]) <= limit)
            .map(|(index, _)| index)
            .last()
            .unwrap_or(0);
        line[..end].trim_end()
    };
    format!("{prefix}…")
}

pub(super) fn preview<'a>(mut lines: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut preview = lines.by_ref().take(3).collect::<Vec<_>>();
    if lines.next().is_some() {
        preview.push("…");
    }
    preview
}

fn item_padding(columns: usize) -> &'static str {
    if columns >= 8 { " " } else { "" }
}

pub(super) fn item_width(columns: usize) -> usize {
    columns.saturating_sub(2 + item_padding(columns).len() * 2)
}

pub(super) fn item<'a>(
    out: &mut String,
    columns: usize,
    title: &'a str,
    lines: impl IntoIterator<Item = &'a str>,
) {
    let padding = item_padding(columns);
    let inner = item_width(columns);
    if inner == 0 {
        for line in std::iter::once(title).chain(lines) {
            for row in wrapped(&display(line), columns.max(1)) {
                writeln!(out, "{row}").unwrap();
            }
        }
        return;
    }
    let rule = "─".repeat(columns - 2);
    writeln!(out, "┌{rule}┐").unwrap();
    let mut rows = |line| {
        for row in wrapped(&display(line), inner) {
            let fill = inner.saturating_sub(width(&row));
            writeln!(out, "│{padding}{row}{:fill$}{padding}│", "").unwrap();
        }
    };
    rows(title);
    for line in lines {
        rows(line);
    }
    writeln!(out, "└{rule}┘").unwrap();
}

/// Wrap a sanitized line at terminal cell boundaries, splitting overlong words.
pub(super) fn wrapped(mut remaining: &str, columns: usize) -> Vec<String> {
    let mut rows = Vec::new();
    loop {
        let end = fitting_bytes(remaining, columns);
        if end == remaining.len() {
            rows.push(remaining.to_owned());
            return rows;
        }
        if end == 0 {
            // A double-width grapheme has no representation in one cell.
            let span = Span::raw(remaining);
            let first = span.styled_graphemes(Style::default()).next().unwrap();
            rows.push("�".to_owned());
            remaining = &remaining[first.symbol.len()..];
        } else {
            let split = remaining[..end]
                .rfind(' ')
                .filter(|&space| !remaining[..space].trim().is_empty())
                .map_or(end, |space| space + 1);
            rows.push(remaining[..split].trim_end_matches(' ').to_owned());
            remaining = remaining[split..].trim_start_matches(' ');
        }
        if remaining.is_empty() {
            return rows;
        }
    }
}

/// Wrap one sanitized Activity line without visiting the part outside the preview.
pub(super) fn activity_wrap(line: &str, columns: usize) -> impl Iterator<Item = &str> {
    let mut remaining = (columns > 0).then_some(line);
    std::iter::from_fn(move || {
        let line = remaining.take()?;
        let end = fitting_bytes(line, columns);
        if end == line.len() {
            return Some(line);
        }
        if end == 0 {
            let span = Span::raw(line);
            let first = span.styled_graphemes(Style::default()).next()?;
            remaining = Some(&line[first.symbol.len()..]).filter(|rest| !rest.is_empty());
            return Some("�");
        }
        let split = if line[end..].starts_with(' ') {
            end
        } else {
            line[..end]
                .rfind(' ')
                .filter(|&space| !line[..space].trim().is_empty())
                .unwrap_or(end)
        };
        remaining = Some(line[split..].trim_start_matches(' ')).filter(|rest| !rest.is_empty());
        Some(line[..split].trim_end_matches(' '))
    })
}

pub(super) fn fitting_bytes(line: &str, columns: usize) -> usize {
    let mut bytes = 0;
    let mut cells = 0;
    for grapheme in Span::raw(line).styled_graphemes(Style::default()) {
        cells += width(grapheme.symbol);
        if cells > columns {
            break;
        }
        bytes += grapheme.symbol.len();
    }
    bytes
}

pub(super) fn activity_preview(body: &str, columns: usize, limit: usize) -> Vec<String> {
    if columns == 0 || limit == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut cells = 0;
    for word in body.split_whitespace() {
        for part in activity_wrap(&display(word), columns) {
            let size = width(part);
            let gap = usize::from(!line.is_empty());
            if cells + gap + size > columns {
                if lines.len() + 1 == limit {
                    let last = excerpt(&line, columns, true);
                    let last = if last == "…" && columns > 1 {
                        let end = fitting_bytes(&line, columns - 1);
                        format!("{}…", &line[..end])
                    } else {
                        last
                    };
                    lines.push(last);
                    return lines;
                }
                lines.push(std::mem::take(&mut line));
                cells = 0;
            }
            if !line.is_empty() {
                line.push(' ');
                cells += 1;
            }
            line.push_str(part);
            cells += size;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}
