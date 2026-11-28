//! A deterministic pulse computed from facts and an explicit clock.

use super::{rail, records, seats::Reader, typed};
use malak::Error;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use time::{OffsetDateTime, UtcOffset};

pub(in crate::cli) struct Timing {
    pub silent_ms: u64,
    pub stale_ms: u64,
    pub midnight_ms: u64,
}

/// Calls is a projection hook: OR.7 supplies its validated open calls here.
#[expect(
    clippy::too_many_arguments,
    reason = "pure projection names its facts, clock, reachability and configured timing explicitly"
)]
pub(in crate::cli) fn row(
    name: &str,
    messages: &[Value],
    events: &[Value],
    calls: &[Value],
    now: u64,
    reachable: bool,
    overdue: bool,
    timing: &Timing,
) -> Value {
    let mut open = BTreeMap::new();
    let mut waits = BTreeMap::new();
    let mut finished = 0;
    for event in events.iter().filter(|event| event["type"] == "rail") {
        let subject = event["subject"].as_str().unwrap_or("");
        match event["event"].as_str().unwrap_or("") {
            "started" => {
                open.insert(subject, event);
            }
            "finished" => {
                open.remove(subject);
                waits.remove(subject);
                if event["at_ms"].as_u64().unwrap_or(0) >= timing.midnight_ms {
                    finished += 1;
                }
            }
            "waits on" => {
                waits.insert(subject, event);
            }
            "handed over" | "unblocked" => {
                waits.remove(subject);
            }
            _ => {}
        }
    }
    let latest = messages.iter().filter(|m| {
        matches!(
            m["channel"].as_str().unwrap_or(""),
            "lead" | "questions" | "rail" | "briefs"
        ) || m["channel"].as_str().unwrap_or("").starts_with("to/")
            || m["channel"].as_str().unwrap_or("").starts_with("relay/")
    });
    let mut activity = 0;
    let mut lead_activity = messages
        .iter()
        .filter(|record| record["sender"] == "lead" && record["grade"] == "proven")
        .map(typed::at_ms)
        .max()
        .unwrap_or(0);
    for record in latest {
        activity = activity.max(typed::at_ms(record));
        if record["sender"] == "lead" && record["grade"] == "proven" {
            lead_activity = lead_activity.max(typed::at_ms(record));
        }
    }
    let red = calls.iter().find(|call| call["level"] == "red");
    let orange = calls.iter().find(|call| call["level"] == "orange");
    let silent = (!open.is_empty() || !calls.is_empty())
        && now.saturating_sub(lead_activity) >= timing.silent_ms;
    let wait = waits.values().max_by_key(|event| event["at_ms"].as_u64());
    let (colour, reason, blocker) = if !reachable {
        ("black", "not reachable", None)
    } else if silent {
        ("black", "lead silent", None)
    } else if let Some(call) = red {
        ("red", "open red call", Some(call))
    } else if let Some(call) = orange {
        ("yellow", "open orange call", Some(call))
    } else if let Some(wait) = wait {
        ("yellow", "waits on", Some(*wait))
    } else if overdue {
        ("yellow", "handover brief overdue", None)
    } else if !open.is_empty() && now.saturating_sub(activity) >= timing.stale_ms {
        ("yellow", "no recent activity", None)
    } else {
        ("green", "active", None)
    };
    let on = open
        .values()
        .max_by_key(|event| event["at_ms"].as_u64())
        .map_or("idle", |event| event["subject"].as_str().unwrap_or("idle"));
    json!({"lead":name,"colour":colour,"reason":reason,"on":on,"blocker":blocker,"last_activity":activity,"counts":{"started":open.len(),"calls":calls.len(),"finished":finished}})
}

fn open_calls(messages: &[Value]) -> Vec<Value> {
    // Interpret only authored question headers and proven lead call-level notes.
    // The full calls projection can replace this hook without changing row().
    let mut calls = BTreeMap::<(String, u64), Value>::new();
    let mut answered = HashSet::new();
    for message in messages {
        let reply = &message["headers"]["reply_to"];
        if message["sender"] == "lead" && message["grade"] == "proven" && reply[0] == "questions" {
            answered.insert((
                reply[1].as_str().unwrap_or("").to_owned(),
                reply[2].as_u64().unwrap_or(0),
            ));
        }
        if message["channel"] == "questions"
            && message["sender"] == "lead"
            && message["grade"] == "proven"
            && reply.is_null()
        {
            let text = typed::text(message);
            let level = text
                .lines()
                .find_map(|line| line.strip_prefix("Level: "))
                .unwrap_or("blue");
            if ["red", "orange", "blue"].contains(&level) {
                calls.insert((message["creation"].as_str().unwrap_or("").to_owned(), message["position"].as_u64().unwrap_or(0)), json!({"level":level,"link":["questions",message["creation"],message["position"]]}));
            }
        }
        if message["channel"] == "calls"
            && let Some(fields) = typed::fields(message, "call-level", "lead")
            && let (Some(question), Some(level)) = (fields.get("Question"), fields.get("Level"))
            && let Ok(link) = serde_json::from_str::<Value>(question)
            && ["red", "orange", "blue"].contains(&level.as_str())
        {
            calls.insert(
                (
                    link[2].as_str().unwrap_or("").to_owned(),
                    link[3].as_u64().unwrap_or(0),
                ),
                json!({"level":level,"link":link}),
            );
        }
    }
    calls
        .into_iter()
        .filter(|(key, _)| !answered.contains(key))
        .map(|(_, call)| call)
        .collect()
}

pub(in crate::cli) fn view(reader: &Reader, now: u64) -> Result<Vec<Value>, Error> {
    if !reader.chief {
        return Err(Error::refused(
            "pulse is for the chief of staff; leads may read rail for their own work",
        ));
    }
    let roster = reader
        .roster
        .as_ref()
        .ok_or_else(|| Error::refused("no organisation roster"))?;
    let utc = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)
        .map_err(|_| Error::refused("invalid pulse time"))?;
    let local = utc.to_offset(UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC));
    let midnight = local
        .replace_time(time::Time::MIDNIGHT)
        .unix_timestamp_nanos()
        .max(0)
        / 1_000_000;
    let timing = Timing {
        silent_ms: roster.lead_silent_minutes.saturating_mul(60_000),
        stale_ms: roster.work_stale_minutes.saturating_mul(60_000),
        midnight_ms: midnight as u64,
    };
    let mut top = reader.open_board(None)?;
    let ramp = records::retained(&top.config, &mut top.session, &["cos/ramp"])?;
    let candidate = roster
        .candidate
        .as_ref()
        .and_then(|candidate| candidate["id"].as_str());
    let seated = ramp
        .iter()
        .filter(|message| {
            candidate.is_some_and(|id| {
                typed::fields(message, "ramp", id)
                    .is_some_and(|fields| fields.get("Step").is_some_and(|step| step == "seated"))
            })
        })
        .map(typed::at_ms)
        .max();
    let ready = ramp.iter().any(|message| {
        candidate.is_some_and(|id| {
            typed::fields(message, "ramp", id)
                .is_some_and(|fields| fields.get("Step").is_some_and(|step| step == "ready"))
        }) && seated.is_some_and(|at| typed::at_ms(message) >= at)
    });
    let mut rows = Vec::new();
    let mut briefed = 0;
    for (name, lead) in &roster.leads {
        let fetched = if lead.active {
            (|| {
                let mut seat = reader.open_board(Some(name))?;
                records::retained(
                    &seat.config,
                    &mut seat.session,
                    &[
                        "rail",
                        "briefs",
                        "calls",
                        "lead",
                        "questions",
                        "relay/*",
                        "to/*",
                    ],
                )
            })()
        } else {
            Err(Error::refused("removed"))
        };
        let reachable = fetched.is_ok();
        let messages = fetched.unwrap_or_default();
        let handover = seated.is_some_and(|start| {
            messages.iter().any(|record| {
                typed::at_ms(record) >= start
                    && typed::fields(record, "brief", "lead").is_some_and(|fields| {
                        fields.get("Kind").is_some_and(|kind| kind == "handover")
                    })
            })
        });
        briefed += usize::from(handover);
        let overdue = seated.is_some_and(|start| {
            !ready
                && !handover
                && now.saturating_sub(start) >= roster.ramp_minutes.saturating_mul(60_000)
        });
        let rail_messages: Vec<_> = messages
            .iter()
            .filter(|m| m["channel"] == "rail")
            .cloned()
            .collect();
        let events = rail::project(name, &rail_messages, |_| None);
        rows.push(row(
            name,
            &messages,
            &events,
            &open_calls(&messages),
            now,
            reachable,
            overdue,
            &timing,
        ));
    }
    if let Some(start) = seated.filter(|_| !ready) {
        rows.push(json!({"lead":"chief of staff","colour":if now.saturating_sub(start)>=30*60_000 {"yellow"} else {"green"},"reason":if now.saturating_sub(start)>=30*60_000 {"chief of staff ramp-up overdue"} else {"onboarding"},"on":format!("onboarding: {briefed} of {} leads briefed, started {}",roster.leads.values().filter(|lead| lead.active).count(),crate::cli::output::clock(Some(start))),"started":start}));
    }
    if seated.is_none() || ready {
        rows.push(own(reader, now)?);
    }
    Ok(rows)
}

/// The brief view's own-board pulse; this does not grant the pulse API to leads.
pub(in crate::cli) fn own(reader: &Reader, now: u64) -> Result<Value, Error> {
    let mut seat = reader.open_board(None)?;
    let messages = records::retained(
        &seat.config,
        &mut seat.session,
        &[
            "rail",
            "briefs",
            "calls",
            "lead",
            "questions",
            "relay/*",
            "to/*",
        ],
    )?;
    let rails: Vec<_> = messages
        .iter()
        .filter(|m| m["channel"] == "rail")
        .cloned()
        .collect();
    let events = rail::project(&seat.board, &rails, |_| None);
    let utc = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)
        .map_err(|_| Error::refused("invalid pulse time"))?;
    let local = utc.to_offset(UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC));
    let timing = Timing {
        silent_ms: reader
            .roster
            .as_ref()
            .map_or(60, |r| r.lead_silent_minutes)
            .saturating_mul(60_000),
        stale_ms: reader
            .roster
            .as_ref()
            .map_or(30, |r| r.work_stale_minutes)
            .saturating_mul(60_000),
        midnight_ms: (local
            .replace_time(time::Time::MIDNIGHT)
            .unix_timestamp_nanos()
            .max(0)
            / 1_000_000) as u64,
    };
    Ok(row(
        &seat.board,
        &messages,
        &events,
        &open_calls(&messages),
        now,
        true,
        false,
        &timing,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> Timing {
        Timing {
            silent_ms: 3_600_000,
            stale_ms: 1_800_000,
            midnight_ms: 0,
        }
    }
    fn activity(at: u64) -> Value {
        json!({"channel":"lead","sender":"lead","grade":"proven","sent_at":[at/1000,0]})
    }
    fn event(kind: &str, at: u64) -> Value {
        json!({"type":"rail","event":kind,"subject":"build","at_ms":at})
    }

    #[test]
    fn a_silent_lead_is_black_and_hides_a_red_blocker() {
        let result = row(
            "a",
            &[activity(0)],
            &[event("started", 0)],
            &[json!({"level":"red"})],
            3_600_000,
            true,
            false,
            &timing(),
        );
        assert_eq!(result["reason"], "lead silent");
        assert!(result["blocker"].is_null());
    }
    #[test]
    fn an_open_red_call_is_red() {
        assert_eq!(
            row(
                "a",
                &[activity(0)],
                &[],
                &[json!({"level":"red"})],
                0,
                true,
                false,
                &timing()
            )["colour"],
            "red"
        );
    }
    #[test]
    fn an_open_orange_call_is_yellow() {
        assert_eq!(
            row(
                "a",
                &[activity(0)],
                &[],
                &[json!({"level":"orange"})],
                0,
                true,
                false,
                &timing()
            )["reason"],
            "open orange call"
        );
    }
    #[test]
    fn an_open_dependency_is_yellow() {
        assert_eq!(
            row(
                "a",
                &[activity(0)],
                &[event("waits on", 0)],
                &[],
                0,
                true,
                false,
                &timing()
            )["reason"],
            "waits on"
        );
    }
    #[test]
    fn starting_work_does_not_close_a_dependency() {
        assert_eq!(
            row(
                "a",
                &[activity(0)],
                &[event("waits on", 0), event("started", 1)],
                &[],
                1,
                true,
                false,
                &timing()
            )["reason"],
            "waits on"
        );
    }
    #[test]
    fn finishing_the_subject_closes_its_dependency() {
        assert_eq!(
            row(
                "a",
                &[activity(0)],
                &[event("waits on", 0), event("finished", 1)],
                &[],
                1,
                true,
                false,
                &timing()
            )["colour"],
            "green"
        );
    }
    #[test]
    fn stale_open_work_is_yellow_before_the_silence_window() {
        assert_eq!(
            row(
                "a",
                &[activity(0)],
                &[event("started", 0)],
                &[],
                1_800_000,
                true,
                false,
                &timing()
            )["reason"],
            "no recent activity"
        );
    }
    #[test]
    fn idle_without_calls_is_green_even_when_old() {
        assert_eq!(
            row("a", &[], &[], &[], 9_000_000, true, false, &timing())["colour"],
            "green"
        );
    }
    #[test]
    fn pulse_counts_only_finishes_since_the_given_local_midnight() {
        let timing = Timing {
            midnight_ms: 1,
            ..timing()
        };
        assert_eq!(
            row(
                "a",
                &[],
                &[event("finished", 0), event("finished", 1)],
                &[],
                1,
                true,
                false,
                &timing
            )["counts"]["finished"],
            1
        );
    }
    #[test]
    fn unreadable_board_wins_over_red_call() {
        let timing = Timing {
            silent_ms: 60_000,
            stale_ms: 30_000,
            midnight_ms: 0,
        };
        assert_eq!(
            row(
                "a",
                &[],
                &[],
                &[json!({"level":"red"})],
                0,
                false,
                false,
                &timing
            )["reason"],
            "not reachable"
        );
    }
    #[test]
    fn pulse_is_deterministic_for_state_and_clock() {
        let timing = Timing {
            silent_ms: 60_000,
            stale_ms: 30_000,
            midnight_ms: 0,
        };
        assert_eq!(
            row("a", &[], &[], &[], 0, true, false, &timing),
            row("a", &[], &[], &[], 0, true, false, &timing)
        );
    }
}
