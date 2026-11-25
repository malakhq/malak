"""Read the board config and use the CLI's structured responses."""

import fcntl
import json
import os
import re
from pathlib import Path
import select
import shlex
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid
from collections import Counter
from datetime import datetime, timedelta, timezone

import recipe_output as out
from recipe_board import (
    LEAD_WATCHER_CONSEQUENCE, board_config, board_entries, board_identity, board_name,
    board_selector, config_for_socket, lead_watcher_command,
)

QUESTIONS_CHANNEL = "questions"
ORDERS_CHANNEL = "orders"
ORDER_HOURS = 8
NEVER_DELEGATED = """Never delegated, whatever this order says. These stay open as questions:
- changing the spec
- deleting data
- publishing or pushing anything
- spending money
- anything that cannot be undone
"""
PROBE_CHANNEL = "selftest/lead"
PROBE_ID_PREFIX = "lead-up-"
PROBE_BODY = "Malak lead-up self-test"
BOARD_COLUMNS = ("Board", "Status", "Participants", "Lead waiting")
MALAK_EXE = os.environ.get("MALAK_RECIPE_EXE", "malak")


class RecipeError(Exception):
    def __init__(self, message, code=2):
        super().__init__(message)
        self.code = code


BOARD_CONFIG = None


def use_board(config):
    """Bind participant calls to the requested board while keeping their handover."""
    global BOARD_CONFIG
    if config:
        BOARD_CONFIG = config


def malak_command(*args):
    chosen = ["--config", BOARD_CONFIG] if BOARD_CONFIG and args[0] != "--config" else []
    return [MALAK_EXE, *chosen, *args]


def malak(*args, handover=None, check=True, body=None, timeout=None):
    env = None if handover is None else {**os.environ, "MALAK_HANDOVER": str(handover)}
    result = subprocess.run(
        malak_command(*args), env=env, stdin=subprocess.DEVNULL if body is None else None,
        input=body, capture_output=True, text=not isinstance(body, bytes), timeout=timeout,
    )
    if isinstance(body, bytes):
        result.stdout = result.stdout.decode(errors="replace")
        result.stderr = result.stderr.decode(errors="replace")
    if check and result.returncode:
        raise RecipeError(result.stderr.strip() or f"malak exited with status {result.returncode}",
                          result.returncode)
    return result


def board_state_from(settings):
    return Path(settings["board"]["state"])


def board_state(source):
    return board_state_from(tomllib.load(source))


def board_status(config, *, check=True):
    result = malak("--config", config, "board", "status", "--json", check=check)
    return None if result.returncode else json.loads(result.stdout)


def enrol_lead_sender(config, name, handover, *binding, timeout=None):
    result = malak("--config", config, "enrol", "--json", "--out", handover,
                   "--grant", "lead:send", *binding, name, timeout=timeout)
    return json.loads(result.stdout)["id"]


def check_board(config, handover):
    # Verify that the selected handover can attach, independent of its grants.
    malak("--config", config, "recipe", "__check-board", handover=handover)


def start_board(config):
    if board_status(config, check=False) is not None:
        return
    with tempfile.TemporaryFile() as errors:
        child = subprocess.Popen(
            [MALAK_EXE, "--config", config, "board", "start"],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=errors,
            start_new_session=True,
        )
        with child.stdout:
            readable, _, _ = select.select([child.stdout], [], [], 15)
            if readable and child.stdout.readline() == b"ready\n":
                return
        if child.poll() is None:
            child.terminate()
        child.wait()
        if board_status(config, check=False) is not None:
            return
        errors.seek(0)
        raise RecipeError(errors.read().decode(errors="replace").strip() or "board missed readiness")


def probe(config, handover):
    step = "create channel"
    try:
        malak("--config", config, "channel", "create", PROBE_CHANNEL)
        step = "send note"
        identifier = PROBE_ID_PREFIX + uuid.uuid4().hex
        receipt = malak(
            "--config", config, "send", "--json", "--kind", "note", "--id", identifier, PROBE_CHANNEL, PROBE_BODY,
            handover=handover,
        )
        stored = json.loads(receipt.stdout)
        creation, position = stored["creation"], stored["position"]
        body = list(PROBE_BODY.encode())
        while True:
            step = "read note"
            output = malak("--config", config, "read", "--json", PROBE_CHANNEL, handover=handover).stdout
            values = map(json.loads, output.splitlines())
            records = [value for value in values if "channels" not in value]
            for record in records:
                if (
                    record["creation"] != creation or record["sender"] != "lead"
                    or record["kind"] != "note" or record["body"] != body
                    or not record["id"].startswith(PROBE_ID_PREFIX)
                    or record["position"] > position
                ):
                    raise RecipeError("unexpected record before the probe")
                if record["position"] == position:
                    if record["id"] != identifier:
                        raise RecipeError("read returned a different note at the probe position")
                    break
            else:
                if not records:
                    raise RecipeError("probe missing from the read reply")
                # A previous run may have stopped after sending. Retire only
                # verified probes so a backlog cannot hide the new one.
                record = records[-1]
            step = "acknowledge note"
            malak("--config", config, "ack", "--creation", creation, PROBE_CHANNEL, str(record["position"]),
                  handover=handover)
            if record["position"] == position:
                return
    except (RecipeError, OSError, KeyError, ValueError) as error:
        raise RecipeError(f"self-test failed at {step}: {error}") from error


def parent_edits(contents, assignment):
    newline = "\r\n" if "\r\n" in contents else "\n"
    # A config may use dotted keys, a table header or an inline table.
    yield f"board.{assignment}{newline}{contents}"
    start = 0
    for line in contents.splitlines(keepends=True):
        end = start + len(line)
        if line.lstrip().startswith("[") and line.endswith("\n"):
            yield f"{contents[:end]}{assignment}{newline}{contents[end:]}"
        elif "=" in line:
            value = line.split("=", 1)[1].lstrip()
            if value.startswith("{"):
                offset = end - len(value) + 1
                yield f"{contents[:offset]}{assignment}, {contents[offset:]}"
        start = end


def with_parent(contents, settings, parent):
    value = json.dumps(parent, ensure_ascii=False).replace("\x7f", "\\u007f")
    expected = {**settings, "board": {**settings["board"], "parent": parent}}
    for candidate in parent_edits(contents, f"parent = {value}"):
        # A table-looking line may be inside a string or another table.
        # Verify that parent is the only setting the edit changed.
        try:
            if tomllib.loads(candidate) == expected:
                return candidate
        except tomllib.TOMLDecodeError:
            continue
    raise RecipeError("could not add parent to the board configuration")


def replace_config(config, contents, metadata):
    with tempfile.NamedTemporaryFile(dir=config.parent, delete=False) as temp:
        path = Path(temp.name)
        try:
            temp.write(contents.encode())
            temp.flush()
            current = os.fstat(temp.fileno())
            if (current.st_uid, current.st_gid) != (metadata.st_uid, metadata.st_gid):
                os.fchown(temp.fileno(), metadata.st_uid, metadata.st_gid)
            os.fchmod(temp.fileno(), stat.S_IMODE(metadata.st_mode))
            os.fsync(temp.fileno())
            # Let the CLI check its own config constraints before publishing.
            board_status(path)
            os.replace(path, config)
        finally:
            path.unlink(missing_ok=True)


def current_session():
    process = malak("recipe", "__session-process").stdout
    if not process:
        raise RecipeError("Malak could not identify the invoking session")
    return json.loads(process)


def bind_lead(config, handover):
    session = current_session()
    lead = next(person for person in board_status(config)["participants"] if person["id"] == "lead")
    claim = lead.get("claim", {})
    if claim.get("state") == "bound" and all(
        claim["session"].get(field) == session[field] for field in ("pid", "start_time")
    ):
        return
    malak("--config", config, "rekey", "lead", "--session", str(session["pid"]),
          "--fingerprint", lead["fingerprint"], "--out", handover)


def prepare_board(config, parent):
    # Lock the directory so replacing the config keeps starts serialized.
    directory = os.open(config.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        fcntl.flock(directory, fcntl.LOCK_EX)
        with config.open("rb") as source:
            contents = source.read().decode()
            settings = tomllib.loads(contents)
            board = settings["board"]
            handover = Path(board["state"]) / "lead.handover"
            updated = None
            if parent is not None:
                if "parent" not in board:
                    updated = with_parent(contents, settings, parent)
                elif board["parent"] != parent:
                    raise RecipeError(f"board already names parent {board['parent']!r}")
            try:
                start_board(config)
            except (RecipeError, OSError, ValueError) as error:
                raise RecipeError(f"board startup failed: {error}") from error
            bind_lead(config, handover)
            check_board(config, handover)
            probe(config, handover)
            if updated is not None:
                replace_config(config, updated, os.fstat(source.fileno()))
        return handover
    finally:
        os.close(directory)


def lead_up(config, parent_config=None):
    config = Path(config).absolute()
    parent_config = Path(parent_config).absolute() if parent_config else None
    name = board_name(config)
    parent = board_name(parent_config) if parent_config is not None else None
    manager = parent_manager(parent_config, parent) if parent_config is not None else None
    if parent_config is None and (selected := os.environ.get("MALAK_HANDOVER")):
        with config.open("rb") as source:
            default_handover = board_state(source) / "lead.handover"
        if Path(selected).resolve() != default_handover.resolve():
            check_board(config, selected)
    handover = prepare_board(config, parent)
    if manager is not None:
        report_to_parent(config, name, parent_config, parent, *manager)
    fields = (("Parent", parent), ("Parent report", "Sent")) if parent_config else ()
    selector = board_selector(config, name)
    fields += (
        ("Board commands", f"Pass {selector} on every board command."),
        ("Lead watcher", f"Run `{lead_watcher_command(config, name)}` in the background. "
                         "Use background Bash in Claude Code or a background command in Codex. "
                         "Handle the output, acknowledge lead messages, and run it again."),
        ("Acknowledgements", "Acknowledge each lead message once handled with "
                             f"`malak {selector} ack --creation CREATION lead POSITION`."),
        ("Consequence", f"Without the watcher, {LEAD_WATCHER_CONSEQUENCE}"),
        ("Setup guide", "Run `malak recipe --help`."),
    )
    out.details(f"Board {name} ready. Lead check passed.", fields, stream=sys.stderr)
    export = f"export MALAK_HANDOVER={shlex.quote(str(handover))}"
    if out.interactive(sys.stdout):
        out.next_step(export, stream=sys.stdout, command=True)
    else:
        print(export)


def channel_status(status, name):
    return next((channel for channel in status["channels"] if channel["name"] == name), None)


def first_due(channel, reader):
    return max(channel["floor"], channel.get("readers", {}).get(reader, 0) + 1)


def active_participants(status):
    return sorted(person["name"] for person in status["participants"] if not person["revoked"])


def lead_creation(status):
    lead = channel_status(status, "lead")
    return lead["creation"] if lead else None


def parent_manager(parent_config, parent_name):
    handover = os.environ.get("MALAK_HANDOVER")
    manager = os.environ.get("MALAK_PARTICIPANT_ID")
    enrollment = f"Ask the owner to run malak recipe worker-start NAME --board {shlex.quote(parent_name)} first."
    if not handover:
        raise RecipeError(f"Manager has no handover on parent {parent_name!r}. {enrollment}")
    if not manager:
        raise RecipeError(f"Pass --id with the manager's participant ID on {parent_name!r}. {enrollment}")

    status = board_status(parent_config)
    if not any(
        person["id"] == manager and manager != "lead" and not person["revoked"]
        for person in status["participants"]
    ):
        raise RecipeError(f"Manager {manager!r} is not enrolled on parent {parent_name!r}. {enrollment}")
    check_board(parent_config, handover)
    expected = lead_creation(status)
    if expected is None:
        raise RecipeError(f"parent {parent_name!r} has no lead channel")
    return handover, expected


def report_to_parent(config, name, parent_config, parent, handover, expected):
    participants = ", ".join(active_participants(board_status(config)))
    report = f"Board {name}\nConfig {config}\nParticipants {participants}\n"
    receipt = malak("--config", parent_config, "send", "--json", "--kind", "report", "lead", "-",
                    handover=handover, body=report)
    actual = json.loads(receipt.stdout)["creation"]
    if actual != expected:
        raise RecipeError(f"The lead channel on parent {parent!r} changed creation during the report.")


def board_summary(config, *, count_participants=False):
    status = board_status(config, check=False)
    if status is None:
        return "Stopped", "Unavailable", "Unavailable"
    names = active_participants(status)
    participants = len(names) if count_participants else ", ".join(names)
    lead = channel_status(status, "lead")
    waiting = max(0, lead["head"] - first_due(lead, "lead")) if lead else 0
    return "Running", participants, waiting


def list_boards():
    rows = [(name, *board_summary(config)) for name, config in board_entries()]
    out.table("Boards", BOARD_COLUMNS, rows, "No boards found.")

def tree_boards():
    entries = board_entries()
    boards = dict(entries)
    if len(boards) != len(entries):
        duplicate = next(name for name, count in Counter(name for name, _ in entries).items() if count > 1)
        board_config(duplicate)  # Use the resolver's ambiguity diagnostic.
    parents = {}
    children = {name: [] for name in boards}
    roots = []
    for name, config in boards.items():
        with config.open("rb") as source:
            parent = tomllib.load(source)["board"].get("parent")
        if parent is not None and not isinstance(parent, str):
            raise RecipeError(f"parent must be a board name in {config}")
        parents[name] = parent
        if parent in boards:
            children[parent].append(name)
        else:
            marker = "" if parent is None else f" [missing parent {parent}]"
            roots.append((name, marker))

    seen = set()
    rows = []

    def add_branch(root, marker):
        pending = [(root, 0, marker)]
        while pending:
            name, depth, suffix = pending.pop()
            if name in seen:
                continue
            seen.add(name)
            summary = board_summary(boards[name], count_participants=True)
            rows.append((f"{'  ' * depth}{name}{suffix}", *summary))
            pending.extend((child, depth + 1, "") for child in reversed(children[name]))

    for root, marker in roots:
        add_branch(root, marker)
    for name in boards:
        if name in seen:
            continue
        # Every remaining branch leads back to a cycle. Start at a member
        # of that cycle so its whole component can be shown once.
        chain = set()
        while name not in chain:
            chain.add(name)
            name = parents[name]
        add_branch(name, " [parent cycle]")
    out.table("Board hierarchy", BOARD_COLUMNS, rows, "No boards found.")


def get_message(channel, position, *, config=None, handover=None, allow_missing=False):
    options = ["--config", config] if config else []
    output = malak(*options, "get", "--json", "--", channel, position, handover=handover).stdout
    if not output.startswith("{"):
        raise RecipeError(f"no message at {channel} #{position}: {output.strip()}")
    record = json.loads(output)
    if "result" in record:
        if allow_missing:
            return None
        raise RecipeError(f"no message at {channel} #{position}: {record['result']}")
    return record


def reply_command(inbox, channel, creation, position, kind="note"):
    return malak_command(
        "send", "--json", "--kind", kind, "--reply-to",
        channel, creation, str(position),
        "--", inbox, "-",
    )


def send_result(command, title, fields=(), *, handover=None, body=None, stdin_prefix=None):
    env = None if handover is None else {**os.environ, "MALAK_HANDOVER": str(handover)}
    if stdin_prefix is None:
        result = subprocess.run(command, input=body, env=env, stdout=subprocess.PIPE)
    else:
        with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, env=env) as send:
            try:
                with send.stdin:
                    send.stdin.write(stdin_prefix)
                    shutil.copyfileobj(sys.stdin.buffer, send.stdin)
            except BrokenPipeError:
                # The CLI reports a refusal, for example when the body exceeds its byte limit.
                pass
            stdout = send.stdout.read()
            result = subprocess.CompletedProcess(command, send.wait(), stdout)
    if result.returncode == 0:
        value = json.loads(result.stdout)
        out.receipt(title(value["position"]), value, fields)
    return result.returncode


def answer_channel(config, position):
    if handover := os.environ.get("MALAK_HANDOVER"):
        try:
            socket_folder, sender = board_identity(handover)
        except (OSError, ValueError):
            # Leave credential validation to the participant command.
            return "lead"
        if sender != "lead":
            # The lead closes questions. Keep participant replies on their
            # original route without opening the owner door.
            return "lead"
        if not config:
            config = config_for_socket(socket_folder)
            if config is None:
                raise RecipeError("cannot find the handover's board; pass --board NAME or --config PATH")
    options = ["--config", config] if config else []
    number = int(position)
    questions = json.loads(malak(*options, "mailbox", "--questions", "--json").stdout)
    return QUESTIONS_CHANNEL if any(q["position"] == number for q in questions) else "lead"


def answer(config, order, kind, *address):
    use_board(config)
    prefix = None
    if order:
        mark = order_mark(config, order)
        if not sys.stdin.buffer.peek(1):
            out.error("send the answer", "Message body is empty.", "Give the reply on standard input.")
            return 2
        prefix = f"{mark}\n\n".encode()
    if len(address) == 4:
        inbox, channel, creation, position = address
    else:
        if len(address) == 2:
            channel, position = address
        else:
            position, = address
            channel = answer_channel(config, position)
        question = get_message(channel, position)
        sender = question["sender"]
        if channel == QUESTIONS_CHANNEL and sender == "lead" and not question.get("headers", {}).get("reply_to"):
            inbox = QUESTIONS_CHANNEL
        else:
            inbox = "lead" if sender == "lead" else f"to/{sender}"
        creation = question["creation"]
    command = reply_command(inbox, channel, creation, position, kind)
    suffix = f" under order #{order}" if order else ""
    return send_result(
        command,
        lambda _: f"Answered {channel} #{position}{suffix}.",
        (("Inbox", inbox),),
        stdin_prefix=prefix,
    )


def order_end(until, duration, now):
    if until and duration:
        raise RecipeError("give --until or --for, not both")
    if duration:
        match = re.fullmatch(r"([1-9][0-9]*)([mhd])", duration)
        if match is None:
            raise RecipeError(f"--for must be a number then m, h or d, like 30m, 12h or 2d, not {duration!r}")
        unit = {"m": "minutes", "h": "hours", "d": "days"}[match[2]]
        return now + timedelta(**{unit: int(match[1])})
    if not until:
        return now + timedelta(hours=ORDER_HOURS)
    try:
        end = datetime.strptime(until, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
    except ValueError:
        raise RecipeError(f"--until must be a UTC time like 2026-01-31T06:00:00Z, not {until!r}")
    if end <= now:
        raise RecipeError(f"--until {until} is not in the future")
    return end


def post_order(config, until="", duration=""):
    use_board(config)
    end = order_end(until, duration, datetime.now(timezone.utc).replace(microsecond=0))
    scope = sys.stdin.read().strip()
    if not scope:
        raise RecipeError("say what the lead may decide, on standard input")
    body = f"Until: {end:%Y-%m-%dT%H:%M:%SZ}\n\n{scope}\n\n{NEVER_DELEGATED}"
    local = end.astimezone()
    remaining = end - datetime.now(timezone.utc)
    minutes = max(0, round(remaining.total_seconds() / 60))
    hours, rest = divmod(minutes, 60)
    age = f"{hours} hr {rest} min" if hours and rest else f"{hours} hr" if hours else f"{rest} min"
    result = malak("send", "--json", "--kind", "note", ORDERS_CHANNEL, "-", body=body)
    receipt = json.loads(result.stdout)
    zone = local.strftime("%z")
    expiry = f"{local:%Y-%m-%d %H:%M} {zone[:3]}:{zone[3:]} (in {age})"
    out.receipt(f"Order posted to {ORDERS_CHANNEL} #{receipt['position']}.", receipt, (("Ends", expiry),))


def order_mark(config, position):
    use_board(config)
    order = get_message(ORDERS_CHANNEL, position)
    listed = malak("mailbox", "--orders", "--plain").stdout.splitlines()
    state = next((line.split("  ")[1] for line in listed if line.startswith(f"order #{position}  ")), None)
    if state is None:
        raise RecipeError(f"no order at {ORDERS_CHANNEL} #{position}")
    if state != "in force":
        raise RecipeError(f"order #{position} has ended; leave the question open for the owner")
    return f"Under order: {order['creation']} {position}"


def forward_question(config, position):
    manager = os.environ.get("MALAK_HANDOVER")
    with open(config, "rb") as source:
        settings = tomllib.load(source)
    parent = settings["board"].get("parent")
    if parent is None:
        raise RecipeError("this board names no parent to send the question to")
    if not isinstance(parent, str) or parent in {"", ".", ".."} or "/" in parent:
        raise RecipeError(f"invalid parent board name in {config}")
    if not manager:
        raise RecipeError(f"MALAK_HANDOVER must name the manager's handover on {parent!r}")
    parent_config = board_config(parent)
    check_board(parent_config, manager)
    original = get_message(QUESTIONS_CHANNEL, position, config=config,
                           handover=board_state_from(settings) / "lead.handover")
    sender = f'{original["sender_name"]} ({original["sender"]})'
    header = f"Forwarded from {sender} on board {board_name(config)}:\n\n"
    trace = original.get("headers", {}).get("trace")
    link = ["--trace", trace] if trace else []
    # The manager sends it on as its own message under its own handover.
    result = malak(
        "--config", parent_config, "send", "--json", "--kind", "note", *link, QUESTIONS_CHANNEL, "-",
        handover=manager,
        body=header + bytes(original["body"]).decode(errors="replace"),
    )
    receipt = json.loads(result.stdout)
    out.receipt(
        f"Forwarded {QUESTIONS_CHANNEL} #{position} to {QUESTIONS_CHANNEL} on parent board {parent}.",
        receipt, (("Position", receipt["position"]),),
    )


def forward(config, channel, position, target=None):
    if channel == QUESTIONS_CHANNEL and target is None:
        return forward_question(config, position)
    if target is None:
        prefix, _, name = channel.partition("/")
        if prefix != "relay" or not name or "/" in name:
            raise RecipeError("supply a target for channels other than relay/<name>")
        target = name

    use_board(config)
    if handover := os.environ.get("MALAK_HANDOVER"):
        check_board(config, handover)
    status = board_status(config)
    recipient = next((
        person["id"] for person in status["participants"]
        if person["name"] == target and not person["revoked"] and person["id"] != "lead"
    ), None)
    if recipient is None:
        raise RecipeError(f"no enrolled participant named {target!r}")

    original = get_message(channel, position)
    inbox = f"to/{recipient}"
    options, body = forward_record(original)
    return send_result(
        malak_command("send", "--json", *options, "--", inbox, "-"),
        lambda _: f"Forwarded {channel} #{position} to {target}.",
        (("Inbox", inbox),),
        body=body,
    )


def forward_record(original):
    sender = f'{original["sender_name"]} ({original["sender"]})'
    body = f"Forwarded from {sender}:\n\n".encode() + bytes(original["body"])
    options = ["--kind", "note", "--reply-to", original["channel"],
               original["creation"], str(original["position"])]
    if trace := original.get("headers", {}).get("trace"):
        options.extend(("--trace", trace))
    return options, body



def recover_worker_brief(config, state, status, participant, destination):
    inbox = f"to/{participant}"
    channel = channel_status(status, inbox)
    if channel is None or channel["stopped"]:
        raise RecipeError(f"cannot read the previous inbox {inbox}. Supply --brief FILE.")

    # The previous key may be revoked. Read using the lead's handover.
    lead = Path(state) / "lead.handover"
    prefix = f"Your inbox: {inbox}\n".encode()
    for position in range(channel["floor"], channel["head"]):
        record = get_message(inbox, str(position), config=config, handover=lead, allow_missing=True)
        if record is None:
            continue
        if record["creation"] != channel["creation"]:
            raise RecipeError(f"previous inbox {inbox} changed. Supply --brief FILE.")
        if record["sender"] == "lead" and record["grade"] == "proven" and record["kind"] == "task":
            body = bytes(record["body"])
            if body.startswith(prefix):
                Path(destination).write_bytes(body[len(prefix):])
                return
    if channel["floor"] > 1:
        raise RecipeError(f"history was trimmed in the previous inbox {inbox}. Supply --brief FILE.")


def wait_task(config, handover, worker_id, cursor, cancelled=lambda: False):
    inbox = f"to/{worker_id}"
    seen_creation, seen_position, _ = cursor.split(":", 2)
    seen_position = int(seen_position)
    while not cancelled():
        status = board_status(config)
        channel = channel_status(status, inbox)
        if channel is None or channel["stopped"]:
            raise RecipeError(f"cannot watch inbox {inbox}")
        creation = channel["creation"]
        start = first_due(channel, worker_id)
        if seen_creation == creation:
            start = max(start, seen_position + 1)
        for position in range(start, channel["head"]):
            # GET can pass over a waiting note without acknowledging it.
            output = malak("--config", config, "get", "--json", "--agent", inbox, str(position),
                           handover=handover)
            messages = json.loads(output.stdout)["messages"]
            if messages and messages[0]["creation"] != creation:
                break
            if messages and messages[0]["presentation"] == "task":
                trace = messages[0].get("trace", "")
                return f"{creation}:{position}:{trace}"
        else:
            seen_creation, seen_position = creation, channel["head"] - 1
            time.sleep(1)


def main(action, *args):
    if action == "state":
        with open(args[0], "rb") as source:
            print(board_state(source))
    elif action == "lead-up":
        lead_up(*args)
    elif action == "list":
        list_boards(*args)
    elif action == "tree":
        tree_boards(*args)
    elif action == "answer":
        return answer(*args)
    elif action == "order":
        post_order(*args)
    elif action == "forward":
        return forward(*args)
    elif action == "wait-task":
        print(wait_task(*args))


if __name__ == "__main__":
    os.umask(0o077)
    try:
        sys.exit(main(*sys.argv[1:]))
    except (RecipeError, OSError, KeyError, ValueError) as error:
        action = sys.argv[1] if len(sys.argv) > 1 else "recipe"
        out.error(f"run {action}", error, "Check the board and the recipe inputs before retrying.")
        sys.exit(1)
