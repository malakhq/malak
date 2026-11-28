//! Publish pending launcher facts only from the current lead's own session.

use super::records;
use crate::cli::{
    board_group, board_names,
    config::{Config, io_error},
    owner, owner_commands,
    participant::{self, Command, Session},
};
use malak::{Error, Kind};
use malak_fs::{Directory, check_user_permissions};
use nix::{fcntl::OFlag, unistd::geteuid};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

mod state;

static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn notice_once(notice: String) -> Option<String> {
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    eprintln!("{notice}");
    Some(notice)
}

pub(in crate::cli) fn reconcile(config: &Path, session: &mut Session) -> Option<String> {
    match sync(config, session) {
        Ok(Some(reason)) => notice_once(format!(
            "Rail publication paused: {reason}. No lifecycle facts will be replayed until the owner restores this board's publication history."
        )),
        Ok(None) => None,
        Err(error) => notice_once(format!(
            "Rail publication pending: {error}. Source facts remain on the board; the next lead command or watcher tick will retry."
        )),
    }
}

/// A failed publication never interrupts the caller's normal command. Pending
/// facts are durable and the next command or watcher tick tries them again.
pub(in crate::cli) fn on_command(config: Option<PathBuf>) -> Option<String> {
    let result = (|| {
        let path = if let Some(path) = config {
            let handover = participant::preview_handover(Some(path.clone()))?;
            if !handover.id().is_lead() {
                return Ok(None);
            }
            path
        } else {
            super::seats::primary_home(None)?.0
        };
        let settings = Config::read(Some(path.clone()))?;
        if !settings.board.state.join("launches").exists() {
            return Ok(None);
        }
        let mut session = Session::open(Some(path.clone()))?;
        Ok(reconcile(&path, &mut session))
    })();
    let result: Result<Option<String>, Error> = result;
    result.ok().flatten()
}

fn sync(config: &Path, session: &mut Session) -> Result<Option<String>, Error> {
    let handover = participant::preview_handover(Some(config.to_owned()))?;
    if !handover.id().is_lead() {
        return Ok(None);
    }
    let settings = Config::read(Some(config.to_owned()))?.board;
    let launches =
        match Directory::open(&settings.state).and_then(|root| root.child("launches".as_ref())) {
            Ok(folder) => folder,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        };
    let status = owner_commands::status(&mut owner(Some(config.to_owned()))?, &[])?;
    let people = status["participants"]
        .as_array()
        .ok_or_else(|| Error::unavailable("no participant metadata"))?;
    if !people
        .iter()
        .any(|person| person["id"] == "lead" && person["claim"]["state"] == "bound")
    {
        return Ok(None);
    }
    let rail_floor = status["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|channel| channel["name"] == "rail")
        .and_then(|channel| channel["floor"].as_u64())
        .unwrap_or(1);
    let mut state = match state::State::open(&settings.state, rail_floor)? {
        state::Open::Ready(state) => state,
        state::Open::Busy => return Ok(None),
        state::Open::Paused(reason) => return Ok(Some(reason.to_owned())),
    };
    let pin = handover
        .board_key()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut workers = Vec::new();
    for person in people.iter().filter(|person| person["id"] != "lead") {
        let Some(id) = person["id"].as_str() else {
            continue;
        };
        let file = match launches.open_file(format!("{id}.json").as_ref(), OFlag::O_RDONLY) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error(error)),
        };
        check_user_permissions(&file, geteuid().as_raw(), true).map_err(io_error)?;
        let evidence: Value =
            serde_json::from_reader(file).map_err(|error| io_error(error.into()))?;
        if evidence["board"] == pin && evidence["id"] == id {
            workers.push((person, evidence));
        }
    }
    if workers.is_empty() {
        return Ok(None);
    }
    let board = board_names::name(&board_group::Entry::read(config.to_owned()));
    let messages = records::retained(config, session, &["lead", "rail"])?;
    let mut published: Vec<_> = super::rail::retained_projection(&board, &messages)
        .into_iter()
        .filter(|event| {
            event["type"] == "rail"
                && matches!(event["event"].as_str(), Some("started" | "finished"))
        })
        .map(|event| {
            (
                event["event"].as_str().unwrap().to_owned(),
                event["link"].clone(),
            )
        })
        .collect();
    let mut changed = false;
    for (event, link) in &published {
        changed |= state.remember(event, link);
    }
    if changed {
        state.save()?;
    }
    let mut pending = Vec::new();
    for (person, evidence) in workers {
        let id = person["id"].as_str().unwrap();
        for fact in messages
            .iter()
            .filter(|record| record["channel"] == "lead" && record["grade"] == "proven")
        {
            let accepted = fact["sender"] == "malakowner"
                && fact["kind"] == "note"
                && fact["id"].as_str().is_some_and(|notice| {
                    notice.starts_with(&format!("claim-{id}-")) && notice.ends_with("-accepted")
                });
            let finished = fact["sender"] == id
                && fact["kind"] == "report"
                && ["report", "last_report"]
                    .iter()
                    .any(|field| matches_reference(fact, &evidence[field]));
            let event = if accepted {
                "started"
            } else if finished {
                "finished"
            } else {
                continue;
            };
            pending.push((event, person, fact));
        }
    }
    // All source facts are on this board's lead channel. Catch up in source
    // order so the newest open start also remains the newest on the rail.
    pending.sort_by_key(|(_, _, fact)| fact["position"].as_u64());
    for (event, person, fact) in pending {
        let link = json!([board, "lead", fact["creation"], fact["position"]]);
        if state.contains(event, &link)
            || published
                .iter()
                .any(|(kind, old)| kind == event && old == &link)
        {
            continue;
        }
        let subject = person["name"]
            .as_str()
            .or_else(|| person["id"].as_str())
            .unwrap();
        let body = format!("Type: rail\nEvent: {event}\nSubject: {subject}\nLink: {link}\n");
        let identifier = format!(
            "rail-{event}-{}-{}",
            fact["creation"].as_str().unwrap(),
            fact["position"]
        );
        Command::Send {
            channel: "rail".into(),
            id: Some(identifier),
            kind: Kind::Note,
            reply_to: Vec::new(),
            content_type: None,
            max_bytes: None,
            trace: None,
            json: true,
            body: None,
        }
        .execute_tool(session, Some(body.into_bytes()))?;
        state.remember(event, &link);
        state.save()?;
        published.push((event.to_owned(), link));
    }

    Ok(None)
}

fn matches_reference(fact: &Value, reference: &Value) -> bool {
    ["channel", "creation", "position", "id", "sent_at"]
        .iter()
        .all(|field| !reference[field].is_null() && fact[field] == reference[field])
        && fact["headers"]["trace"] == reference["trace"]
}
