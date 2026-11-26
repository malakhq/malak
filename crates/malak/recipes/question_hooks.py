"""Record a lead's question-tool calls on its board."""

from contextlib import contextmanager
from datetime import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess

import adapters
from agent_settings import read_file, replace_file
from board_data import (
    QUESTIONS_CHANNEL, RecipeError, channel_status, get_message, malak, may_retry_send, reply_command,
)
from recipe_board import board_settings


QUESTION_TOOLS = adapters.merged("question_tools")
TOOLS = set(QUESTION_TOOLS)
ASYNC_TOOLS = adapters.union_of(lambda adapter: [adapter.async_tool] if adapter.async_tool else [])
EVENTS = {"PreToolUse", "PostToolUse"}
RECOMMENDED = " (Recommended)"


def translate(payload):
    """Put a payload in the tool's own event vocabulary into hook-event form."""
    for adapter in adapters.all_adapters():
        if adapter.translate is not None:
            payload = adapter.translate(payload)
    return payload


def is_question(payload):
    return payload.get("tool_name") in TOOLS and payload.get("hook_event_name") in EVENTS


def identifier(*parts):
    return hashlib.sha256(json.dumps(parts).encode()).hexdigest()[:32]


class CallRecords:
    """Persist tool-call receipts and outstanding answer attempts beside the board."""

    def __init__(self, directory, session):
        self.path = directory / f"{identifier(session)}.json"
        self.raw = read_file(self.path, private=True)
        self.calls = json.loads(self.raw) if self.raw is not None else {}

    def save(self):
        updated = json.dumps(self.calls, ensure_ascii=False).encode()
        replace_file(self.path, self.raw, updated)
        self.raw = updated


@contextmanager
def records(config, session, *, create):
    directory = Path(board_settings(config)["state"]) / "question-hooks"
    if create:
        directory.mkdir(mode=0o700, exist_ok=True)
    try:
        fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    except FileNotFoundError:
        yield None
        return
    try:
        metadata = os.fstat(fd)
        if metadata.st_uid != os.geteuid() or stat.S_IMODE(metadata.st_mode) != 0o700:
            raise ValueError("question hook records need a private board directory")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield CallRecords(directory, session)
    finally:
        os.close(fd)


def question_inputs(payload):
    tool_input = payload.get("tool_input")
    questions = tool_input.get("questions") if isinstance(tool_input, dict) else None
    if not isinstance(questions, list):
        return
    tool = QUESTION_TOOLS[payload["tool_name"]]
    for index, question in enumerate(questions):
        if not isinstance(question, dict) or (question := tool.normalize(question)) is None:
            continue
        if not isinstance(text := question.get("question"), str):
            continue
        key = tool.key(question, text)
        if text.strip() and isinstance(key, str) and key:
            yield str(index), key, question


def question_body(question):
    options = []
    recommendation = question.get("recommendation", "")
    for option in question["options"]:
        label = option["label"]
        description = option.get("description", "")
        if label.endswith(RECOMMENDED):
            label = label.removesuffix(RECOMMENDED)
            if not recommendation:
                recommendation = label
        options.append(f"- {label}: {description}\n" if description else f"- {label}\n")
    return subprocess.run(
        ["/bin/sh", "-c", '. "$1"; question_body "$2" "$3" "$4" "$5"', "malak",
         Path(__file__).with_name("board.sh"), question["question"], question.get("blocks", ""),
         "".join(options), recommendation],
        capture_output=True, check=True,
    ).stdout


def attempt(records, entry, field, status, channel):
    previous = entry.get(field)
    if previous is None:
        previous = entry[field] = {
            "run": status["run"], "head": channel["head"], "creation": channel["creation"],
            "time": datetime.now().strftime("%H:%M"),
        }
        # Keep the retry boundary even if the send is accepted but its reply is lost.
        records.save()
    if may_retry_send(previous["run"], previous, status["run"], channel):
        return previous
    return None


def post_questions(payload, saved, call, config, handover, status, channel):
    for index, key, question in question_inputs(payload):
        answer_key = identifier(key)
        entry = call.setdefault(index, {"key": answer_key})
        if "position" in entry or entry["key"] != answer_key:
            continue
        try:
            body = question_body(question)
            if not attempt(saved, entry, "question_attempt", status, channel):
                continue
            message_id = message_identifier(payload, index, "question")
            result = malak("--config", config, "send", "--json", "--id", message_id,
                           QUESTIONS_CHANNEL, "-", handover=handover, body=body,
                           session=payload["session_id"])
            receipt = json.loads(result.stdout)
            if receipt["id"] == message_id and receipt["creation"] == channel["creation"]:
                entry.update(id=receipt["id"], position=receipt["position"], creation=receipt["creation"])
                entry.pop("question_attempt")
                saved.save()
        except (KeyError, TypeError, ValueError, RecipeError, subprocess.CalledProcessError):
            continue


def answers(payload):
    response = payload.get("tool_response")
    if isinstance(response, str):
        response = json.loads(response)
    if not isinstance(response, dict) or not isinstance(reply := response.get("answers"), dict):
        return {}
    selected = {}
    for key, value in reply.items():
        if isinstance(value, dict):
            value = value.get("answers")
        if isinstance(value, list) and all(isinstance(choice, str) for choice in value):
            value = ", ".join(choice for choice in value if choice.strip())
        if isinstance(value, str) and value.strip():
            selected[identifier(key)] = value
    return selected


def message_identifier(payload, index, kind):
    return identifier(payload["session_id"], payload["tool_name"], payload["tool_use_id"], index, kind)


def open_owner_questions(config, handover):
    result = malak("--config", config, "mailbox", "--questions", "--json", handover=handover)
    return [
        question["position"] for question in json.loads(result.stdout) if question["kind"] == "owner"
    ]


def async_replies(prompt):
    """The (tool, call, index) of each asynchronous question the prompt answers."""
    return [reply for adapter in adapters.all_adapters() if adapter.async_replies
            for reply in adapter.async_replies(prompt)]


def note_async_replies(replied, config, status, session):
    channel = channel_status(status, QUESTIONS_CHANNEL)
    if channel is None:
        return False
    marked = False
    with records(config, session, create=False) as saved:
        if saved is None:
            return False
        for tool, call, index in replied:
            entry = saved.calls.get(identifier(tool, call), {}).get(index)
            if entry and entry.get("creation") == channel["creation"] and not entry.get("answer_seen"):
                entry["answer_seen"] = True
                marked = True
        if marked:
            saved.save()
    return marked


def record_answers(payload, saved, call, config, handover, status, channel):
    reply = answers(payload)
    pending = False
    open_questions = None
    for index, entry in call.items():
        answer = reply.get(entry["key"])
        if not answer or entry.get("answered") or entry.get("creation") != channel["creation"]:
            continue
        try:
            if "answer_attempt" in entry:
                if open_questions is None:
                    open_questions = set(open_owner_questions(config, handover))
                # The old send may have reached the board, or the lead used
                # the command shown by the reminder.
                if entry["position"] not in open_questions:
                    entry["answered"] = True
                    entry.pop("answer_attempt")
                    saved.save()
                    continue
            delivery = attempt(saved, entry, "answer_attempt", status, channel)
            if delivery is None:
                pending = True
                continue
            question = get_message(QUESTIONS_CHANNEL, str(entry["position"]), config=config,
                                   handover=handover, allow_missing=True)
            if question is None or question["id"] != entry["id"] or question["creation"] != entry["creation"]:
                entry.pop("answer_attempt")
                saved.save()
                continue
            body = f'Owner, in chat at {delivery["time"]}: "{answer}"\n'.encode()
            message_id = message_identifier(payload, index, "answer")
            command = reply_command(
                QUESTIONS_CHANNEL, QUESTIONS_CHANNEL, entry["creation"], entry["position"],
                identifier=message_id,
            )
            result = malak("--config", config, *command[1:], handover=handover, body=body,
                           session=payload["session_id"])
        except RecipeError:
            pending = True
            continue
        receipt = json.loads(result.stdout)
        if receipt["id"] == message_id and receipt["creation"] == channel["creation"]:
            entry["answered"] = True
            entry.pop("answer_attempt")
            saved.save()
        else:
            pending = True
    return pending


def record(payload, config, handover, status):
    call_id = payload.get("tool_use_id")
    if not isinstance(call_id, str) or not call_id:
        return
    pre = payload["hook_event_name"] == "PreToolUse"
    if (channel := channel_status(status, QUESTIONS_CHANNEL)) is None or (pre and channel["stopped"]):
        return
    with records(config, payload["session_id"], create=pre) as saved:
        if saved is None:
            return
        key = identifier(payload["tool_name"], call_id)
        if pre:
            call = saved.calls.setdefault(key, {})
            post_questions(payload, saved, call, config, handover, status, channel)
        elif payload["tool_name"] not in ASYNC_TOOLS and (call := saved.calls.get(key)):
            return record_answers(payload, saved, call, config, handover, status, channel)


def pending_answer(config, handover, status, session):
    channel = channel_status(status, QUESTIONS_CHANNEL)
    if channel is None:
        return None
    with records(config, session, create=False) as saved:
        if saved is None:
            return None
        pending = {
            entry["position"]: entry["id"]
            for call in saved.calls.values() for entry in call.values()
            if ("answer_attempt" in entry or entry.get("answer_seen"))
            and entry["creation"] == channel["creation"]
        }
    if pending:
        for position in open_owner_questions(config, handover):
            if position not in pending:
                continue
            question = get_message(QUESTIONS_CHANNEL, str(position), config=config,
                                   handover=handover, allow_missing=True)
            if question and question["id"] == pending[position] and question["creation"] == channel["creation"]:
                return position
    return None
