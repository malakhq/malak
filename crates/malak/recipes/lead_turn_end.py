"""Hand a lead's pending mail to it when it is about to end its turn."""

import hashlib
import json
import time

from board_data import channel_status, first_due, get_message, malak
from lead_status import available, can_get_lead, guest_label, guest_reply_ack, pending_guests
from recipe_board import board_selector


BUDGET = 32 * 1024
CONTINUE = (
    "Before you stop, handle the Malak messages below, then acknowledge the lead "
    "messages with `malak ack`. Answer agent questions with `malak recipe answer`; for a "
    "question for the owner, ask the owner first and record their reply."
)
REMINDER = "Malak: lead mail you were handed is still pending: {summary}."


def pending_lead(config, handover, status):
    lead = channel_status(status, "lead")
    if lead is None or not can_get_lead(status, int(time.time() * 1000)):
        return []
    messages = []
    for position in range(first_due(lead, "lead"), lead["head"]):
        message = get_message("lead", str(position), config=config, handover=handover,
                              allow_missing=True)
        if message is not None and message["creation"] == lead["creation"]:
            messages.append(message)
    return messages


def open_questions(config, handover, status):
    channel = channel_status(status, "questions")
    if channel is None or channel["floor"] >= channel["head"]:
        return [], 0
    result = malak("--config", config, "mailbox", "--questions", "--json", handover=handover)
    asked = json.loads(result.stdout)
    agents = []
    for question in asked:
        if question["kind"] != "agent":
            continue
        message = get_message("questions", str(question["position"]), config=config,
                              handover=handover, allow_missing=True)
        if message is not None and message["creation"] == channel["creation"]:
            message["audience"] = question.get("audience", "owner")
            agents.append(message)
    return agents, sum(question["kind"] == "owner" for question in asked)


def label(message, channel):
    sender = message.get("sender_name") or message.get("sender") or "unknown"
    address = guest_label(message) if channel is None else f"{channel} #{message['position']}"
    audience = message.get("audience")
    for_whom = {"owner": ", for the owner", "both": ", for the owner and the lead"}.get(audience, "")
    return f"{address} from {sender}, kind {message['kind']}{for_whom}"


def signature(name, lead, agents, guests):
    pending = [("lead", m["creation"], m["position"]) for m in lead]
    pending += [("questions", m["creation"], m["position"]) for m in agents]
    pending += [(m["guest_board"], m["creation"], m["position"]) for m in guests]
    return hashlib.sha256(json.dumps([name, pending]).encode()).hexdigest()


def body_text(message):
    text = message.get("text")
    if text is None:
        text = bytes(message.get("body") or []).decode("utf-8", "replace")
    return text


def handover_text(name, command, lead, agents, guests, owner_open, reserved):
    header = f"Malak board {json.dumps(name)}: {CONTINUE}"
    footer = ""
    if owner_open:
        noun = "question is" if owner_open == 1 else "questions are"
        footer = f"\n{owner_open} owner {noun} waiting on the owner; do not wait for it."
    entries = [("lead", m) for m in lead] + [("questions", m) for m in agents]
    entries += [(None, m) for m in guests]
    room = BUDGET - reserved - len(header.encode()) - len(footer.encode())
    parts = []
    for index, (channel, message) in enumerate(entries):
        title = label(message, channel)
        if channel is None:
            title += f"\nAfter handling, run `{guest_reply_ack(command, message)}`."
            fetch = f"{command} read lead"
        else:
            fetch = f"malak get {channel} {message['position']}"
        body = body_text(message)
        left = len(entries) - index
        # Share what is left evenly, so one long message cannot hide the others.
        allowance = max(room // left - len(title.encode()) - 8, 0)
        data = body.encode()
        if len(data) > allowance:
            note = f"\n[shortened: {len(data)} bytes; fetch the rest with {fetch}]"
            data = data[:max(allowance - len(note.encode()), 0)]
            body = data.decode("utf-8", "ignore") + note
        part = f"\n--- {title}\n{body}"
        room -= len(part.encode())
        parts.append(part)
    return header + "".join(parts) + footer


def turn_end(name, config, handover, status, last, active, reserved):
    """Return (reply, signature to remember) for one lead board."""
    lead = pending_lead(config, handover, status)
    agents, owner_open = open_questions(config, handover, status)
    guests = available(pending_guests, config, handover) or []
    if not lead and not agents and not guests:
        return None, None
    current = signature(name, lead, agents, guests)
    # Without a remembered set, a Stop that already continued once must not continue again.
    if current == last or (active and last is None):
        parts = []
        if lead:
            parts.append(f"{len(lead)} lead message{'s' * (len(lead) != 1)} unacknowledged")
        if agents:
            parts.append(f"{len(agents)} agent question{'s' * (len(agents) != 1)} unanswered")
        if guests:
            parts.append(f"{len(guests)} guest repl{'y' if len(guests) == 1 else 'ies'} unacknowledged")
        return {"systemMessage": REMINDER.format(summary=", ".join(parts))}, current
    command = f"malak {board_selector(config, name)}"
    return {"decision": "block", "reason": handover_text(name, command, lead, agents, guests, owner_open, reserved)}, current
