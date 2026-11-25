use super::{
    messages::{RecordBody, read_message},
    model::{Address, GrantAction, Model, Participant, Status},
    now_ms,
};
use crate::cli::{
    output::{self, display},
    participant::Session,
};
use malak::{ChannelName, Error};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    widgets::{Paragraph, Row, Table},
};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

const SCAN_LIMIT: u64 = 200;
const HEADINGS: [&str; 6] = [
    "Name",
    "Enrolled",
    "Grant changed",
    "Revoked",
    "Last message",
    "Grant ends",
];

pub(super) fn backfill(
    status: &Status,
    selected: Option<&str>,
    config: Option<PathBuf>,
) -> BTreeMap<String, u64> {
    let Some((mut session, viewer)) = Session::open_preview(config) else {
        return BTreeMap::new();
    };
    let now = i128::from(now_ms());
    let active = |person: &&Participant| {
        !person.revoked && person.grant.ends_at_ms().is_none_or(|end| end > now)
    };
    let Some(viewer) = status
        .participants
        .iter()
        .filter(active)
        .find(|p| p.id == viewer.as_str())
    else {
        return BTreeMap::new();
    };
    let mut latest: BTreeMap<String, u64> = BTreeMap::new();
    for channel in &status.channels {
        let Ok(name) = channel.name.parse::<ChannelName>() else {
            continue;
        };
        if !viewer.grant.covers(GrantAction::Get, &name) {
            continue;
        }
        let senders = status
            .participants
            .iter()
            .filter(active)
            .filter(|p| {
                selected.is_none_or(|id| id == p.id) && p.grant.covers(GrantAction::Send, &name)
            })
            .map(|p| p.id.as_str())
            .collect::<HashSet<_>>();
        if senders.is_empty() {
            continue;
        }
        let mut address = Address {
            channel: channel.name.clone(),
            creation: channel.creation.clone(),
            position: 0,
        };
        for position in
            (channel.floor.max(channel.head.saturating_sub(SCAN_LIMIT))..channel.head).rev()
        {
            address.position = position;
            match read_message(&mut session, &address, RecordBody::Omit) {
                Ok(Some(record)) if senders.contains(record.sender.as_str()) => {
                    let at_ms = record.at_ms();
                    let current = latest.entry(record.sender).or_default();
                    *current = (*current).max(at_ms);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }
    latest
}

pub(super) fn screen(frame: &mut Frame<'_>, model: &Model, scroll: usize) {
    let now = now_ms();
    let started = output::timestamp(model.started_at_ms, now);
    let [title, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(frame.area());
    frame.render_widget(
        Paragraph::new(format!("Participants · Run started {started} · q quit")),
        title,
    );
    let header = Row::new(HEADINGS).style(Style::default().add_modifier(Modifier::BOLD));
    let people = model.people();
    if frame.area().width < 85 || people.iter().any(|person| person.claim.is_some()) {
        let rows = people
            .iter()
            .skip(scroll)
            .flat_map(|person| {
                let fields = cells(model, person, now, false);
                let mut lines = vec![display(&fields[0])];
                lines.extend(
                    HEADINGS
                        .iter()
                        .skip(1)
                        .zip(fields.into_iter().skip(1))
                        .map(|(label, value)| format!("  {label}  {value}")),
                );
                lines.extend(claim_lines(person, now));
                lines.push(String::new());
                lines
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(rows.join("\n")), body);
        return;
    }
    let content = people.into_iter().skip(scroll).map(|person| {
        let mut values = cells(model, person, now, true);
        values[0] = display(&values[0]);
        Row::new(values)
    });
    let column = Constraint::Length(14);
    let widths = [Constraint::Min(8), column, column, column, column, column];
    frame.render_widget(Table::new(content, widths).header(header), body);
}

pub(super) fn print(model: &Model) -> Result<(), Error> {
    let now = now_ms();
    let started = output::timestamp(model.started_at_ms, now);
    let format = output::Format::stdout();
    let mut text = format.details("Participants", &[("Run started", &started)]);
    let rows = model
        .people()
        .iter()
        .map(|person| cells(model, person, now, false).to_vec())
        .collect::<Vec<_>>();
    if rows.is_empty() {
        text.push_str("No participants.\n");
    } else {
        text.push('\n');
        text.push_str(&format.table(&HEADINGS, &rows));
        for person in model.people() {
            let lines = claim_lines(person, now);
            if !lines.is_empty() {
                text.push('\n');
                text.push_str(&lines.join("\n"));
                text.push('\n');
            }
        }
    }
    output::write(&text)
}

fn claim_lines(person: &Participant, now: u64) -> Vec<String> {
    let Some(claim) = &person.claim else {
        return Vec::new();
    };
    let mut lines = vec![format!(
        "{}: {} · {} · issuer {}",
        display(&person.name),
        display(&claim.state),
        claim_process(&claim.session),
        display(&claim.issuer),
    )];
    if claim.refused > 0 {
        lines.push(format!(
            "  Refused {} · last {}",
            claim.refused,
            output::timestamp(claim.last_refused_at_ms, now),
        ));
    }
    lines.extend(claim.history.iter().map(|event| {
        let details = event
            .session
            .as_ref()
            .map_or_else(String::new, claim_process);
        format!(
            "  {} {} {}",
            output::timestamp(Some(event.at_ms), now),
            display(&event.event),
            details,
        )
    }));
    lines
}

fn claim_process(process: &super::model::ClaimProcess) -> String {
    let pid = process
        .pid
        .map_or_else(|| "?".into(), |pid| pid.to_string());
    let program = display(process.program.as_deref().unwrap_or("?"));
    let folder = display(process.folder.as_deref().unwrap_or("folder unavailable"));
    format!("PID {pid} {program} ({folder})")
}

fn cells(model: &Model, person: &Participant, now: u64, compact: bool) -> [String; 6] {
    let stamp = |ms: Option<u64>| {
        if compact {
            ms.map_or_else(|| "Unavailable".into(), |ms| output::age(ms, now))
        } else {
            output::timestamp(ms, now)
        }
    };
    [
        person.launcher_state.map_or_else(
            || person.name.clone(),
            |state| format!("{} {state}", person.name),
        ),
        stamp(person.enrolled_at_ms),
        stamp(person.grant_changed_at_ms),
        person.revoked_at_ms.map_or_else(
            || {
                if person.revoked {
                    "Unavailable".into()
                } else {
                    "No".into()
                }
            },
            |at| stamp(Some(at)),
        ),
        stamp(model.last_message(&person.id)),
        stamp(
            person
                .grant
                .ends_at_ms()
                .and_then(|at| u64::try_from(at).ok()),
        ),
    ]
}
