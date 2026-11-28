//! Resolve dependency work through the caller's own authenticated read seats.

use super::{Reader, project_with_peers, typed};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(super) type Peers = HashMap<String, Option<Vec<Value>>>;

pub(super) fn valid(link: &Value) -> bool {
    link.as_array().is_some_and(|link| link.len() == 4)
        && (0..3).all(|index| link[index].is_string())
        && link[3].as_u64().is_some_and(|position| position > 0)
}

pub(super) fn identity(link: &Value, fact: &Value) -> Option<Value> {
    if !valid(link)
        || fact["grade"] != "proven"
        || fact["channel"] != link[1]
        || fact["creation"] != link[2]
        || fact["position"] != link[3]
    {
        return None;
    }
    let board = link[0].as_str()?;
    let reply = &fact["headers"]["reply_to"];
    if fact["sender"] == "lead"
        && fact["kind"] == "note"
        && reply[0] == "questions"
        && reply[1].is_string()
        && reply[2].as_u64().is_some_and(|position| position > 0)
    {
        return Some(json!(["fact", board, "questions", reply[1], reply[2]]));
    }
    if fact["channel"] != "questions"
        && let Some(worker) = super::lifecycle_enrolment(board, "started", link, fact)
    {
        return Some(json!(["worker", board, worker]));
    }
    Some(json!(["fact", board, link[1], link[2], link[3]]))
}

pub(super) fn matches(wait: &Value, event: &Value) -> bool {
    !wait["_work"].is_null() && wait["_work"] == event["_work"]
}

pub(in crate::cli) fn open_waits(events: &[Value]) -> Vec<Value> {
    let mut waits = BTreeMap::new();
    let mut lifecycle = HashSet::new();
    for event in events.iter().filter(|event| event["type"] == "rail") {
        if !super::first_lifecycle_fact(
            event["event"].as_str().unwrap_or(""),
            &event["link"],
            &mut lifecycle,
        ) {
            continue;
        }
        match event["event"].as_str() {
            Some("waits on") if event["_closed"] != true => {
                waits.insert(
                    (
                        event["board"].to_string(),
                        event["creation"].to_string(),
                        event["position"].to_string(),
                    ),
                    event.clone(),
                );
            }
            Some("wait done") => {
                waits.remove(&(
                    event["link"][0].to_string(),
                    event["link"][2].to_string(),
                    event["link"][3].to_string(),
                ));
            }
            Some("finished" | "unblocked" | "handed over") => {
                waits.retain(|_, wait| !matches(wait, event));
            }
            _ => {}
        }
    }
    waits.into_values().collect()
}

fn links(messages: &[Value], field: &str) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|message| {
            let fields = typed::fields(message, "rail", "lead")?;
            let link = serde_json::from_str(fields.get(field)?).ok()?;
            valid(&link).then_some(link)
        })
        .collect()
}

fn fetch(reader: &Reader, addresses: Vec<Value>, facts: &mut HashMap<String, Option<Value>>) {
    for link in addresses {
        let key = link.to_string();
        if facts.contains_key(&key) {
            continue;
        }
        let found = (|| {
            let mut seat = reader.open_board(link[0].as_str()).ok()?;
            let (record, body) = seat
                .session
                .get_record_with_body(
                    link[1].as_str()?,
                    link[2].as_str()?,
                    link[3].as_u64()?,
                    false,
                    true,
                )
                .ok()??;
            Some(crate::cli::participant::json_message(record.label, &body))
        })();
        facts.insert(key, found);
    }
}

pub(in crate::cli) fn read(reader: &Reader, board: &str, messages: &[Value]) -> Vec<Value> {
    let mut facts: HashMap<_, _> = messages
        .iter()
        .map(|fact| {
            (
                json!([board, fact["channel"], fact["creation"], fact["position"]]).to_string(),
                Some(fact.clone()),
            )
        })
        .collect();
    let work = links(messages, "Work link");
    fetch(reader, links(messages, "Link"), &mut facts);
    fetch(reader, work.clone(), &mut facts);
    let mut peers = Peers::new();
    for link in work {
        let Some(peer) = link[0].as_str().filter(|peer| *peer != board) else {
            continue;
        };
        if peers.contains_key(peer) {
            continue;
        }
        let messages = (|| {
            let mut seat = reader.open_board(Some(peer)).ok()?;
            super::records::retained(&seat.config, &mut seat.session, &["rail"]).ok()
        })();
        let projected = messages.map(|messages| {
            fetch(reader, links(&messages, "Link"), &mut facts);
            super::project(peer, &messages, |link| {
                facts.get(&link.to_string()).cloned().flatten()
            })
        });
        peers.insert(peer.to_owned(), projected);
    }
    let rails: Vec<_> = messages
        .iter()
        .filter(|message| message["channel"] == "rail")
        .cloned()
        .collect();
    project_with_peers(
        board,
        &rails,
        |link| facts.get(&link.to_string()).cloned().flatten(),
        &peers,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_and_its_question_resolve_to_the_same_work() {
        let question = json!(["b", "questions", "c", 1]);
        let answer = json!(["b", "questions", "c", 2]);
        let original = json!({"channel":"questions","creation":"c","position":1,"sender":"0123456789abcdef0123456789abcdef","kind":"note","grade":"proven"});
        let reply = json!({"channel":"questions","creation":"c","position":2,"sender":"lead","kind":"note","grade":"proven","headers":{"reply_to":["questions","c",1]}});
        assert_eq!(identity(&question, &original), identity(&answer, &reply));
    }

    #[test]
    fn a_subject_match_never_closes_an_unlinked_wait() {
        for kind in ["finished", "unblocked", "handed over"] {
            let wait = json!({"type":"rail","event":"waits on","subject":"build","board":"b","creation":"c","position":1});
            let closure = json!({"type":"rail","event":kind,"subject":"build","link":["b","lead","c",2],"_work":["worker","b","enrolment"]});
            assert_eq!(open_waits(&[wait, closure]).len(), 1);
        }
    }

    #[test]
    fn linked_waits_match_enrolments_instead_of_display_subjects() {
        let wait = json!({"type":"rail","event":"waits on","subject":"consumer","board":"b","creation":"c","position":1,"_work":["worker","b","new"]});
        let old = json!({"type":"rail","event":"finished","subject":"consumer","link":["b","lead","c",2],"_work":["worker","b","old"]});
        assert_eq!(open_waits(&[wait.clone(), old.clone()]).len(), 1);
        let matching = json!({"type":"rail","event":"finished","subject":"different label","link":["b","lead","c",3],"_work":["worker","b","new"]});
        assert!(open_waits(&[wait, old, matching]).is_empty());
    }

    #[test]
    fn a_later_peer_completion_does_not_invalidate_an_earlier_explicit_close() {
        let work = json!(["peer", "questions", "q", 1]);
        let fact = json!({"channel":"questions","creation":"q","position":1,"grade":"proven","sender":"lead","kind":"note"});
        let note = |event: &str, position: u64, link: Value, extra: String| {
            let body =
                format!("Type: rail\nEvent: {event}\nSubject: dependency\nLink: {link}\n{extra}");
            json!({"sender":"lead","grade":"proven","kind":"note","channel":"rail","creation":"r","position":position,"sent_at":[position,0],"body":body.as_bytes()})
        };
        let wait = note(
            "waits on",
            1,
            json!(["b", "lead", "record", 1]),
            format!("Work link: {work}"),
        );
        let done = note("wait done", 2, json!(["b", "rail", "r", 1]), String::new());
        for (at, expected) in [(1_500, "note"), (3_000, "rail")] {
            let closure =
                json!({"event":"unblocked","at_ms":at,"_work":["fact","peer","questions","q",1]});
            let peers = HashMap::from([("peer".to_owned(), Some(vec![closure]))]);
            let events = project_with_peers(
                "b",
                &[wait.clone(), done.clone()],
                |link| (link == &work).then(|| fact.clone()),
                &peers,
            );
            assert_eq!(events[1]["type"], expected);
        }
    }
}
