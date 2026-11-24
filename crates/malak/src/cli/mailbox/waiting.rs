//! Review the retained lead messages after each worker's acknowledgement cursor.

use super::{
    messages::{RecordBody, read_message},
    model::{Address, Model, Preview},
    status_with_workers, text,
};
use crate::cli::{
    config::Config,
    output, owner_for_board,
    participant::{Session, check_preview_board},
};
use malak::Error;
use serde::Serialize;
use std::{cmp::Reverse, fmt::Write, path::PathBuf};

#[derive(Serialize)]
struct Entry<'a> {
    participant: &'a str,
    inbox: &'a str,
    messages: Vec<Message>,
    #[serde(skip)]
    launcher_state: Option<&'static str>,
}

#[derive(Serialize)]
struct Message {
    position: u64,
    kind: String,
    #[serde(skip)]
    sent_at: [u64; 2],
}

impl Message {
    fn order(&self) -> ([u64; 2], u64) {
        (self.sent_at, self.position)
    }
}

pub(in crate::cli) fn run(
    config: Option<PathBuf>,
    name: Option<String>,
    json: bool,
) -> Result<(), Error> {
    check_preview_board(config.clone())?;
    let settings = Config::read(config.clone())?.board;
    let (mut reader, _) = owner_for_board(&settings)?;
    let model = Model::new(
        status_with_workers(&mut reader, false, &settings.state)?,
        name,
    );
    if !model.has_selection() {
        return Err(Error::refused("no participant by that name"));
    }
    let mut session = None;
    let mut waiting = Vec::new();
    for (participant, channel) in model.selected_worker_inboxes() {
        let due = channel.due_positions(&participant.id);
        if due.is_empty() {
            continue;
        }
        let (session, viewer) = match &mut session {
            Some(connection) => connection,
            None => session.insert(
                Session::open_preview(config.clone())
                    .ok_or_else(|| Error::unavailable("no handover to read the board with"))?,
            ),
        };
        if !model
            .participant(viewer.as_str())
            .is_some_and(|viewer| viewer.can_get(&channel.name))
        {
            continue;
        }
        let mut messages = Vec::new();
        for position in due {
            let address = channel.address(position);
            if let Some(message) = read_message(session, &address, RecordBody::Omit)?
                && message.sender == "lead"
            {
                messages.push(Message {
                    position,
                    kind: message.kind,
                    sent_at: message.sent_at,
                });
            }
        }
        if !messages.is_empty() {
            waiting.push(Entry {
                participant: &participant.name,
                inbox: &channel.name,
                messages,
                launcher_state: participant.launcher_state,
            });
        }
    }
    if json {
        waiting.sort_by_key(|entry| (entry.participant, entry.inbox));
        return output::line(format_args!("{}", serde_json::json!({"waiting": waiting})));
    }
    for entry in &mut waiting {
        entry
            .messages
            .sort_unstable_by_key(|message| Reverse(message.order()));
    }
    waiting.sort_unstable_by_key(|entry| {
        let newest = &entry.messages[0];
        Reverse((newest.sent_at, entry.inbox, newest.position))
    });
    output::write(&render(&waiting, &output::Format::stdout()))
}

pub(super) fn live(model: &Model) -> impl Iterator<Item = (Address, &Preview)> {
    model
        .selected_worker_inboxes()
        .flat_map(|(person, channel)| {
            channel
                .due_positions(&person.id)
                .map(|at| channel.address(at))
        })
        .filter_map(|address| {
            let message = model.resolved_preview(&address).flatten()?;
            (message.sender == "lead").then_some((address, message))
        })
}

fn render(entries: &[Entry<'_>], format: &output::Format) -> String {
    let mut out = String::new();
    if format.interactive() {
        out.push_str(&format.details("Waiting inboxes", &[]));
    }
    for entry in entries {
        let state = entry
            .launcher_state
            .map_or_else(String::new, |s| format!(" {s}"));
        let name = format!("{}{state}  {}", entry.participant, entry.inbox);
        let messages = entry
            .messages
            .iter()
            .map(|message| format!("#{} {}", message.position, message.kind))
            .collect::<Vec<_>>();
        if format.interactive() {
            text::item(
                &mut out,
                text::terminal_width(),
                &name,
                messages.iter().map(String::as_str),
            );
        } else {
            writeln!(out, "{name}  {}", messages.join(", ")).unwrap();
        }
    }
    out
}
