//! Call tools share the command-line projection and authenticated writers.

use super::{Reply, seat::Seat};
use crate::cli::org::{
    calls::{self, Action, Link},
    seats::Reader,
};
use malak::Error;
use rmcp::model::Tool;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) const NAMES: &[&str] = &[
    "calls",
    "raise_call",
    "call_level",
    "call_merge",
    "decide_call",
    "relay_owner_decision",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    level: Option<String>,
    #[serde(default)]
    all: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raise {
    position: u64,
    level: String,
    topic: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Level {
    question: Link,
    level: String,
    topic: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Merge {
    questions: Vec<Link>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decide {
    question: Link,
    body: String,
}

pub(super) fn call(seat: &Seat, name: &str, input: Value, send_id: &str) -> Result<Reply, Error> {
    let value = if name == "calls" {
        let args: Read = super::arguments(input)?;
        let reader = Reader::open(seat.config.clone())?;
        calls::result(calls::list(&reader, args.level.as_deref(), args.all)?)
    } else {
        let action = match name {
            "raise_call" => {
                let args: Raise = super::arguments(input)?;
                Action::Raise {
                    position: args.position,
                    level: args.level,
                    topic: args.topic,
                }
            }
            "call_level" => {
                let args: Level = super::arguments(input)?;
                Action::Level {
                    question: args.question,
                    level: args.level,
                    topic: args.topic,
                }
            }
            "call_merge" => {
                let args: Merge = super::arguments(input)?;
                Action::Merge {
                    questions: args.questions,
                }
            }
            "decide_call" => {
                let args: Decide = super::arguments(input)?;
                Action::Decide {
                    question: args.question,
                    body: args.body,
                }
            }
            "relay_owner_decision" => {
                let args: Decide = super::arguments(input)?;
                Action::Relay {
                    question: args.question,
                    body: args.body,
                }
            }
            _ => return Err(Error::refused("unknown calls tool")),
        };
        calls::write(seat.config.clone(), action, Some(send_id.to_owned()))?
    };
    Ok(Reply {
        text: value.to_string(),
        value,
    })
}

pub(super) fn tools() -> Vec<Tool> {
    let link = json!({"type":"array","minItems":4,"maxItems":4,"description":"[board, channel, creation, position]; channel is questions."});
    [
        ("calls", "Read open calls with proposed and effective levels and every lowering. By default red and orange are listed and all includes blue; delivery is configurable. Unauthorised typed notes stay text.", json!({"level":{"type":"string"},"all":{"type":"boolean"}}),vec![]),
        ("raise_call", "Propose a call's level as its board's lead; writes call-level and blocked on rail. A sub only asks.",json!({"position":{"type":"integer","minimum":1},"level":{"type":"string","description":"Default blue, orange, red; configurable in roster severity order."},"topic":{"type":"string"}}),vec!["position","level"]),
        ("call_level", "Set a question's effective level up or down as top lead. Topic is only a hint. The lead's proposal and every original stay visible.",json!({"question":link,"level":{"type":"string","description":"Default blue, orange, red; configurable in roster severity order."},"topic":{"type":"string"}}),vec!["question","level"]),
        ("call_merge", "Merge questions asking the same thing, as their own board's lead or the top lead. Each original lead records the shared decision.",json!({"questions":{"type":"array","minItems":2,"items":link}}),vec!["questions"]),
        ("decide_call", "Decide what you know as top lead. Writes Type: decision on top calls. The asking lead records it with answer and its decision link.",json!({"question":link,"body":{"type":"string"}}),vec!["question","body"]),
        ("relay_owner_decision", "Relay the owner's words as top lead. Writes Type: decision with owner attribution on top calls. Record only what the owner said.",json!({"question":link,"body":{"type":"string"}}),vec!["question","body"]),
    ].into_iter().map(|(name,description,properties,required)| Tool::new(name, description, json!({"type":"object","additionalProperties":false,"properties":properties,"required":required}).as_object().unwrap().clone())).collect()
}
