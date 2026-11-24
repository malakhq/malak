//! Human terminal templates and the shared terminal-safety boundary.

use super::config::io_error;
use malak::Error;
use ratatui::{crossterm::terminal, text::Line};
use std::fmt::{self, Write as _};
use std::io::{self, IsTerminal, Write};
mod table;
mod time;
mod usage;

pub(super) use time::{age, clock, local_zone, now_ms, timestamp};
pub(super) use usage::{error, help, operation_error, usage_error};

pub(super) fn line(args: fmt::Arguments<'_>) -> Result<(), Error> {
    writeln!(io::stdout().lock(), "{args}").map_err(io_error)
}

pub(super) fn display(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
            text.extend(character.escape_default());
        } else if !matches!(character, '\u{00ad}' | '\u{034f}' | '\u{061c}' | '\u{115f}'..='\u{1160}' | '\u{17b4}'..='\u{17b5}' | '\u{180b}'..='\u{180f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{3164}' | '\u{fe00}'..='\u{fe0f}' | '\u{feff}' | '\u{ffa0}' | '\u{fff0}'..='\u{fff8}' | '\u{1bca0}'..='\u{1bca3}' | '\u{1d173}'..='\u{1d17a}' | '\u{e0000}'..='\u{e0fff}')
        {
            text.push(character);
        }
    }
    text
}

pub(super) struct Format {
    width: usize,
    interactive: bool,
    styled: bool,
}

impl Format {
    pub(super) fn stdout() -> Self {
        Self::new(io::stdout().is_terminal())
    }

    pub(super) fn interactive(&self) -> bool {
        self.interactive
    }

    pub(super) fn width(&self) -> usize {
        self.width
    }

    fn stderr(legacy: bool) -> Self {
        let stderr = io::stderr();
        let mut format = Self::new(stderr.is_terminal());
        if format.interactive
            && !legacy
            && let Ok(size) = rustix::termios::tcgetwinsize(stderr)
            && size.ws_col > 0
        {
            format.width = usize::from(size.ws_col);
        }
        format
    }

    fn new(interactive: bool) -> Self {
        let interactive =
            interactive && std::env::var_os("TERM").as_deref() != Some("dumb".as_ref());
        Self {
            width: if interactive {
                terminal::size()
                    .ok()
                    .filter(|(width, _)| *width > 0)
                    .map_or(80, |(width, _)| width as usize)
            } else {
                80
            },
            interactive,
            styled: interactive && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    pub(super) fn details(&self, title: &str, fields: &[(&str, &str)]) -> String {
        let mut out = String::new();
        self.wrapped(&mut out, "", &display(title), self.interactive);
        if self.styled {
            out = format!("\x1b[1m{}\x1b[0m\n", out.trim_end_matches('\n'));
        }
        self.fields(&mut out, fields);
        out
    }

    pub(super) fn screen(&self, title: &str, section: &str, fields: &[(&str, &str)]) -> String {
        if !self.interactive {
            return self.details(title, fields);
        }
        let mut out = self.details(title, &[]);
        let next = fields.iter().find(|(label, _)| *label == "Next");
        let fields: Vec<_> = fields
            .iter()
            .copied()
            .filter(|(label, _)| *label != "Next")
            .collect();
        if !fields.is_empty() {
            if !section.is_empty() {
                self.section(&mut out, section);
            }
            self.screen_fields(&mut out, &fields);
        }
        if let Some((_, step)) = next {
            self.next(&mut out, step);
        }
        out
    }

    pub(super) fn section(&self, out: &mut String, title: &str) {
        out.push('\n');
        self.rule(out, '─');
        out.push_str(&self.details(title, &[]));
    }

    pub(super) fn next(&self, out: &mut String, step: &str) {
        if !self.interactive {
            return;
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
        self.rule(out, '═');
        let mut footer = String::from("Next\n");
        self.paragraph(&mut footer, "  ", step);
        self.dim(out, &footer);
    }

    fn rule(&self, out: &mut String, line: char) {
        if self.interactive {
            let mut rule = line.to_string().repeat(self.width.min(80));
            rule.push('\n');
            self.dim(out, &rule);
        }
    }

    fn dim(&self, out: &mut String, text: &str) {
        if self.styled {
            writeln!(out, "\x1b[2m{}\x1b[0m", text.trim_end_matches('\n')).unwrap();
        } else {
            out.push_str(text);
        }
    }

    pub(super) fn fields(&self, out: &mut String, fields: &[(&str, &str)]) {
        self.render_fields(out, fields, false, "  ");
    }

    pub(super) fn screen_fields(&self, out: &mut String, fields: &[(&str, &str)]) {
        self.render_fields(out, fields, self.interactive, "  ");
    }

    fn render_fields(
        &self,
        out: &mut String,
        fields: &[(&str, &str)],
        compact: bool,
        indent: &str,
    ) {
        let label_width = fields
            .iter()
            .map(|(label, _)| width(label))
            .max()
            .unwrap_or(0);
        for &(label, value) in fields {
            let label = display(label);
            let value = display(value);
            let gap = label_width.saturating_sub(width(&label)) + 2;
            let prefix = format!("{indent}{label}{:gap$}", "");
            let available = self.width.saturating_sub(width(&prefix));
            if compact
                && width(&value) > available
                && (available < 16 || value.split_whitespace().any(|word| width(word) > available))
            {
                self.wrapped(out, indent, &label, true);
                self.wrapped(out, &format!("{indent}  "), &value, true);
            } else {
                self.wrapped(out, &prefix, &value, self.interactive);
            }
        }
    }

    pub(super) fn message(&self, title: &str, fields: &[(&str, &str)], body: &[u8]) -> String {
        let mut out = self.screen(title, "", fields);
        let text = std::str::from_utf8(body);
        if self.interactive {
            let heading = match text {
                Ok(_) => "Body".to_owned(),
                Err(_) => format!("Body ({} bytes, hexadecimal)", body.len()),
            };
            self.section(&mut out, &heading);
        }
        match text {
            Ok("") if self.interactive => out.push_str("    Empty\n"),
            Ok("") => out.push_str("\n  Body  Empty\n"),
            Ok(body) => {
                if !self.interactive {
                    out.push_str("\n  Body\n");
                }
                self.paragraph(&mut out, "    ", body);
            }
            Err(_) => {
                if !self.interactive {
                    writeln!(out, "\n  Body ({} bytes, hexadecimal)", body.len()).unwrap();
                }
                let row = self.width.saturating_sub(4).div_euclid(3).max(1);
                for chunk in body.chunks(row) {
                    out.push_str("    ");
                    for (index, byte) in chunk.iter().enumerate() {
                        if index != 0 {
                            out.push(' ');
                        }
                        write!(out, "{byte:02x}").unwrap();
                    }
                    out.push('\n');
                }
            }
        }
        out
    }

    pub(super) fn paragraph(&self, out: &mut String, indent: &str, text: &str) {
        for line in text.split('\n') {
            self.wrapped(out, indent, &display(line), self.interactive);
        }
    }

    fn wrapped(&self, out: &mut String, prefix: &str, text: &str, wrap: bool) {
        out.push_str(prefix);
        if !wrap {
            out.push_str(text);
            out.push('\n');
            return;
        }
        let indent = width(prefix);
        let mut used = indent;
        for word in text.split_inclusive(' ') {
            let cells = width(word);
            if used > indent && used + width(word.trim_end_matches(' ')) > self.width {
                while out.ends_with(' ') && used > indent {
                    out.pop();
                    used -= 1;
                }
                out.push('\n');
                write!(out, "{:indent$}", "").unwrap();
                used = indent;
            }
            out.push_str(word);
            used += cells;
        }
        out.push('\n');
    }
}

pub(super) fn width(value: &str) -> usize {
    Line::from(value).width()
}

pub(super) fn write(text: &str) -> Result<(), Error> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(io_error)
}

pub(super) fn details(title: &str, fields: &[(&str, &str)]) -> Result<(), Error> {
    write(&Format::stdout().screen(title, "", fields))
}

pub(super) fn receipt(title: &str, fields: &[(&str, &str)]) -> Result<(), Error> {
    write(&Format::stdout().screen(title, "Receipt", fields))
}

pub(super) fn result(text: &str) -> Result<(), Error> {
    details(text, &[])
}

pub(super) fn diagnostic(title: &str, fields: &[(&str, &str)]) {
    write_diagnostic(&Format::stderr(true).details(title, fields));
}

fn write_diagnostic(text: &str) {
    let _ = io::stderr().lock().write_all(text.as_bytes());
}

pub(super) fn progress(text: &str, next: Option<&str>) {
    let format = Format::stderr(false);
    if format.interactive {
        let mut screen = format.details(text, &[]);
        if let Some(next) = next {
            format.next(&mut screen, next);
        }
        write_diagnostic(&screen);
    }
}

pub(super) fn sender_name_or_id(id: &str) -> &str {
    match id {
        malak_core::upgrade_notice::SENDER_ID => "owner",
        _ => id,
    }
}

pub(super) fn short_id(id: &str) -> String {
    if id.len() > 12 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        format!("{}...", &id[..8])
    } else {
        id.to_owned()
    }
}

/// Choose a prefix that identifies this ID among the IDs shown in a table.
pub(super) fn unique_id<'a>(id: &str, others: impl Iterator<Item = &'a str> + Clone) -> String {
    if short_id(id) == id {
        return id.to_owned();
    }
    for end in 8..id.len() {
        let prefix = &id[..end];
        if !others
            .clone()
            .any(|other| other != id && other.starts_with(prefix))
        {
            return format!("{prefix}...");
        }
    }
    id.to_owned()
}

pub(super) fn durability(value: &str) -> &str {
    match value {
        "memory" => "In memory",
        "restart" => "Survives a board restart",
        "synced" => "Synced to storage",
        other => other,
    }
}

pub(super) fn event(at: Option<u64>, action: &str) -> String {
    format!("{}  {}", clock(at), display(action))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(columns: usize) -> Format {
        Format {
            width: columns,
            interactive: true,
            styled: false,
        }
    }

    #[test]
    fn terminal_next_step_is_behind_the_footer_rule() {
        let screen = terminal(45).screen(
            "Hook probe received.",
            "",
            &[
                ("Channel", "selftest/lead"),
                ("Next", "Trigger a hook event."),
            ],
        );

        assert!(screen.ends_with(&format!(
            "\n{}\nNext\n  Trigger a hook event.\n",
            "═".repeat(45)
        )));
    }

    #[test]
    fn piped_receipt_keeps_its_original_bytes() {
        let screen = Format::new(false).screen("Note sent.", "Receipt", &[("ID", "fixture")]);

        assert_eq!(screen, "Note sent.\n  ID  fixture\n");
    }

    #[test]
    fn a_narrow_name_table_moves_complete_ids_below_aligned_names() {
        let id = "0123456789abcdef0123456789abcdef";
        let rows = [vec![
            "A particularly long display name for this test".into(),
            "Active".into(),
            id.into(),
        ]];

        let table = terminal(45).name_table(&["Name", "State", "ID"], &rows, |_| false);

        assert!(table.contains("…  Active"), "{table}");
        assert!(table.contains(id), "{table}");
        assert!(table.lines().all(|line| width(line) <= 45), "{table}");
    }

    #[test]
    fn message_body_keeps_lines_and_neutralizes_terminal_controls() {
        let format = Format::new(false);
        let body = b"First\r\n\x1b[2JSecond\n\nLast";
        let block = format.message("Task from lead in help #1", &[], body);

        assert_eq!(
            block,
            "Task from lead in help #1\n\n  Body\n    First\\r\n    \\u{1b}[2JSecond\n    \n    Last\n"
        );
    }

    #[test]
    fn narrow_tables_use_labelled_records_without_cutting_values() {
        let format = Format {
            width: 20,
            ..Format::new(false)
        };
        let rows = [vec!["questions".into(), "abcdef0123456789".into()]];

        assert_eq!(
            format.table(&["Channel", "Creation"], &rows),
            "  Channel  questions\n  Creation  abcdef0123456789\n"
        );
    }

    #[test]
    fn identifier_prefixes_expand_when_they_would_be_ambiguous() {
        let id = "abcdef0133333333";
        let other = "abcdef0122222222";

        assert_eq!(unique_id(id, [id, other].into_iter()), "abcdef013...");
    }

    #[test]
    fn dynamic_labels_and_values_cannot_add_output_lines() {
        let format = Format::new(false);
        let block = format.details("Message\u{202e} A\nB", &[("Creation", "ID\u{200b}\nNext")]);

        assert_eq!(block, "Message A\\nB\n  Creation  ID\\nNext\n");
    }
}
