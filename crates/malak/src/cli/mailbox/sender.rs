use super::text;
use crate::cli::output::display;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::path::Path;

#[derive(Clone, Deserialize)]
pub(super) struct Sender {
    transport: String,
    cwd: Option<String>,
    session: Option<Session>,
    earlier_session: Option<Session>,
}

#[derive(Clone, Deserialize)]
struct Session {
    program: String,
    pid: u32,
}

impl Sender {
    pub fn from_label(label: &Map<String, Value>) -> Option<Self> {
        serde_json::from_value(label.get("sender_trace")?.clone()).ok()
    }

    pub fn line(&self, actor: &str, columns: usize) -> String {
        let prefix = format!("{} sent from ", display(actor));
        if self.transport != "unix" {
            return fit(&format!("{prefix}{}", display(&self.transport)), columns);
        }
        let folder = self.cwd.as_deref().map_or_else(
            || "an unknown folder".into(),
            |cwd| {
                let home = std::env::home_dir();
                let relative = home
                    .as_ref()
                    .and_then(|home| Path::new(cwd).strip_prefix(home).ok());
                match relative {
                    Some(path) if path.as_os_str().is_empty() => "~".into(),
                    Some(path) => format!("~/{}", display(&path.to_string_lossy())),
                    None => display(cwd),
                }
            },
        );
        match (&self.session, &self.earlier_session) {
            (Some(session), Some(earlier)) => {
                let prefix = format!("{prefix}{} {} in ", display(&session.program), session.pid);
                let suffix = format!(" (earlier: {} {})", display(&earlier.program), earlier.pid);
                let room = columns.saturating_sub(text::width(&prefix) + text::width(&suffix));
                if room == 0 {
                    return fit(
                        &format!("{}{}", prefix.trim_end_matches(" in "), suffix),
                        columns,
                    );
                }
                format!("{prefix}{}{suffix}", fit(&folder, room))
            }
            _ => fit(&format!("{prefix}{folder}"), columns),
        }
    }
}

fn fit(value: &str, columns: usize) -> String {
    if text::width(value) <= columns {
        value.into()
    } else if columns == 0 {
        String::new()
    } else {
        let end = text::fitting_bytes(value, columns - 1);
        format!("{}…", &value[..end])
    }
}
