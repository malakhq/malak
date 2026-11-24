use super::{Event, EventName, invalid};
use clap::ValueEnum;
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, Read, Write};

#[derive(Clone, Copy, ValueEnum)]
pub(super) enum Tool {
    Claude,
    Codex,
    Generic,
}

impl Tool {
    pub(super) fn format(self) -> &'static dyn Format {
        match self {
            Self::Claude | Self::Codex => &AgentJson,
            Self::Generic => &Generic,
        }
    }
}

pub(super) trait Format {
    fn input(&self, event: EventName, session: &str) -> Value;
    fn read(&self, input: &mut dyn Read) -> io::Result<Option<Event>>;
    fn reply(&self, event: EventName, context: &str) -> Option<Value>;

    fn write(&self, output: &mut dyn Write, event: EventName, context: &str) -> io::Result<()> {
        if let Some(reply) = self.reply(event, context) {
            serde_json::to_writer(&mut *output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    }
}

struct Generic;

impl Format for Generic {
    fn input(&self, event: EventName, session: &str) -> Value {
        json!({ "event": event, "session": session })
    }

    fn read(&self, input: &mut dyn Read) -> io::Result<Option<Event>> {
        #[derive(Deserialize)]
        struct Input {
            event: EventName,
            session: String,
        }

        let input: Input = serde_json::from_reader(input)?;
        Ok(Some(Event {
            session: input.session,
            name: input.event,
        }))
    }

    fn reply(&self, event: EventName, context: &str) -> Option<Value> {
        if event == EventName::TurnEnd && !context.is_empty() {
            return Some(json!({ "decision": "continue", "context": context }));
        }
        Some(json!({ "context": context }))
    }
}

// Claude Code and Codex use the same wire format for these five events.
struct AgentJson;

const AGENT_EVENTS: [(EventName, &str); 5] = [
    (EventName::SessionStart, "SessionStart"),
    (EventName::Prompt, "UserPromptSubmit"),
    (EventName::Step, "PostToolUse"),
    (EventName::TurnEnd, "Stop"),
    (EventName::SessionEnd, "SessionEnd"),
];

fn agent_event(event: EventName) -> &'static str {
    AGENT_EVENTS
        .into_iter()
        .find_map(|(name, spelling)| (name == event).then_some(spelling))
        .expect("every contract event has a tool event")
}

impl Format for AgentJson {
    fn input(&self, event: EventName, session: &str) -> Value {
        json!({ "hook_event_name": agent_event(event), "session_id": session })
    }

    fn read(&self, input: &mut dyn Read) -> io::Result<Option<Event>> {
        #[derive(Deserialize)]
        struct Input {
            hook_event_name: String,
            #[serde(default)]
            session_id: Value,
        }

        let input: Input = serde_json::from_reader(input)?;
        let Some((name, _)) = AGENT_EVENTS
            .into_iter()
            .find(|&(_, name)| name == input.hook_event_name)
        else {
            return Ok(None);
        };
        let session = input
            .session_id
            .as_str()
            .ok_or_else(|| invalid("hook input needs a session_id string"))?;
        Ok(Some(Event {
            session: session.to_owned(),
            name,
        }))
    }

    fn reply(&self, event: EventName, context: &str) -> Option<Value> {
        if context.is_empty() {
            return None;
        }
        if event == EventName::TurnEnd {
            // A Stop hook continues the turn by blocking it with a reason.
            return Some(json!({ "decision": "block", "reason": context }));
        }
        Some(json!({
            "hookSpecificOutput": { "hookEventName": agent_event(event), "additionalContext": context }
        }))
    }
}
