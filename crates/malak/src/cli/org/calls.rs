//! Validated calls shared by the CLI, MCP, pulse, digest and watchers.

mod model;
mod names;
pub(in crate::cli) mod settings;
#[cfg(test)]
mod tests;
mod write;
pub(in crate::cli) use model::{Link, Projection};
#[cfg(feature = "mcp")]
pub(in crate::cli) use write::record;
pub(in crate::cli) use write::{Action, recipe, record_recipe, write};

use super::{bounded, records, seats::Reader};
use crate::cli::{board_group::Entry, board_names};
use malak::Error;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// A special decision-looking answer is never trusted as an ordinary lead
/// answer when its source cannot be proved. None means an ordinary answer.
pub(in crate::cli) fn recorded(
    config: Option<std::path::PathBuf>,
    record: &Value,
) -> Option<Option<Value>> {
    let text = super::typed::text(record);
    let header = text.split("\n\n").next().unwrap_or("");
    let has_decision = header.lines().any(|line| line.starts_with("Decision: "));
    if let Some(by) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Decided by: "))
        && by != names::TOP_RECORDED
        && by != names::OWNER_RECORDED
        && !has_decision
    {
        let active = by.starts_with("owner") && top_active(config.clone());
        return (!model::ordinary_answer(&text, active)).then_some(None);
    }
    let special = has_decision
        || text.starts_with("Decided by: top lead")
        || text.starts_with("Decided by: owner, relayed by top lead")
        || [
            names::LEVEL,
            names::MERGE,
            names::DECISION,
            "cos-decision",
            "owner-decision",
        ]
        .iter()
        .any(|kind| text.starts_with(&format!("Type: {kind}\n")));
    if !special {
        return None;
    }
    Some((|| {
        if record["sender"] != names::LEAD
            || record["grade"] != "proven"
            || record["kind"] != "note"
            || record["headers"]["reply_to"][0] != names::QUESTION_CHANNEL
        {
            return None;
        }
        if let Some(note) = checked_by_lead(config.clone(), &text) {
            return Some(note);
        }
        let reader = Reader::open(config.clone()).ok()?;
        let board = config
            .map(|config| board_names::name(&Entry::read(config)))
            .unwrap_or_else(|| reader.home_board.clone());
        let reply = &record["headers"]["reply_to"];
        let question = Link(
            board,
            reply[0].as_str()?.into(),
            reply[1].as_str()?.into(),
            reply[2].as_u64()?,
        );
        let projection = load(&reader).ok()?;
        let recorded = projection.recorded_decision(&question, &text)?;
        let decisions = projection.decisions_for(&question);
        let mut latest = (*decisions.last()?).clone();
        let mut display = format!(
            "Recorded decision: {}\nDecision source: {} ({})\n\n{}",
            recorded["link"],
            latest["link"],
            latest["decided_by"].as_str().unwrap_or(""),
            latest["body"].as_str().unwrap_or("")
        );
        for earlier in &decisions[..decisions.len() - 1] {
            display.push_str(&format!(
                "\n\nEarlier decision: {} ({})\n\n{}",
                earlier["link"],
                earlier["decided_by"].as_str().unwrap_or(""),
                earlier["body"].as_str().unwrap_or("")
            ));
        }
        latest["text"] = json!(display);
        latest["source_checked_by"] = json!("self");
        Some(latest)
    })())
}

/// Subs trust their own proven lead's record. The credential
/// check binds this view to that board; no foreign reader Seat is opened.
fn checked_by_lead(config: Option<std::path::PathBuf>, text: &str) -> Option<Value> {
    if crate::cli::participant::preview_handover(config)
        .ok()?
        .id()
        .is_lead()
    {
        return None;
    }
    let (header, body) = text.split_once("\n\n")?;
    let by = match header.lines().next()?.strip_prefix("Decided by: ")? {
        names::TOP_RECORDED => names::TOP,
        names::OWNER_RECORDED => names::OWNER_RELAY,
        _ => return None,
    };
    let mut links = header
        .lines()
        .filter_map(|line| line.strip_prefix("Decision: "));
    let link = Link::parse(links.next()?)?;
    if links.next().is_some() || link.1 != names::CHANNEL {
        return None;
    }
    Some(json!({"decided_by":by,"text":body,"source_checked_by":"lead"}))
}

/// An unattributed legacy reply proves a lead decision in an organisation,
/// never owner provenance. Standalone boards do not infer this organisation attribution.
pub(in crate::cli) fn bare_lead_answer(config: Option<std::path::PathBuf>, record: &Value) -> bool {
    record["sender"] == "lead"
        && record["grade"] == "proven"
        && record["headers"]["reply_to"][0] == "questions"
        && !super::typed::text(record)
            .lines()
            .next()
            .is_some_and(|line| line.starts_with("Decided by: "))
        && top_active(config)
}

/// Attribution also applies to subs, which cannot open organisation readers.
/// Consult only owner roster metadata; this never grants or reads a Seat.
fn top_active(config: Option<std::path::PathBuf>) -> bool {
    use crate::cli::config::Config;
    let Ok(local) = Config::read(config) else {
        return false;
    };
    board_names::list_quiet()
        .unwrap_or_default()
        .iter()
        .any(|entry| {
            super::seats::load(&entry.path)
                .ok()
                .flatten()
                .is_some_and(|roster| {
                    roster.top_lead.is_some()
                        && std::iter::once(&roster.top)
                            .chain(roster.leads.values().map(|lead| &lead.config))
                            .any(|path| {
                                Config::read(Some(path.clone()))
                                    .is_ok_and(|other| other.board.state == local.board.state)
                            })
                })
        })
}

/// Persist only a fingerprint in delivery cursors, never question or owner text.
pub(in crate::cli) fn signature(item: &Value) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(item.to_string().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(in crate::cli) fn top(reader: &Reader) -> Option<String> {
    reader
        .roster
        .as_ref()
        .filter(|roster| roster.top_lead.is_some())
        .map(|roster| board_names::name(&Entry::read(roster.top.clone())))
}

pub(in crate::cli) fn load(reader: &Reader) -> Result<Projection, Error> {
    let top = top(reader);
    let candidate = reader
        .roster
        .as_ref()
        .and_then(|roster| roster.candidate.as_ref())
        .and_then(|candidate| candidate["id"].as_str());
    let mut boards = BTreeMap::new();
    let mut options = BTreeMap::new();
    for board in reader.boards() {
        let fetched = (|| {
            let mut seat = reader.open_board(Some(&board))?;
            let channels: &[&str] = if top.as_deref() == Some(&board) && board != reader.home_board
            {
                &["calls"]
            } else {
                &["questions", "calls", "lead", "to/*"]
            };
            records::retained(&seat.config, &mut seat.session, channels)
        })();
        match fetched {
            Ok(records) => {
                options.insert(board.clone(), settings::for_board(reader, &board)?);
                boards.insert(board, records);
            }
            Err(error) if board == reader.home_board => return Err(error),
            // An unavailable peer cannot prove a merge or decision. It never
            // makes a local question look answered.
            Err(_) => (),
        }
    }
    Ok(model::project_with_settings(
        &boards,
        top.as_deref(),
        candidate,
        reader
            .roster
            .as_ref()
            .map(|roster| board_names::name(&Entry::read(roster.top.clone())))
            .as_deref(),
        &options,
    ))
}

pub(in crate::cli) fn list(
    reader: &Reader,
    level: Option<&str>,
    all: bool,
) -> Result<Vec<Value>, Error> {
    let selected: Option<Vec<_>> = level.map(|levels| levels.split(',').collect());
    if selected
        .as_ref()
        .is_some_and(|levels| levels.iter().any(|level| level.is_empty()))
    {
        return Err(Error::refused(
            "level filters must be nonempty names separated by commas",
        ));
    }
    let projection = load(reader)?;
    let board = (!reader.chief).then_some(reader.home_board.as_str());
    let mut items = projection
        .open(board)
        .into_iter()
        .filter(|call| {
            let level = call["level"].as_str().unwrap_or("");
            selected
                .as_ref()
                .map_or(all || call["delivery"] != "digest", |levels| {
                    levels.contains(&level)
                })
        })
        .collect::<Vec<_>>();
    items.extend(
        projection
            .notes
            .into_iter()
            .filter(|note| board.is_none_or(|board| note["board"] == board)),
    );
    items.sort_by_key(|item| {
        (
            std::cmp::Reverse(item["at_ms"].as_u64()),
            item["link"].to_string(),
        )
    });
    Ok(items)
}

pub(in crate::cli) fn result(items: Vec<Value>) -> Value {
    bounded::result(
        items,
        "Fetch the indicated call fact with get, then other retained calls positions on that board and each linked question or decision. Use calls --level NAME or --all to narrow the list. All links name board, channel, creation and position.",
    )
}

pub(in crate::cli) fn render(item: &Value) -> String {
    let mut text = String::new();
    if item["type"] == "call" {
        text.push_str(&format!(
            "{} · {}\n",
            item["level"].as_str().unwrap_or(""),
            item["subject"].as_str().unwrap_or("")
        ));
        for asker in item["askers"].as_array().into_iter().flatten() {
            text.push_str(&format!(
                "  {} · proposed: {} · effective: {}\n",
                asker["link"],
                asker["proposed"].as_str().unwrap_or("none"),
                asker["effective"].as_str().unwrap_or("none")
            ));
            for change in asker["proposal_history"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(asker["level_history"].as_array().into_iter().flatten())
            {
                text.push_str(&format!(
                    "    {} → {} · {}\n",
                    change["original"].as_str().unwrap_or("none"),
                    change["level"].as_str().unwrap_or(""),
                    change["link"]
                ));
            }
        }
        for decision in item["decisions"].as_array().into_iter().flatten() {
            text.push_str(&format!(
                "  Decided by: {} · {}\n{}\n",
                decision["decided_by"].as_str().unwrap_or(""),
                decision["link"],
                decision["body"].as_str().unwrap_or("")
            ));
        }
    } else {
        text.push_str(&format!(
            "{}\n{}\n",
            item["link"],
            item["text"].as_str().unwrap_or("")
        ));
    }
    text
}

pub(in crate::cli) fn print(value: &Value, json: bool) -> Result<(), Error> {
    if json {
        return crate::cli::output::line(format_args!("{value}"));
    }
    let mut text = String::new();
    for item in value["items"].as_array().into_iter().flatten() {
        text.push_str(&render(item));
    }
    if value["omitted"].as_u64().unwrap_or(0) > 0 {
        text.push_str(&format!(
            "{} items left out. {}\n",
            value["omitted"],
            value["next"].as_str().unwrap_or("")
        ));
    }
    crate::cli::output::write(if text.is_empty() {
        "No open calls.\n"
    } else {
        &text
    })
}

/// Delivery hook for a future push Door. Only this effective colour may push.
pub(in crate::cli) fn effective_red(call: &Value) -> bool {
    call["open"] == true
        && (call["delivery"] == "tick" || call["delivery"].is_null() && call["level"] == "red")
}

/// Watchers use proposal OR effective red (AM2); a lowered proposal still
/// reaches the agents, while a future owner push uses effective_red alone.
pub(in crate::cli) fn watch(reader: &Reader) -> Result<Vec<Value>, Error> {
    let projection = load(reader)?;
    let board = (!reader.chief).then_some(reader.home_board.as_str());
    let mut items: Vec<_> = projection
        .open(board)
        .into_iter()
        .filter(|call| {
            effective_red(call)
                || call["askers"].as_array().is_some_and(|askers| {
                    askers.iter().any(|asker| {
                        asker["open"] == true
                            && asker["proposed_delivery"] == "tick"
                            && board.is_none_or(|board| asker["link"][0] == board)
                    })
                })
        })
        .collect();
    // Every top call note naming this lead's board must be delivered (AM5),
    // including decisions and merges that do not change an effective colour.
    if !reader.chief
        && let Some(top) = top(reader)
    {
        // A missing peer feed cannot hide a proven local proposal. As in
        // load(), unavailable peer facts supply no decision authority.
        let records = reader
            .open_board(Some(&top))
            .and_then(|mut seat| records::retained(&seat.config, &mut seat.session, &["calls"]))
            .unwrap_or_default();
        for record in records {
            let valid = [names::LEVEL, names::MERGE, names::DECISION]
                .iter()
                .find_map(|kind| super::typed::fields(&record, kind, "lead"));
            if let Some(fields) = valid {
                let links = fields
                    .get("Link")
                    .and_then(|value| Link::parse(value))
                    .into_iter()
                    .chain(
                        fields
                            .get("Links")
                            .and_then(|value| serde_json::from_str::<Vec<Link>>(value).ok())
                            .into_iter()
                            .flatten(),
                    );
                if links
                    .into_iter()
                    .any(|link| projection.involves(&link, &reader.home_board))
                {
                    items.push(json!({"type":"note","board":top,"channel":"calls","creation":record["creation"],"position":record["position"],"link":[top,"calls",record["creation"],record["position"]],"text":super::typed::text(&record),"at_ms":super::typed::at_ms(&record)}));
                }
            }
        }
    }
    items.sort_by_key(|item| {
        (
            std::cmp::Reverse(item["at_ms"].as_u64()),
            std::cmp::Reverse(item["link"][3].as_u64()),
        )
    });
    Ok(items)
}
