use super::{
    JUMP_KEYS, Palette, detail_line,
    rows::{Presentation, Row},
};
use crate::cli::mailbox::view::{Pane, RowKey, Tab, View};
use ratatui::{layout::Rect, text::Line};
use std::collections::VecDeque;

struct Content {
    sender: Option<String>,
    body: Vec<String>,
    address: Vec<String>,
}

impl Content {
    fn len(&self) -> usize {
        self.body.len() + self.address.len()
    }
}

pub(super) fn message_page(
    view: &mut View,
    rows: &[Row<'_>],
    area: Rect,
    now: u64,
    color: bool,
    palette: &Palette,
) -> (Vec<Line<'static>>, Vec<(char, RowKey)>) {
    let width = usize::from(area.width);
    let room = usize::from(area.height);
    let columns = width.saturating_sub(4);
    let active = view.focus == Pane::Messages;
    let open = view.tab != Tab::Activity || view.activity_open;
    let full = view.full_body.is_some();
    let cursor = view.cursor(Pane::Messages);
    let index = cursor.index;
    let first = cursor.first.min(index);
    let selected = &rows[index];
    let sender = |row: &Row<'_>, room| (room > 0).then(|| row.sender_line(columns)).flatten();
    let preview = |row: &Row<'_>, limit| {
        if open {
            row.body(columns, Some(limit), now)
        } else {
            Vec::new()
        }
    };
    let selected_sender = sender(selected, room - 1);
    let room_below_header = room - 1 - usize::from(selected_sender.is_some());
    let address = if active && (open || full) {
        selected
            .reply_label(columns, full)
            .into_iter()
            .take(room_below_header)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let remaining = room_below_header - address.len();
    let body = if let Some(scroll) = &mut view.full_body {
        let lines = selected.body(columns, None, now);
        scroll.fit(remaining, lines.len());
        lines
            .into_iter()
            .skip(scroll.offset)
            .take(scroll.shown)
            .collect()
    } else {
        preview(selected, remaining.min(if active { 3 } else { 2 }))
    };
    let selected = Content {
        sender: selected_sender,
        body,
        address,
    };
    let mut remaining = remaining.saturating_sub(selected.body.len());
    let mut page = VecDeque::from([(index, selected)]);
    if !full {
        for earlier in (first..index).rev() {
            let sender = sender(&rows[earlier], remaining.saturating_sub(2));
            let body = preview(&rows[earlier], 2);
            let needed = 2 + body.len() + usize::from(sender.is_some());
            if needed > remaining {
                break;
            }
            remaining -= needed;
            page.push_front((
                earlier,
                Content {
                    sender,
                    body,
                    address: Vec::new(),
                },
            ));
        }
        for (later, row) in rows.iter().enumerate().skip(index + 1) {
            if remaining < 2 {
                break;
            }
            let sender = sender(row, remaining - 2);
            let space = remaining - 2 - usize::from(sender.is_some());
            let body = preview(row, space.min(2));
            remaining = space - body.len();
            page.push_back((
                later,
                Content {
                    sender,
                    body,
                    address: Vec::new(),
                },
            ));
        }
    }
    if !full && let Some((first, _)) = page.front() {
        view.cursor(Pane::Messages)
            .show_from(*first, &rows[*first].key);
    }

    let hidden = rows.len() - page.len();
    let mut lines = Vec::new();
    let mut jumps = Vec::new();
    let mut letters = JUMP_KEYS.chars();
    for (i, content) in page {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let row = &rows[i];
        let letter = active.then(|| letters.next()).flatten();
        if let Some(letter) = letter {
            jumps.push((letter, row.key.clone()));
        }
        lines.push(row.header(
            now,
            width,
            Presentation {
                active: active && i == index,
                open: content.len() > 0,
                letter: letter.filter(|_| view.jumping),
                color,
            },
        ));
        lines.extend(
            content
                .sender
                .into_iter()
                .chain(content.body)
                .chain(content.address)
                .map(|line| detail_line(line, palette)),
        );
    }
    if remaining > 0 && hidden > 0 && !full {
        lines.push(Line::styled(format!("  … {hidden} more"), palette.rule));
    }
    (lines, jumps)
}
