//! Project lead-authored rail notes and merge them by time.

use super::{records, seats::Reader, typed};
use malak::Error;
use serde_json::{Value, json};
use std::collections::HashMap;

const EVENTS: &[&str] = &[
    "started",
    "finished",
    "merged",
    "blocked",
    "unblocked",
    "waits on",
    "handed over",
    "decided",
    "lead removed",
    "chief of staff switched",
];

pub(in crate::cli) fn project(
    board: &str,
    messages: &[Value],
    mut resolve: impl FnMut(&Value) -> Option<Value>,
) -> Vec<Value> {
    messages.iter().map(|record| {
        let mut item = json!({"board":board, "channel":record["channel"], "creation":record["creation"], "position":record["position"], "at_ms":typed::at_ms(record), "text":typed::text(record), "type":"note"});
        if let Some(fields) = typed::fields(record, "rail", "lead")
            && let (Some(event), Some(subject), Some(link)) = (fields.get("Event"), fields.get("Subject"), fields.get("Link"))
            && EVENTS.contains(&event.as_str())
            && let Ok(link) = serde_json::from_str::<Value>(link)
            && link.as_array().is_some_and(|link| link.len() == 4)
            && (event != "decided" || resolve(&link).is_some_and(|answer| answer["sender"] == "lead" && answer["grade"] == "proven" && answer["kind"] == "note" && answer["headers"]["reply_to"].as_array().is_some_and(|r| r.first().is_some_and(|c| c == "questions")))) {
                item["type"] = json!("rail"); item["event"] = json!(event); item["subject"] = json!(subject); item["link"] = link;
                if let Some(other) = fields.get("Other board") { item["other_board"] = json!(other); }
        }
        item
    }).collect()
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
        let mut links = HashMap::<String, Option<Value>>::new();
        let projected = project(&seat.board, &messages, |link| {
            let key = link.to_string();
            if let Some(found) = links.get(&key) {
                return found.clone();
            }
            let found = (|| {
                let mut linked = reader.open_board(link[0].as_str()).ok()?;
                let channel = link[1].as_str()?;
                let creation = link[2].as_str()?;
                let position = link[3].as_u64()?;
                let (record, body) = linked
                    .session
                    .get_record_with_body(channel, creation, position, false, true)
                    .ok()??;
                Some(crate::cli::participant::json_message(record.label, &body))
            })();
            links.insert(key, found.clone());
            found
        });
        all.extend(
            projected
                .into_iter()
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
    fn decided_without_a_proven_answer_link_stays_a_note() {
        let record = json!({"sender":"lead","grade":"proven","kind":"note","body": b"Type: rail\nEvent: decided\nSubject: choice\nLink: [\"b\",\"questions\",\"c\",1]".as_slice()});
        assert_eq!(project("b", &[record], |_| None)[0]["type"], "note");
    }
}
