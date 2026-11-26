"""How `observe` words its reply for each agent tool and event.

Each tool's adapter has its own table from event to reply shape. A new agent
tool is a new adapter file, plus a `matches` test if its payloads can be told apart.
"""

import adapters

NATIVE_CONTEXT = tuple(sorted(adapters.union("context_events")))


def tool_for(payload, named=None):
    """Name the tool a payload came from.

    The installed command names its tool. Without that (an older install), the
    adapters tell their payloads apart and one of them is the default.
    """
    return adapters.tool_for(payload, named)


def reply(tool, event, value):
    """The JSON object to print, or None when this tool has no reply for the event."""
    return adapters.get(tool).reply(event, value) if value else None
