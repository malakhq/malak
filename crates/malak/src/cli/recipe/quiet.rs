//! Answer `recipe observe --reminder` before any recipe is unpacked, when the
//! hook payload alone shows there is nothing to say.
//!
//! Claude Code and Codex fire an entry with no matcher on every occurrence of
//! its event, so a tool call or a stopped subagent that Malak ignores still
//! costs one process. Unpacking the recipe bundle and starting Python for that
//! takes far longer than the hook budget allows, so the payload is read here
//! first. The tool names mirror `TASK_TOOLS` in the observe recipe and `TOOLS`
//! in question_hooks.py; a recipe test keeps the two lists equal.

use serde::Deserialize;
use serde::de::IgnoredAny;

/// Events the observe recipe acts on. Any other event is ignored.
pub(super) const EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "SubagentStart",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];

/// The tools whose `PostToolUse` the recipe records: owner questions and plan updates.
pub(super) const POST_TOOLS: [&str; 7] = [
    "AskUserQuestion",
    "request_user_input",
    "request_user_input_async",
    "TaskCreate",
    "TaskUpdate",
    "TodoWrite",
    "update_plan",
];

#[derive(Deserialize)]
struct Head {
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    /// Only the generic `{"event": ...}` format has this; the recipe decides those.
    event: Option<IgnoredAny>,
}

/// True when the payload is one the recipe would ignore without output or effect.
pub(super) fn nothing_to_say(input: &[u8]) -> bool {
    let Ok(head) = serde_json::from_slice::<Head>(input) else {
        return false;
    };
    if head.event.is_some() {
        return false;
    }
    match head.hook_event_name.as_deref() {
        Some("PostToolUse") => !head
            .tool_name
            .as_deref()
            .is_some_and(|tool| POST_TOOLS.contains(&tool)),
        Some(event) => !EVENTS.contains(&event),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Payloads captured from `claude -p` 2.1.292 and `codex exec` 0.169, with ids and paths scrubbed.
    const LIVE: &str = include_str!("../../../recipes/tests/live-payloads.json");
    /// Vendor-schema payloads for events the live runs did not produce.
    const SCHEMA: &str = include_str!("../../../recipes/tests/observe-payloads.json");

    fn quiet(name: &str) -> bool {
        let payload = [LIVE, SCHEMA]
            .into_iter()
            .find_map(|file| {
                serde_json::from_str::<Value>(file)
                    .unwrap()
                    .get(name)
                    .cloned()
            })
            .unwrap_or_else(|| panic!("no fixture {name}"));
        nothing_to_say(payload.to_string().as_bytes())
    }

    #[test]
    fn unused_events_and_unwatched_tools_are_quiet() {
        for name in [
            "claude_agent_done",
            "claude_subagent_bash_done",
            "claude_subagent_stop",
            "codex_ls_done",
            "codex_subagent_stop",
            "codex_permission_request",
        ] {
            assert!(quiet(name), "{name}");
        }
    }

    #[test]
    fn events_the_recipe_acts_on_go_to_it() {
        for name in [
            "claude_session_start",
            "claude_prompt",
            "claude_agent",
            "claude_subagent_start",
            "claude_subagent_bash",
            "claude_stop",
            "claude_session_end",
            "codex_session_start",
            "codex_prompt",
            "codex_ls",
            "codex_bash",
            "codex_stop",
            "codex_session_end",
            "claude_task",
            "codex_plan",
            "claude_question",
            "codex_question",
            "generic_question_start",
        ] {
            assert!(!quiet(name), "{name}");
        }
    }

    #[test]
    fn unreadable_input_goes_to_the_recipe() {
        for input in ["", "not json", "[]", "{}", r#"{"hook_event_name":7}"#] {
            assert!(!nothing_to_say(input.as_bytes()), "{input}");
        }
    }
}
