"""Read a lead's waiting work for the installed prompt hook."""

import fcntl
import json
import os
from pathlib import Path
import stat
import time

from board_data import RecipeError, channel_status, first_due, get_message, malak
from recipe_board import board_settings, lead_watcher_command


OWNER_QUESTION_AGE_SECONDS = 3 * 60
GET_GRANT = 4


def watcher_running(config):
    try:
        path = Path(board_settings(config)["socket_folder"]) / "o"
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            metadata = os.fstat(fd)
            if metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
                return None
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
            return False
        finally:
            os.close(fd)
    except (KeyError, OSError, TypeError, ValueError):
        return None


def oldest_lead(config, handover, status, now):
    lead = channel_status(status, "lead")
    if lead is None or not can_get_lead(status, now):
        return None
    for pos in range(first_due(lead, "lead"), lead["head"]):
        message = get_message("lead", str(pos), config=config, handover=handover, allow_missing=True)
        if message is not None and message["creation"] == lead["creation"]:
            seconds, nanos = message["sent_at"]
            return lead["head"] - pos, age(now - (seconds * 1000 + nanos // 1_000_000))
    return None


def can_get_lead(status, now):
    person = next((person for person in status["participants"] if person["id"] == "lead"), None)
    if person is None:
        return False
    ends, rules = person["grant"]
    if ends is not None:
        before_epoch, seconds, nanos = ends
        if before_epoch or seconds * 1000 + nanos // 1_000_000 <= now:
            return False
    # The one-segment channel can match only a literal or one wildcard.
    return any(pattern in {"lead", "*", "**"} and mask & GET_GRANT for pattern, mask in rules)


def age(milliseconds):
    seconds = max(0, milliseconds // 1000)
    return f"{seconds // 60} min" if seconds >= 60 else f"{seconds} sec"


def waiting_context(name, config, handover, status):
    try:
        return describe_waiting(name, config, handover, status)
    except (RecipeError, OSError, ValueError, KeyError, TypeError):
        return None


def describe_waiting(name, config, handover, status):
    now = int(time.time() * 1000)
    waiting = []
    if lead := oldest_lead(config, handover, status, now):
        count, oldest = lead
        noun = "message" if count == 1 else "messages"
        waiting.append(f"{count} lead {noun} unacknowledged (oldest {oldest})")
    questions = old_questions(config, handover, channel_status(status, "questions"), now)
    if questions:
        noun = "question" if len(questions) == 1 else "questions"
        waiting.append(f"{len(questions)} owner {noun} open (oldest {age(now - min(questions))})")
    running = watcher_running(config)
    if running is None:
        waiting.append("could not tell whether your watcher is running")
    elif not running:
        waiting.append(f"your watcher is not running. Restart with `{lead_watcher_command(config, name)}`")
    if waiting:
        return f"Malak board {json.dumps(name)}: {', '.join(waiting)}."
    return None


def old_questions(config, handover, channel, now):
    if channel is None or channel["floor"] >= channel["head"]:
        return []
    result = malak("--config", config, "mailbox", "--questions", "--json", handover=handover)
    cutoff = now - OWNER_QUESTION_AGE_SECONDS * 1000
    return [
        question["at_ms"] for question in json.loads(result.stdout)
        if question["kind"] == "owner" and question["at_ms"] <= cutoff
    ]
