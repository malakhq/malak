"""How `observe` words its reply for each agent tool and event.

Each tool has one table from event to reply shape. A new agent tool is a new
table, plus a line in `tool_for` if its payloads can be told apart.
"""


def additional_context(event):
    def shape(text):
        return {"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}}
    return shape


def stop(reply):
    """Claude Code and Codex read a Stop reply as it is: block with a reason, or show a message."""
    return reply


def generic_stop(reply):
    if "reason" in reply:
        return {"decision": "continue", "context": reply["reason"]}
    return reply


NATIVE_CONTEXT = ("SessionStart", "UserPromptSubmit", "SubagentStart", "PreToolUse", "PostToolUse")
NATIVE = {
    **{event: additional_context(event) for event in NATIVE_CONTEXT},
    "Stop": stop,
}
FORMATS = {
    "claude": NATIVE,
    "codex": NATIVE,
    # `{"event": ..., "session": ...}` payloads. The question events answer with
    # a plain context field, and turn_end continues instead of blocking.
    "generic": {
        "PreToolUse": lambda text: {"context": text},
        "PostToolUse": lambda text: {"context": text},
        "Stop": generic_stop,
    },
}


def tool_for(payload, named=None):
    """Name the tool a payload came from.

    The installed command names its tool. Without that (an older install), Codex is
    the tool that puts a turn_id on its tool, prompt and stop events.
    """
    if "event" in payload:
        return "generic"
    if named in FORMATS:
        return named
    return "codex" if "turn_id" in payload else "claude"


def reply(tool, event, value):
    """The JSON object to print, or None when this tool has no reply for the event."""
    shape = FORMATS[tool].get(event)
    return shape(value) if shape and value else None
