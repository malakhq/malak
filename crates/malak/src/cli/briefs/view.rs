//! A minimal linked brief view; screen layout is specified separately.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;

fn link(value: &Value) -> String {
    if let Some(parts) = value.as_array() {
        return parts
            .iter()
            .map(|part| {
                part.as_str()
                    .map_or_else(|| part.to_string(), str::to_owned)
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    format!(
        "{} {} {} #{}",
        value["board"].as_str().unwrap_or(""),
        value["channel"].as_str().unwrap_or(""),
        value["creation"].as_str().unwrap_or(""),
        value["position"]
    )
}

pub(super) fn text(value: &Value) -> String {
    let mut text = String::new();
    for item in value["items"].as_array().into_iter().flatten() {
        if let Some(title) = item["title"].as_str() {
            let _ = writeln!(
                text,
                "{} · {} · {title}",
                item["lead"].as_str().unwrap_or(""),
                item["kind"].as_str().unwrap_or("")
            );
            let _ = writeln!(text, "  {}", link(&item["link"]));
        } else if item["section"] == "bottom line" || item["section"] == "unreadable" {
            let _ = writeln!(
                text,
                "{} · {} · {}",
                item["lead"]
                    .as_str()
                    .or_else(|| item["board"].as_str())
                    .unwrap_or(""),
                item["colour"].as_str().unwrap_or(""),
                item["reason"].as_str().unwrap_or("")
            );
            if !item["link"].is_null() {
                let _ = writeln!(text, "  {}", link(&item["link"]));
            }
        } else if let Some(subject) = item["subject"].as_str() {
            let _ = writeln!(
                text,
                "{}: {subject}\n  {}",
                item["section"].as_str().unwrap_or(""),
                link(&item["link"])
            );
        } else if let Some(note) = item["note"].as_str() {
            let _ = writeln!(text, "{note}");
        } else if item["body"].is_string() {
            let _ = writeln!(text, "Source: {}", link(&item["link"]));
        }
        if let Some(status) = item["status"]
            .as_str()
            .filter(|status| *status != "current")
        {
            let _ = writeln!(
                text,
                "  {status}; replaced by {}",
                link(&item["replaced_by"])
            );
        }
        if let Some(body) = item["body"].as_str() {
            text.push_str(body);
            if !body.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    if value["omitted"].as_u64().unwrap_or(0) > 0 {
        let _ = writeln!(text, "{} item(s) left out.", value["omitted"]);
    }
    if let Some(next) = value["next"].as_str() {
        let _ = writeln!(text, "Next: {next}");
    }
    if text.is_empty() {
        text.push_str("No briefs.\n");
    }
    text
}

fn section(mut item: Value, section: &str) -> Value {
    item["section"] = section.into();
    crate::cli::org::rail::public_event(item)
}

pub(super) fn items(
    briefs: Vec<Value>,
    mut rail: Vec<Value>,
    pulse: Option<Value>,
    calls: Option<Vec<Value>>,
    cutoff: u64,
) -> Vec<Value> {
    let mut items = Vec::new();
    items.extend(pulse.map(|pulse| section(pulse, "bottom line")));
    rail.sort_by_key(|event| (event["at_ms"].as_u64(), event["position"].as_u64()));
    let waits = crate::cli::org::rail::open_waits(&rail);
    let mut stuck = BTreeMap::new();
    for event in rail {
        let subject = event["subject"].as_str().unwrap_or("").to_owned();
        match event["event"].as_str() {
            Some("finished") => {
                stuck.remove(&subject);
                if event["at_ms"].as_u64().unwrap_or(0) >= cutoff {
                    items.push(section(event, "finished"));
                }
            }
            Some("blocked") => {
                stuck.insert(subject, event);
            }
            Some("unblocked" | "handed over") => {
                stuck.remove(&subject);
            }
            _ => (),
        }
    }
    items.extend(
        stuck
            .into_values()
            .chain(waits)
            .map(|event| section(event, "stuck")),
    );
    match calls {
        Some(calls) => items.extend(calls.into_iter().map(|call| section(call, "open calls"))),
        None => items.push(serde_json::json!({"section":"open calls", "items":[],
            "note":"calls: not available in this build"})),
    }
    items.extend(briefs.into_iter().map(|brief| section(brief, "briefs")));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finished_work_keeps_its_source_link() {
        let link = json!({"board":"build","channel":"rail","creation":"key","position":4});
        let rows = items(
            vec![],
            vec![json!({"event":"finished","link":link})],
            None,
            Some(vec![]),
            0,
        );
        assert_eq!(rows[0]["section"], "finished");
        assert_eq!(rows[0]["link"], link);
    }

    #[test]
    fn a_pulse_is_the_bottom_line_when_available() {
        let rows = items(
            vec![],
            vec![],
            Some(json!({"colour":"green"})),
            Some(vec![]),
            0,
        );
        assert_eq!(rows[0]["section"], "bottom line");
    }

    #[test]
    fn blue_calls_are_in_the_digest() {
        let rows = items(
            vec![],
            vec![],
            None,
            Some(vec![json!({"level":"blue","link":{"position":1}})]),
            0,
        );
        assert_eq!(rows[0]["section"], "open calls");
        assert_eq!(rows[0]["level"], "blue");
    }

    #[test]
    fn an_unblocked_subject_is_no_longer_stuck() {
        let rows = items(
            vec![],
            vec![
                json!({"event":"blocked","subject":"build","at_ms":1}),
                json!({"event":"unblocked","subject":"build","at_ms":2}),
            ],
            None,
            Some(vec![]),
            0,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn the_time_window_applies_to_finished_work() {
        let rows = items(
            vec![],
            vec![json!({"event":"finished","subject":"build","at_ms":1})],
            None,
            Some(vec![]),
            2,
        );
        assert!(rows.is_empty());
    }
}
