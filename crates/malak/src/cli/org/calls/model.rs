//! Calls are facts, never authority supplied by the writer of a record.

use super::{super::typed, names, settings::Settings};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(in crate::cli) struct Link(pub String, pub String, pub String, pub u64);

impl Link {
    pub fn of(board: &str, record: &Value) -> Option<Self> {
        Some(Self(
            board.into(),
            record["channel"].as_str()?.into(),
            record["creation"].as_str()?.into(),
            record["position"].as_u64()?,
        ))
    }
    pub fn parse(value: &str) -> Option<Self> {
        let link: Self = serde_json::from_str(value).ok()?;
        (!link.0.is_empty() && !link.1.is_empty() && !link.2.is_empty() && link.3 > 0)
            .then_some(link)
    }
    pub fn question(&self) -> bool {
        self.1 == names::QUESTION_CHANNEL
    }
}

fn field(text: &str, name: &str) -> Option<String> {
    text.split("\n\n")
        .next()
        .unwrap_or("")
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")).map(str::to_owned))
}

pub(super) fn ordinary_answer(text: &str, top_active: bool) -> bool {
    match text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("Decided by: "))
    {
        Some("lead") => true,
        Some("owner" | "owner, recorded by lead, no top lead") => !top_active,
        Some(_) => false,
        None => true, // Unattributed answers predate the position conventions.
    }
}

fn reply(board: &str, record: &Value) -> Option<Link> {
    let reply = &record["headers"]["reply_to"];
    Some(Link(
        board.into(),
        reply[0].as_str()?.into(),
        reply[1].as_str()?.into(),
        reply[2].as_u64()?,
    ))
}

#[derive(Default)]
pub(in crate::cli) struct Projection {
    /// Includes closed calls so record validation and readers use the same facts.
    pub calls: Vec<Value>,
    pub notes: Vec<Value>,
    pub answers: BTreeMap<Link, Value>,
    pub decisions: BTreeMap<Link, Value>,
    groups: BTreeMap<Link, BTreeSet<Link>>,
}

impl Projection {
    pub fn involves(&self, question: &Link, board: &str) -> bool {
        question.0 == board
            || self
                .groups
                .get(question)
                .is_some_and(|group| group.iter().any(|link| link.0 == board))
    }

    pub fn decisions_for(&self, question: &Link) -> Vec<&Value> {
        let mut notes: Vec<_> = self
            .decisions
            .values()
            .filter(|note| {
                serde_json::from_value::<Link>(note["question"].clone()).is_ok_and(|target| {
                    target == *question
                        || self
                            .groups
                            .get(question)
                            .is_some_and(|group| group.contains(&target))
                })
            })
            .collect();
        // All decisions share the top calls creation. Its positions establish
        // recency even when the wall clock moves backwards.
        notes.sort_by_key(|note| note["link"][3].as_u64());
        notes
    }

    pub fn decision_for(&self, question: &Link, decision: &Link) -> Option<&Value> {
        let note = self.decisions.get(decision)?;
        let target: Link = serde_json::from_value(note["question"].clone()).ok()?;
        (target == *question
            || self
                .groups
                .get(question)
                .is_some_and(|group| group.contains(&target)))
        .then_some(note)
    }
    pub fn recorded_decision(&self, question: &Link, text: &str) -> Option<&Value> {
        let mut links = text
            .split("\n\n")
            .next()?
            .lines()
            .filter_map(|line| line.strip_prefix("Decision: "));
        let link = Link::parse(links.next()?)?;
        if links.next().is_some() {
            return None;
        }
        let note = self.decision_for(question, &link)?;
        let attribution = if note["decided_by"] == names::TOP {
            names::TOP_RECORDED
        } else {
            names::OWNER_RECORDED
        };
        text.starts_with(&format!("Decided by: {attribution}\n"))
            .then_some(note)
    }

    pub fn open(&self, board: Option<&str>) -> Vec<Value> {
        self.calls
            .iter()
            .filter(|call| {
                call["open"] == true
                    && board.is_none_or(|board| {
                        call["askers"].as_array().is_some_and(|askers| {
                            askers
                                .iter()
                                .any(|asker| asker["link"][0] == board && asker["open"] == true)
                        })
                    })
            })
            .cloned()
            .map(|mut call| {
                if let Some(board) = board {
                    let own = call["askers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|asker| asker["link"][0] == board && asker["open"] == true)
                        .max_by_key(|asker| asker["severity"].as_u64())
                        .cloned();
                    if let Some(own) = own {
                        for (field, source) in [
                            ("level", "effective"),
                            ("proposed", "proposed"),
                            ("delivery", "delivery"),
                            ("pulse", "pulse"),
                            ("severity", "severity"),
                        ] {
                            call[field] = own[source].clone();
                        }
                    }
                }
                call
            })
            .collect()
    }
}

/// All inputs are obtained through the reading client's own authorised seats.
/// The top board's identity comes from the owner roster, never a message line.
#[cfg(test)]
pub(in crate::cli) fn project(
    boards: &BTreeMap<String, Vec<Value>>,
    top: Option<&str>,
    candidate: Option<&str>,
) -> Projection {
    let options = boards
        .keys()
        .map(|board| (board.clone(), Settings::default()))
        .collect();
    project_with_settings(boards, top, candidate, top, &options)
}

pub(in crate::cli) fn project_with_settings(
    boards: &BTreeMap<String, Vec<Value>>,
    top: Option<&str>,
    candidate: Option<&str>,
    ready_board: Option<&str>,
    options: &BTreeMap<String, Settings>,
) -> Projection {
    let mut result = Projection::default();
    let mut questions = BTreeMap::new();
    let mut proposals = BTreeMap::<Link, Vec<Value>>::new();
    let mut levels = BTreeMap::<Link, Vec<Value>>::new();
    let mut merges = Vec::new();
    let mut honoured = BTreeSet::new();
    for (board, messages) in boards {
        for record in messages {
            let Some(link) = Link::of(board, record) else {
                continue;
            };
            let question_reply = reply(board, record).is_some_and(|link| link.question());
            if link.question()
                && !question_reply
                && record["kind"] == "note"
                && record["grade"] == "proven"
            {
                questions.insert(link.clone(), record.clone());
                result.groups.insert(link.clone(), BTreeSet::from([link]));
            }
        }
    }
    for (board, messages) in boards {
        for record in messages {
            if record["channel"] != names::CHANNEL {
                continue;
            }
            let Some(source) = Link::of(board, record) else {
                continue;
            };
            if let Some(fields) = typed::fields(record, names::LEVEL, names::LEAD) {
                if let (Some(target), Some(level)) = (
                    fields.get("Link").and_then(|link| Link::parse(link)),
                    fields.get("Level").filter(|level| !level.is_empty()),
                ) && questions.contains_key(&target)
                    && (target.0 == *board || top == Some(board))
                {
                    let item = json!({"link":source,"level":level,"topic":fields.get("Topic"),"at_ms":typed::at_ms(record)});
                    if top == Some(board) {
                        levels.entry(target).or_default().push(item);
                    } else {
                        proposals.entry(target).or_default().push(item);
                    }
                    honoured.insert(source.clone());
                }
            } else if let Some(fields) = typed::fields(record, names::MERGE, names::LEAD) {
                if let Some(links) = fields
                    .get("Links")
                    .and_then(|links| serde_json::from_str::<Vec<Link>>(links).ok())
                    && links.len() >= 2
                    && links.iter().all(|link| {
                        questions.contains_key(link) && (link.0 == *board || top == Some(board))
                    })
                {
                    merges.push(links);
                    honoured.insert(source.clone());
                }
            } else if top == Some(board) {
                for (kind, attribution) in [
                    (names::DECISION, names::TOP),
                    (names::DECISION, names::OWNER_RELAY),
                ] {
                    if let Some(fields) = typed::fields(record, kind, names::LEAD)
                        && fields.get("Decided by").is_some_and(|by| by == attribution)
                        && let Some(target) = fields.get("Link").and_then(|link| Link::parse(link))
                        && questions.contains_key(&target)
                    {
                        let text = typed::text(record);
                        let body = text.split_once("\n\n").map_or("", |(_, body)| body);
                        result.decisions.insert(source.clone(), json!({"link":source,"question":target,"type":kind,"decided_by":attribution,"body":body,"text":text,"at_ms":typed::at_ms(record)}));
                        honoured.insert(source.clone());
                    }
                }
            }
            if !honoured.contains(&source) {
                result.notes.push(json!({"type":"note","board":board,"channel":names::CHANNEL,"creation":source.2,"position":source.3,"link":source,"text":typed::text(record),"at_ms":typed::at_ms(record)}));
            }
        }
    }
    for links in merges {
        let mut group = BTreeSet::new();
        for link in &links {
            group.extend(result.groups.get(link).into_iter().flatten().cloned());
        }
        for link in &group {
            result.groups.insert(link.clone(), group.clone());
        }
    }
    for (board, messages) in boards {
        for record in messages {
            if record["sender"] != "lead" || record["grade"] != "proven" || record["kind"] != "note"
            {
                continue;
            }
            let Some(question) = reply(board, record).filter(|link| questions.contains_key(link))
            else {
                continue;
            };
            // Only the answer routes count; a call-level with a reply header is still a call-level.
            let channel = record["channel"].as_str().unwrap_or("");
            if channel != "questions" && channel != "lead" && !channel.starts_with("to/") {
                continue;
            }
            let text = typed::text(record);
            let decision = field(&text, "Decision");
            let typed_note = [
                names::DECISION,
                names::LEVEL,
                names::MERGE,
                "cos-decision",
                "owner-decision",
            ]
            .iter()
            .any(|kind| text.starts_with(&format!("Type: {kind}\n")));
            let valid = if typed_note {
                false
            } else if decision.is_some() {
                result.recorded_decision(&question, &text).is_some()
            } else {
                ordinary_answer(&text, top.is_some())
            };
            // A lead's own question needs its answer on questions.
            if valid && (questions[&question]["sender"] != "lead" || channel == "questions") {
                result.answers.insert(question, record.clone());
            }
        }
    }
    let mut seen = BTreeSet::new();
    for (link, question) in &questions {
        let group = &result.groups[link];
        if !seen.insert(group.first().unwrap().clone()) {
            continue;
        }
        let mut askers = Vec::new();
        let mut call_level: Option<String> = None;
        let mut call_settings = None;
        let mut severity = None;
        let mut any_open = false;
        let decisions: Vec<_> = result.decisions_for(link).into_iter().cloned().collect();
        for asker in group {
            let settings = &options[&asker.0];
            let record = &questions[asker];
            let text = typed::text(record);
            let ready = ready_board == Some(asker.0.as_str())
                && candidate.is_some_and(|candidate| record["sender"] == candidate)
                && text.lines().next() == Some("Type: ready")
                && field(&text, "For").as_deref() == Some("owner");
            let mut history = proposals.get(asker).cloned().unwrap_or_default();
            history.sort_by_key(|change| change["link"][3].as_u64());
            let legacy = (record["sender"] == "lead"
                && field(&text, "For")
                    .is_none_or(|for_whom| matches!(for_whom.as_str(), "owner" | "both")))
            .then(|| field(&text, "Level"))
            .flatten()
            .filter(|level| !level.is_empty());
            let proposed = history
                .last()
                .and_then(|change| change["level"].as_str())
                .map(str::to_owned)
                .or(legacy);
            let mut changes = levels.get(asker).cloned().unwrap_or_default();
            changes.sort_by_key(|change| change["link"][3].as_u64());
            let effective = if ready {
                Some(settings.ready_level.clone())
            } else {
                changes
                    .last()
                    .and_then(|change| change["level"].as_str())
                    .map(str::to_owned)
                    .or(proposed.clone())
            };
            let mut previous = None;
            for change in &mut changes {
                if ready {
                    change["original"] = json!(settings.ready_level);
                    change["ignored"] = json!(true);
                    change["lowered"] = json!(false);
                    continue;
                }
                let original = previous
                    .clone()
                    .or_else(|| {
                        history
                            .iter()
                            .rev()
                            .find(|proposal| proposal["at_ms"].as_u64() <= change["at_ms"].as_u64())
                            .and_then(|proposal| proposal["level"].as_str())
                            .map(str::to_owned)
                    })
                    .or_else(|| legacy_level(record));
                change["original"] = json!(original);
                change["lowered"] = json!(
                    original
                        .as_deref()
                        .zip(change["level"].as_str())
                        .is_some_and(
                            |(before, after)| settings.severity(after) < settings.severity(before)
                        )
                );
                previous = change["level"].as_str().map(str::to_owned);
            }
            let mut prior = legacy_level(record);
            for change in &mut history {
                change["original"] = json!(prior);
                prior = change["level"].as_str().map(str::to_owned);
            }
            let current = effective.as_deref().map(|raw| settings.severity(raw));
            let open = ready || !result.answers.contains_key(asker);
            if open && (!any_open || current > severity) || !open && !any_open && current > severity
            {
                call_level = effective.clone();
                severity = current;
                call_settings = Some(settings);
            }
            any_open |= open;
            askers.push(json!({"link":asker,"sender":record["sender"],"proposed":proposed,"effective":effective,"proposed_delivery":proposed.as_deref().map(|raw| &settings.level(raw).delivery),"delivery":effective.as_deref().map(|raw| &settings.level(raw).delivery),"pulse":effective.as_deref().map(|raw| &settings.level(raw).pulse),"severity":current,"topic":history.last().and_then(|change| change.get("topic")).cloned().unwrap_or_else(|| if record["sender"] == "lead" { json!(field(&text,"Topic")) } else { Value::Null }),"proposal_history":history,"level_history":changes,"ready":ready,"open":open}));
        }
        let Some(level) = call_level.as_deref() else {
            continue;
        };
        let settings = call_settings.expect("level has settings");
        let behaviour = settings.level(level);
        let latest = decisions.last().cloned();
        let source = latest
            .as_ref()
            .map(|decision| decision["link"].clone())
            .or_else(|| {
                askers.iter().find_map(|asker| {
                    asker["level_history"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .rev()
                        .find(|change| change["ignored"] != true)
                        .map(|change| change["link"].clone())
                        .or_else(|| {
                            asker["proposal_history"]
                                .as_array()
                                .and_then(|history| history.last())
                                .map(|change| change["link"].clone())
                        })
                })
            })
            .unwrap_or_else(|| json!(link));
        let fetch = json!({"tool":"get","board":source[0],"channel":source[1],"creation":source[2],"position":source[3]});
        let activity = askers
            .iter()
            .flat_map(|asker| {
                ["proposal_history", "level_history"]
                    .into_iter()
                    .flat_map(move |field| asker[field].as_array().into_iter().flatten())
            })
            .chain(decisions.iter())
            .filter_map(|fact| fact["at_ms"].as_u64())
            .max()
            .unwrap_or(0)
            .max(typed::at_ms(question));
        result.calls.push(json!({"type":"call","board":link.0,"channel":names::QUESTION_CHANNEL,"creation":link.2,"position":link.3,"link":link,"level":call_level,"delivery":behaviour.delivery,"pulse":behaviour.pulse,"severity":severity,"proposed":askers[0]["proposed"],"subject":typed::text(question).lines().find(|line| !line.starts_with("For: ")).unwrap_or("Question"),"text":typed::text(question),"askers":askers,"open":any_open,"decisions":decisions,"decision":latest,"at_ms":activity,"fetch":fetch}));
    }
    result
}

fn legacy_level(record: &Value) -> Option<String> {
    (record["sender"] == "lead")
        .then(|| field(&typed::text(record), "Level"))
        .flatten()
        .filter(|level| !level.is_empty())
}
