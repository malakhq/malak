"""The generic adapter: a tool with no payload format of its own.

Its events are `{"event": ..., "session": ...}`. The question events answer with
a plain context field, and turn_end continues instead of blocking.
"""

from adapters import Adapter

QUESTION_EVENTS = {"question_start": "PreToolUse", "question_end": "PostToolUse"}


def reply(event, value):
    if event in ("PreToolUse", "PostToolUse"):
        return {"context": value}
    if event == "Stop":
        if "reason" in value:
            return {"decision": "continue", "context": value["reason"]}
        return value
    return None


def translate(payload):
    if payload.get("event") == "turn_end":
        return {"hook_event_name": "Stop", "session_id": payload.get("session"),
                "stop_hook_active": payload.get("stop_hook_active")}
    if (event := QUESTION_EVENTS.get(payload.get("event"))) is not None:
        return {
            **payload, "hook_event_name": event, "session_id": payload.get("session"),
            "tool_name": "generic",
        }
    return payload


class Generic:
    """The question tool of a generic tool: an `id` identifies each question."""

    @staticmethod
    def normalize(question):
        return question

    @staticmethod
    def key(question, text):
        return question.get("id", text)


ADAPTER = Adapter(
    key="generic",
    rank=9,
    matches=lambda payload: "event" in payload,
    reply=reply,
    translate=translate,
    question_tools={"generic": Generic},
)
