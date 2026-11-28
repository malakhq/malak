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

/// Calls is a projection hook supplied with validated open calls.
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
    let waits = rail::open_waits(events);
    let mut finished = 0;
    let mut lifecycle = HashSet::new();
    for event in events.iter().filter(|event| event["type"] == "rail") {
        let kind = event["event"].as_str().unwrap_or("");
        if matches!(kind, "started" | "finished") && event["_enrolment"].as_str().is_none() {
            continue;
        }
        if !rail::first_lifecycle_fact(kind, &event["link"], &mut lifecycle) {
            continue;
        }
        match kind {
            "started" => {
                if let Some(enrolment) = event["_enrolment"].as_str() {
                    open.insert(enrolment, event);
                }
            }
            "finished" => {
                let Some(enrolment) = event["_enrolment"].as_str() else {
                    continue;
                };
                open.remove(enrolment);
                if event["_fact_at_ms"].as_u64().unwrap_or(0) >= timing.midnight_ms {
                    finished += 1;
                }
            }
            _ => {}
        }
    }
    let latest = messages.iter().filter(|m| {
        matches!(
            m["channel"].as_str().unwrap_or(""),
            "lead" | "questions" | "rail" | "briefs" | "calls"
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
    let black = calls.iter().find(|call| call["pulse"] == "black");
    let red = calls
        .iter()
        .find(|call| call["pulse"] == "red" || call["pulse"].is_null() && call["level"] == "red");
    let orange = calls.iter().find(|call| {
        call["pulse"] == "yellow" || call["pulse"].is_null() && call["level"] == "orange"
    });
    let silent = (!open.is_empty() || !calls.is_empty())
        && now.saturating_sub(lead_activity) >= timing.silent_ms;
    let wait = waits.iter().max_by_key(|event| event["at_ms"].as_u64());
    let (colour, reason, blocker) = if !reachable {
        ("black", "not reachable", None)
    } else if silent {
        ("black", "lead silent", None)
    } else if let Some(call) = black {
        ("black", "open call", Some(call))
    } else if let Some(call) = red {
        ("red", "open red call", Some(call))
    } else if let Some(call) = orange {
        ("yellow", "open orange call", Some(call))
    } else if let Some(wait) = wait {
        (
            "yellow",
            if wait["_cannot_check"] == true {
                "waits on; cannot check: board unreadable"
            } else {
                "waits on"
            },
            Some(wait),
        )
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
    json!({"lead":name,"colour":colour,"reason":reason,"on":on,"blocker":blocker.cloned().map(rail::public_event),"last_activity":activity,"counts":{"started":open.len(),"calls":calls.len(),"finished":finished}})
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
    let midnight = midnight_ms(now, |at| UtcOffset::local_offset_at(at).ok())?;
    let timing = Timing {
        silent_ms: roster.lead_silent_minutes.saturating_mul(60_000),
        stale_ms: roster.work_stale_minutes.saturating_mul(60_000),
        midnight_ms: midnight,
    };
    let mut top = reader.open_board(None)?;
    let ramp = records::retained(&top.config, &mut top.session, &["top/ramp", "cos/ramp"])?;
    let (seated, ready) = ramp_status(
        roster
            .candidate
            .as_ref()
            .and_then(|candidate| candidate["id"].as_str()),
        &ramp,
    );
    let calls = super::calls::load(reader)?;
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
        let handover = seated.is_some_and(|start| handover_since(&messages, start));
        briefed += usize::from(handover);
        let overdue = seated.is_some_and(|start| {
            !ready
                && !handover
                && now.saturating_sub(start) >= roster.ramp_minutes.saturating_mul(60_000)
        });
        let events = rail::reader_projection(reader, name, &messages);
        rows.push(row(
            name,
            &messages,
            &events,
            &calls.open(Some(name)),
            now,
            reachable,
            overdue,
            &timing,
        ));
    }
    let mut chief = own(reader, now)?;
    if let Some(start) = seated.filter(|_| !ready) {
        chief = ramp_row(
            chief,
            start,
            now,
            briefed,
            roster.leads.values().filter(|lead| lead.active).count(),
        );
    }
    rows.push(chief);
    Ok(rows)
}

fn ramp_status(candidate: Option<&str>, ramp: &[Value]) -> (Option<u64>, bool) {
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
    (seated, ready)
}

fn handover_since(messages: &[Value], start: u64) -> bool {
    messages.iter().any(|record| {
        typed::at_ms(record) >= start
            && typed::fields(record, "brief", "lead")
                .is_some_and(|fields| fields.get("Kind").is_some_and(|kind| kind == "handover"))
    })
}

fn own_handover_overdue(reader: &Reader, messages: &[Value], now: u64) -> Result<bool, Error> {
    let Some(roster) = reader
        .roster
        .as_ref()
        .filter(|roster| !reader.chief && roster.candidate.is_some())
    else {
        return Ok(false);
    };
    let top_name =
        crate::cli::board_names::name(&crate::cli::board_group::Entry::read(roster.top.clone()));
    let mut top = reader.open_board(Some(&top_name))?;
    let ramp = records::retained(&top.config, &mut top.session, &["top/ramp", "cos/ramp"])?;
    let (seated, ready) = ramp_status(
        roster
            .candidate
            .as_ref()
            .and_then(|candidate| candidate["id"].as_str()),
        &ramp,
    );
    Ok(seated.is_some_and(|start| {
        !ready
            && !handover_since(messages, start)
            && now.saturating_sub(start) >= roster.ramp_minutes.saturating_mul(60_000)
    }))
}

fn ramp_row(mut chief: Value, start: u64, now: u64, briefed: usize, total: usize) -> Value {
    let overdue = now.saturating_sub(start) >= 30 * 60_000;
    chief["lead"] = json!("chief of staff");
    chief["on"] = json!(format!(
        "onboarding: {briefed} of {total} leads briefed, started {}{}",
        crate::cli::output::clock(Some(start)),
        if overdue {
            "; chief of staff ramp-up overdue"
        } else {
            ""
        }
    ));
    if chief["colour"] == "green" {
        chief["colour"] = json!(if overdue { "yellow" } else { "green" });
        chief["reason"] = json!(if overdue {
            "chief of staff ramp-up overdue"
        } else {
            "onboarding"
        });
    }
    chief
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
    let events = rail::reader_projection(reader, &seat.board, &messages);
    let calls = super::calls::load(reader)?;
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
        midnight_ms: midnight_ms(now, |at| UtcOffset::local_offset_at(at).ok())?,
    };
    let ramp = own_handover_overdue(reader, &messages, now);
    let mut result = row(
        &seat.board,
        &messages,
        &events,
        &calls.open(Some(&seat.board)),
        now,
        true,
        ramp.as_ref().copied().unwrap_or(false),
        &timing,
    );
    if ramp.is_err() {
        result["on"] = json!(format!(
            "{}; note: top-board ramp status unavailable",
            result["on"].as_str().unwrap_or("idle")
        ));
    }
    Ok(result)
}

fn midnight_ms(
    now: u64,
    offset_at: impl Fn(OffsetDateTime) -> Option<UtcOffset>,
) -> Result<u64, Error> {
    let utc = OffsetDateTime::from_unix_timestamp_nanos(i128::from(now) * 1_000_000)
        .map_err(|_| Error::refused("invalid pulse time"))?;
    let offset = offset_at(utc).unwrap_or(UtcOffset::UTC);
    let midnight = utc.to_offset(offset).date().midnight();
    let candidate = midnight.assume_offset(offset);
    // An offset change later today must not move the start of the day.
    let midnight_offset = offset_at(candidate).unwrap_or(offset);
    Ok((midnight
        .assume_offset(midnight_offset)
        .unix_timestamp_nanos()
        .max(0)
        / 1_000_000) as u64)
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
        json!({"type":"rail","event":kind,"subject":"build","at_ms":at,"_enrolment":"worker","_fact_at_ms":at,"link":["b","lead","c",at+1]})
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
    fn a_call_message_is_activity_while_work_is_open() {
        let now = 1_801_000;
        let message =
            json!({"channel":"calls","sender":"lead","grade":"proven","sent_at":[now / 1000,0]});
        let result = row(
            "a",
            &[message],
            &[event("started", 0)],
            &[],
            now,
            true,
            false,
            &timing(),
        );
        assert_eq!(result["colour"], "green");
        assert_eq!(result["last_activity"], now);
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
    fn finishing_the_same_subject_leaves_an_unlinked_dependency_open() {
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
            "yellow"
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
    fn an_old_exit_replay_cannot_close_a_new_enrolment_with_the_same_name() {
        let old = event("started", 1);
        let mut new = event("started", 2);
        new["_enrolment"] = json!("replacement");
        let exit = event("finished", 3);
        let result = row(
            "a",
            &[],
            &[old, new, exit.clone(), exit],
            &[],
            4,
            true,
            false,
            &timing(),
        );
        assert_eq!(result["counts"]["started"], 1);
        assert_eq!(result["counts"]["finished"], 1);
        assert_eq!(result["on"], "build");
    }

    #[test]
    fn replayed_lifecycle_facts_do_not_reopen_or_close_another_run() {
        let start = event("started", 0);
        let finish = event("finished", 1);
        let next = event("started", 2);
        for (events, expected) in [
            (vec![start.clone(), finish.clone(), start.clone()], 0),
            (vec![start, finish.clone(), next, finish], 1),
        ] {
            let result = row("a", &[], &events, &[], 3, true, false, &timing());
            assert_eq!(result["counts"]["started"], expected);
            assert_eq!(result["counts"]["finished"], 1);
        }
    }

    #[test]
    fn ramp_overdue_progress_keeps_higher_priority_colours_and_normal_fields() {
        for (colour, expected) in [
            ("green", "yellow"),
            ("yellow", "yellow"),
            ("red", "red"),
            ("black", "black"),
        ] {
            let original = json!({"lead":"top","colour":colour,"reason":"existing","counts":{"calls":1},"blocker":{"level":"red"},"last_activity":1});
            let result = ramp_row(original, 1, 30 * 60_000 + 1, 1, 2);
            assert_eq!(result["colour"], expected);
            assert_eq!(result["counts"]["calls"], 1);
            assert_eq!(result["last_activity"], 1);
            assert!(
                result["on"]
                    .as_str()
                    .unwrap()
                    .contains("chief of staff ramp-up overdue")
            );
        }
    }

    #[test]
    fn midnight_uses_its_own_offset_when_the_offset_changes_later_today() {
        let day = 86_400i64;
        let now = ((day + 12 * 3_600) * 1000) as u64;
        for (before, after, expected) in [(0, 3_600, day), (3_600, 0, day - 3_600)] {
            let result = midnight_ms(now, |at| {
                UtcOffset::from_whole_seconds(if at.unix_timestamp() < day + 3_600 {
                    before
                } else {
                    after
                })
                .ok()
            })
            .unwrap();
            assert_eq!(result, expected as u64 * 1000);
        }
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
