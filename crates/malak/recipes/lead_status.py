"""Read a lead's waiting work for the installed prompt hook."""

import fcntl
import json
import os
from pathlib import Path
import stat
import time

from board_data import RecipeError, ack_command, channel_status, first_due, get_message, malak
from question_hooks import pending_answer
from recipe_board import board_selector, board_settings, lead_watcher_command
from worker_queue import LaunchRecord, QueueBoard


# A lead that runs the one-shot watcher once per host-loop tick is idle between
# ticks. It counts as watching while its last tick started or ended this recently.
TICK_WINDOW_SECONDS = 10 * 60
TICK_FILE = "lead-next"
GET_GRANT = 4


def watcher_running(config):
    # Both the long-running and one-shot lead watchers hold this shared lock.
    # Between host-loop ticks the one-shot watcher's own stamp stands in for it.
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
            return ticked_recently(fd)
        finally:
            os.close(fd)
    except (KeyError, OSError, TypeError, ValueError):
        return None


def ticked_recently(directory):
    try:
        mark = os.stat(TICK_FILE, dir_fd=directory, follow_symlinks=False)
    except OSError:
        return False
    return (
        stat.S_ISREG(mark.st_mode)
        and mark.st_uid == os.geteuid()
        and 0 <= time.time() - mark.st_mtime < TICK_WINDOW_SECONDS
    )


def first_waiting(config, handover, channel, reader):
    for pos in range(first_due(channel, reader), channel["head"]):
        message = get_message(channel["name"], str(pos), config=config, handover=handover,
                              allow_missing=True)
        if message is not None and message["creation"] == channel["creation"]:
            return pos
    return None


def lead_due(config, handover, status, now):
    lead = channel_status(status, "lead")
    if lead is not None and can_get_lead(status, now):
        if (pos := first_waiting(config, handover, lead, "lead")) is not None:
            return lead["head"] - pos
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


def exited_worker(config, status, lead):
    if not lead:
        return None
    board = QueueBoard(config, Path(board_settings(config)["state"]))
    for person in status["participants"]:
        if person["id"] == "lead" or person["revoked"]:
            continue
        launch = LaunchRecord(board.state, person["id"])
        evidence, live = launch.observe()
        if (live or not launch.on_board(evidence, board.identity)
                or not (report := evidence.get("last_report") or evidence.get("report"))):
            continue
        if (report["creation"] != lead["creation"]
                or not first_due(lead, "lead") <= report["position"] < lead["head"]):
            continue
        try:
            board.final_report(report, person["id"])
        except RecipeError:
            continue
        return person["name"], report["position"]
    return None


def available(check, *args):
    try:
        return check(*args)
    except (RecipeError, OSError, ValueError, KeyError, TypeError):
        return None


def pending_guests(config, handover):
    result = malak("--config", config, "recipe", "__guest-replies", handover=handover)
    return json.loads(result.stdout)


def guest_label(message):
    channel = " " + message["channel"] if message.get("channel") in ("top/leads", "cos/ramp") else ""
    return f"guest · {message['guest_board']}{channel} #{message['position']}"


def guest_reply_ack(command, message):
    return ack_command(command, "lead", message["creation"], message["position"])


def reminder_lines(name, config, handover, status, session):
    command = f"malak {board_selector(config, name)}"
    if session and (position := available(pending_answer, config, handover, status, session)) is not None:
        yield f"Record the answer: `{command} recipe answer questions {position}`"
    count = available(lead_due, config, handover, status, int(time.time() * 1000))
    lead = channel_status(status, "lead")
    if count:
        ack = ack_command(command, "lead", lead["creation"], lead["head"] - 1)
        yield f"Lead mail waiting ({count}): `{command} read lead`, handle, then `{ack}`."
    if watcher_running(config) is False:
        yield f"Restart your watcher: `{lead_watcher_command(config, name)}`"
    if count and (worker := available(exited_worker, config, status, lead)):
        worker_name, position = worker
        yield (
            f"{worker_name} exited: read its final report with `{command} get lead {position}`, "
            "then merge or retire it."
        )
    for message in available(pending_guests, config, handover) or ():
        ack = guest_reply_ack(command, message)
        yield f"{guest_label(message)} waiting: `{command} read lead`, handle, then `{ack}`."
