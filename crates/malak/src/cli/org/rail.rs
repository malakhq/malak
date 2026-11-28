//! Project lead-authored rail notes and merge them by time.

use super::{records, seats::Reader, typed};
use malak::Error;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

mod dependencies;
pub(in crate::cli) use dependencies::{open_waits, read as reader_projection};

const EVENTS: &[&str] = &[
    "started",
    "finished",
    "merged",
    "blocked",
    "unblocked",
    "waits on",
    "wait done",
    "handed over",
    "decided",
    "lead removed",
    "chief of staff switched",
];

pub(in crate::cli) fn project(
    board: &str,
    messages: &[Value],
    resolve: impl FnMut(&Value) -> Option<Value>,
) -> Vec<Value> {
    project_with_peers(board, messages, resolve, &HashMap::new())
}

fn project_with_peers(
    board: &str,
    messages: &[Value],
    mut resolve: impl FnMut(&Value) -> Option<Value>,
    peers: &dependencies::Peers,
) -> Vec<Value> {
    let mut ordered: Vec<_> = messages.iter().collect();
    ordered.sort_by_key(|record| record["position"].as_u64());
    let mut waits = HashMap::<(String, u64), Value>::new();
    let mut lifecycle = HashSet::new();
    ordered.into_iter().map(|record| {
        let mut item = json!({"board":board, "channel":record["channel"], "creation":record["creation"], "position":record["position"], "at_ms":typed::at_ms(record), "text":typed::text(record), "type":"note"});
        waits.retain(|_, wait| wait["_closed_at_ms"].as_u64().is_none_or(|at| at > typed::at_ms(record)));
        let Some(fields) = typed::fields(record, "rail", "lead") else { return item; };
        let (Some(event), Some(subject), Some(link)) = (fields.get("Event"), fields.get("Subject"), fields.get("Link")) else { return item; };
        let Ok(link) = serde_json::from_str::<Value>(link) else { return item; };
        if !EVENTS.contains(&event.as_str()) || !dependencies::valid(&link) { return item; }
        let wait_key = (link[2].as_str().unwrap().to_owned(), link[3].as_u64().unwrap());
        if event == "wait done" && (link[0] != board || link[1] != "rail" || !waits.get(&wait_key).is_some_and(|wait| wait["subject"] == subject.as_str())) { return item; }
        if event == "decided" && !resolve(&link).is_some_and(|answer| answer["sender"] == "lead" && answer["grade"] == "proven" && answer["kind"] == "note" && answer["headers"]["reply_to"].as_array().is_some_and(|r| r.first().is_some_and(|c| c == "questions"))) { return item; }
        if matches!(event.as_str(), "started" | "finished") {
            let Some(fact) = resolve(&link) else { return item; };
            let Some(enrolment) = lifecycle_enrolment(board, event, &link, &fact) else { return item; };
            item["_enrolment"] = json!(enrolment);
            item["_fact_at_ms"] = json!(typed::at_ms(&fact));
        }
        if matches!(event.as_str(), "finished" | "unblocked" | "handed over")
            && let Some(work) = resolve(&link).and_then(|fact| dependencies::identity(&link, &fact)) {
            item["_work"] = work;
        }
        if event == "waits on" && let Some(work) = fields.get("Work link") {
            let Ok(work) = serde_json::from_str::<Value>(work) else { return item; };
            if !dependencies::valid(&work) { return item; }
            if let Some(identity) = resolve(&work).and_then(|fact| dependencies::identity(&work, &fact)) {
                item["_work"] = identity;
            }
            if let Some(peer) = work[0].as_str().filter(|peer| *peer != board) {
                match peers.get(peer) {
                    Some(Some(events)) => {
                        if let Some(at) = events.iter().filter(|event| matches!(event["event"].as_str(), Some("finished" | "unblocked" | "handed over")) && dependencies::matches(&item, event)).filter_map(|event| event["at_ms"].as_u64()).min() {
                            item["_closed"] = json!(true);
                            item["_closed_at_ms"] = json!(at);
                        }
                    }
                    Some(None) => { item["_cannot_check"] = json!(true); }
                    None => {}
                }
            }
            item["work_link"] = work;
        }
        item["type"] = json!("rail"); item["event"] = json!(event); item["subject"] = json!(subject); item["link"] = link;
        if let Some(other) = fields.get("Other board") { item["other_board"] = json!(other); }
        let first = first_lifecycle_fact(event, &item["link"], &mut lifecycle);
        match event.as_str() {
            "finished" | "unblocked" | "handed over" if first => { waits.retain(|_, wait| !dependencies::matches(wait, &item)); },
            "waits on" if item["_closed_at_ms"].as_u64().is_none_or(|at| at > typed::at_ms(record)) => {
                waits.insert((record["creation"].as_str().unwrap_or("").to_owned(), record["position"].as_u64().unwrap_or(0)), item.clone());
            },
            "wait done" => { waits.remove(&wait_key); },
            _ => {}
        }
        item
    }).collect()
}

pub(in crate::cli) fn public_event(mut event: Value) -> Value {
    if let Some(fields) = event.as_object_mut() {
        fields.retain(|key, _| !key.starts_with('_'));
    }
    event
}

pub(in crate::cli) fn first_lifecycle_fact(
    event: &str,
    link: &Value,
    seen: &mut HashSet<(String, String)>,
) -> bool {
    !matches!(event, "started" | "finished") || seen.insert((event.to_owned(), link.to_string()))
}

fn lifecycle_enrolment(board: &str, event: &str, link: &Value, fact: &Value) -> Option<String> {
    if link[0] != board
        || fact["grade"] != "proven"
        || fact["channel"] != link[1]
        || fact["creation"] != link[2]
        || fact["position"] != link[3]
    {
        return None;
    }
    let worker = |id: &str| id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit());
    let sender = fact["sender"].as_str()?;
    if event == "finished" {
        return (worker(sender) && fact["channel"] == "lead" && fact["kind"] == "report")
            .then(|| sender.to_owned());
    }
    if sender == "lead" && fact["kind"] == "task" {
        return fact["channel"]
            .as_str()?
            .strip_prefix("to/")
            .filter(|id| worker(id))
            .map(str::to_owned);
    }
    if worker(sender) {
        return Some(sender.to_owned());
    }
    if fact["channel"] == "lead" && sender == "malakowner" {
        let id = fact["id"]
            .as_str()?
            .strip_prefix("claim-")?
            .strip_suffix("-accepted")?
            .split_once('-')?
            .0;
        return worker(id).then(|| id.to_owned());
    }
    None
}

pub(in crate::cli) fn retained_projection(board: &str, messages: &[Value]) -> Vec<Value> {
    let rails: Vec<_> = messages
        .iter()
        .filter(|m| m["channel"] == "rail")
        .cloned()
        .collect();
    project(board, &rails, |link| {
        (link[0] == board)
            .then(|| {
                messages
                    .iter()
                    .find(|m| {
                        m["channel"] == link[1]
                            && m["creation"] == link[2]
                            && m["position"] == link[3]
                    })
                    .cloned()
            })
            .flatten()
    })
}

pub(in crate::cli) fn since_ms(value: Option<&str>, now: u64) -> Result<u64, Error> {
    let Some(value) = value else {
        return Ok(0);
    };
    if let Some((number, unit)) = value.char_indices().last().map(|(i, _)| value.split_at(i)) {
        let scale = match unit {
            "m" => Some(60_000u64),
            "h" => Some(3_600_000),
            "d" => Some(86_400_000),
            _ => None,
        };
        if let Some(scale) = scale
            && let Ok(number) = number.parse::<u64>()
        {
            return Ok(now.saturating_sub(number.saturating_mul(scale)));
        }
    }
    value.parse().map_err(|_| {
        Error::refused("--since must be epoch milliseconds or an age such as 24h or 7d")
    })
}

pub(in crate::cli) fn view(
    reader: &Reader,
    board: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<Value>, Error> {
    let cutoff = since_ms(since, crate::cli::output::now_ms())?;
    let boards = board.map_or_else(|| reader.boards(), |name| vec![name.to_owned()]);
    let mut all = Vec::new();
    for board in boards {
        let mut seat = reader.open_board(Some(&board))?;
        let messages = records::retained(&seat.config, &mut seat.session, &["rail"])?;
        let projected = reader_projection(reader, &seat.board, &messages);
        all.extend(
            projected
                .into_iter()
                .map(public_event)
                .filter(|event| event["at_ms"].as_u64().unwrap_or(0) >= cutoff),
        );
    }
    all.sort_by(|a, b| {
        b["at_ms"]
            .as_u64()
            .cmp(&a["at_ms"].as_u64())
            .then_with(|| a["board"].as_str().cmp(&b["board"].as_str()))
            .then_with(|| b["position"].as_u64().cmp(&a["position"].as_u64()))
    });
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wait_done_must_name_a_still_open_wait() {
        let record = |event, position, link: Value| {
            let body = format!("Type: rail\nEvent: {event}\nSubject: dependency\nLink: {link}");
            json!({"sender":"lead","grade":"proven","kind":"note","creation":"c","position":position,"body":body.as_bytes()})
        };
        let wait = record("waits on", 1, json!(["b", "lead", "fact", 1]));
        let done = record("wait done", 2, json!(["b", "rail", "c", 1]));
        let duplicate = record("wait done", 3, json!(["b", "rail", "c", 1]));
        let projected = project("b", &[duplicate, done, wait], |_| None);
        assert_eq!(projected[1]["type"], "rail");
        assert_eq!(projected[2]["type"], "note");
    }

    #[test]
    fn lifecycle_links_derive_enrolments_and_unresolvable_links_remain_notes() {
        let id = "a".repeat(32);
        let link = json!(["b", "lead", "c", 1]);
        let fact = json!({"channel":"lead","creation":"c","position":1,"grade":"proven","kind":"report","sender":id,"sent_at":[1,0]});
        let body = format!("Type: rail\nEvent: finished\nSubject: reused name\nLink: {link}");
        let record = json!({"sender":"lead","grade":"proven","kind":"note","body":body.as_bytes()});
        assert_eq!(
            project("b", std::slice::from_ref(&record), |_| None)[0]["type"],
            "note"
        );
        let projected = project("b", &[record], |_| Some(fact.clone()));
        assert_eq!(projected[0]["_enrolment"], id);
        assert_eq!(projected[0]["_fact_at_ms"], 1000);
        let mut accepted = fact.clone();
        accepted["sender"] = json!("malakowner");
        accepted["kind"] = json!("note");
        accepted["id"] = json!(format!("claim-{id}-42-accepted"));
        assert_eq!(
            lifecycle_enrolment("b", "started", &link, &accepted),
            Some(id)
        );
        assert!(lifecycle_enrolment("b", "finished", &link, &accepted).is_none());
    }

    #[test]
    fn decided_without_a_proven_answer_link_stays_a_note() {
        let record = json!({"sender":"lead","grade":"proven","kind":"note","body": b"Type: rail\nEvent: decided\nSubject: choice\nLink: [\"b\",\"questions\",\"c\",1]".as_slice()});
        assert_eq!(project("b", &[record], |_| None)[0]["type"], "note");
    }
}
