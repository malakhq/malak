"""Write rail facts as lead notes; callers own their lifecycle event."""

import json
import hashlib
from pathlib import Path
import time

import board_data as board
from recipe_board import board_config, board_name, board_settings

EVENTS = {"started", "finished", "merged", "blocked", "unblocked", "waits on", "wait done", "handed over", "decided", "lead removed", "chief of staff switched"}


def emit(config, event, subject, link, other_board=None, handover=None, work_link=None):
    if event not in EVENTS:
        raise board.RecipeError(f"unknown rail event {event!r}")
    if not subject or any("\n" in value or "\r" in value for value in (subject, other_board or "")):
        raise board.RecipeError("rail subjects and board names must be one nonempty line")
    if isinstance(link, dict):
        link = [board_name(Path(config)), link["channel"], link["creation"], link["position"]]
    if not isinstance(link, list) or len(link) != 4:
        raise board.RecipeError("rail link must name board, channel, creation and position")
    encoded = json.dumps(link, separators=(",", ":"))
    body = f"Type: rail\nEvent: {event}\nSubject: {subject}\nLink: {encoded}\n"
    if other_board is not None:
        body += f"Other board: {other_board}\n"
    if work_link is not None:
        if not event_link({"Link": json.dumps(work_link)}):
            raise board.RecipeError("work link must name board, channel, creation and position")
        body += "Work link: " + json.dumps(work_link, separators=(",", ":")) + "\n"
    board.malak("--config", config, "channel", "create", "rail")
    identifier = (f"rail-finished-{link[2]}-{link[3]}" if event == "finished"
                  else "rail-" + hashlib.sha256(body.encode()).hexdigest())
    return json.loads(board.malak("--config", config, "send", "--json", "--id", identifier, "--kind", "note", "rail", "-",
                                 handover=handover, body=body).stdout)


def trim(config, roster, all_boards=False):
    import os
    from recipe_board import config_for_socket, handover_identity

    cutoff = int(time.time() * 1000) - roster["rail_age_days"] * 86_400_000
    credential = Path(os.environ.get("MALAK_HANDOVER") or Path(board_settings(Path(config))["state"]) / "lead.handover")
    socket, holder, _ = handover_identity(credential)
    if holder != "lead":
        raise board.RecipeError("org trim needs the invoking lead's or chief of staff's reading Seat")
    home = config_for_socket(socket)
    if home is None:
        raise board.RecipeError("cannot find the invoking reader's home board")
    board.check_board(home, credential)
    reader = next((f"lead:{name}" for name, lead in roster["leads"].items()
                   if lead["active"] and Path(lead["config"]) == home), None)
    if Path(roster["top"]) == home and roster.get("chief_of_staff"):
        reader = "chief_of_staff:" + roster["chief_of_staff"]["name"]
    seats = roster["seats"].get(reader, [])
    configs = ([Path(roster["top"]), *(Path(lead["config"]) for lead in roster["leads"].values()
                                     if lead["active"])] if all_boards else [Path(config)])
    # Check every read path before any owner mutation.
    readers = []
    for target in configs:
        handover = credential if target == home else next(
            (Path(seat["handover"]) for seat in seats if Path(seat["config"]) == target), None)
        if handover is None:
            raise board.RecipeError(f"no authorised read seat for rail on {target}; org trim did not run")
        board.check_board(target, handover)
        readers.append((target, handover))
    for target, handover in readers:
        status = board.board_status(target)
        channel = board.channel_status(status, "rail")
        if channel is None:
            continue
        import org_state
        with org_state.publication(target, channel["floor"]) as published:
            before = channel["floor"]
            for position in range(channel["floor"], channel["head"]):
                record = board.get_message("rail", str(position), config=target, handover=handover, allow_missing=True)
                if record is None:
                    continue
                seconds, nanos = record["sent_at"]
                if seconds * 1000 + nanos // 1_000_000 >= cutoff:
                    break
                fields = rail_fields(record)
                if fields and "Subject" in fields and fields.get("Event") in ("started", "finished"):
                    link = event_link(fields)
                    if link and enrolment(target, fields["Event"], link, handover):
                        published.remember(fields["Event"], link)
                before = position + 1
            if before > channel["floor"]:
                # Save exact source links before removing their rail notes.
                published.save()
                board.malak("--config", target, "trim", "--creation", channel["creation"], "rail", str(before))


def rail_fields(record):
    if not record or record["sender"] != "lead" or record["grade"] != "proven" or record["kind"] != "note":
        return None
    lines = bytes(record["body"]).decode(errors="replace").splitlines()
    if not lines or lines[0] != "Type: rail":
        return None
    fields = {}
    for line in lines[1:]:
        if not line or ": " not in line:
            break
        key, value = line.split(": ", 1)
        if key in fields:
            return None
        fields[key] = value
    return fields


def event_link(fields):
    try:
        link = json.loads(fields["Link"])
        if (isinstance(link, list) and len(link) == 4
                and all(isinstance(value, str) for value in link[:3])
                and type(link[3]) is int and 0 < link[3] < 2**64):
            return link
    except (KeyError, ValueError):
        pass
    return None


def enrolment(config, event, link, handover):
    import re
    if link[0] != board_name(Path(config)):
        return None
    try:
        fact = board.get_message(link[1], str(link[3]), config=config, handover=handover, allow_missing=True)
    except (board.RecipeError, OSError, ValueError):
        return None
    if not fact or fact["creation"] != link[2] or fact["grade"] != "proven":
        return None
    worker = lambda value: re.fullmatch(r"[a-fA-F0-9]{32}", value) is not None
    sender = fact["sender"]
    if event == "finished":
        return sender if worker(sender) and link[1] == "lead" and fact["kind"] == "report" else None
    if sender == "lead" and fact["kind"] == "task" and re.fullmatch(r"to/[a-fA-F0-9]{32}", link[1]):
        candidate = link[1][3:]
        return candidate if worker(candidate) else None
    if worker(sender):
        return sender
    if link[1] == "lead":
        if sender == "malakowner" and (match := re.fullmatch(r"claim-([a-fA-F0-9]{32})-.+-accepted", fact["id"])):
            return match[1]
    return None


def work_identity(config, link, handover):
    if not event_link({"Link": json.dumps(link)}):
        return None
    try:
        target = config if link[0] == board_name(Path(config)) else board_config(link[0])
        fact = board.get_message(link[1], str(link[3]), config=target, handover=handover, allow_missing=True)
    except (board.RecipeError, OSError, ValueError):
        return None
    if not fact or fact["creation"] != link[2] or fact["grade"] != "proven":
        return None
    import re
    sender = fact["sender"]
    reply = fact.get("headers", {}).get("reply_to")
    if sender == "lead" and fact["kind"] == "note" and reply and reply[0] == "questions":
        # Use the shared reader's decision proof, never a copied attribution.
        # This also covers peer answers loaded while checking a linked wait.
        try:
            shown = json.loads(board.malak("--config", target, "get", "--agent", "--json",
                "--creation", link[2], link[1], str(link[3]), handover=handover).stdout)
        except (board.RecipeError, OSError, ValueError):
            return None
        if not any(message.get("decided_by") for message in shown.get("messages", [])):
            return None
        return ("fact", link[0], *reply)
    if link[1] == "questions":
        return ("fact", *link)
    worker = re.fullmatch(r"[a-fA-F0-9]{32}", sender)
    if worker:
        return ("worker", link[0], sender)
    if sender == "lead" and fact["kind"] == "task" and re.fullmatch(r"to/[a-fA-F0-9]{32}", link[1]):
        return ("worker", link[0], link[1][3:])
    accepted = re.fullmatch(r"claim-([a-fA-F0-9]{32})-.+-accepted", fact["id"])
    if sender == "malakowner" and link[1] == "lead" and accepted:
        return ("worker", link[0], accepted[1])
    return ("fact", *link)


def rail_records(config, handover):
    channel = board.channel_status(board.board_status(config), "rail")
    if channel is None:
        return []
    return [record for position in range(channel["floor"], channel["head"])
            if (record := board.get_message("rail", str(position), config=config, handover=handover, allow_missing=True))]


def closes_work(config, record, handover):
    fields = rail_fields(record)
    if not fields or fields.get("Event") not in ("finished", "unblocked", "handed over"):
        return None
    link = event_link(fields)
    if not link or (fields["Event"] == "finished" and not enrolment(config, "finished", link, handover)):
        return None
    return work_identity(config, link, handover)


def open_wait(config, subject, handover=None):
    # This current lead publishes pending facts before selecting the snapshot.
    board.get_message("rail", "1", config=config, handover=handover, allow_missing=True)
    waits = {}
    lifecycle = set()
    for record in rail_records(config, handover):
        fields = rail_fields(record)
        if fields is None or "Subject" not in fields or not (link := event_link(fields)):
            continue
        event = fields.get("Event")
        identity = (event, tuple(link))
        if event in ("started", "finished"):
            if not enrolment(config, event, link, handover) or identity in lifecycle:
                continue
            lifecycle.add(identity)
        closed = closes_work(config, record, handover)
        if closed is not None:
            waits = {key: wait for key, wait in waits.items() if wait[1] != closed}
        if event == "wait done" and fields["Subject"] == subject and link[:2] == [board_name(Path(config)), "rail"]:
            waits.pop((link[2], link[3]), None)
        elif event == "waits on" and fields["Subject"] == subject:
            work = event_link({"Link": fields.get("Work link", "null")})
            if "Work link" in fields and work is None:
                continue
            waits[(record["creation"], record["position"])] = (record, work_identity(config, work, handover) if work else None, work)
    peers = {}
    for key, (_, identity, link) in list(waits.items()):
        if not link or identity is None or link[0] == board_name(Path(config)):
            continue
        if link[0] not in peers:
            try:
                target = board_config(link[0])
                peers[link[0]] = [closes_work(target, record, handover) for record in rail_records(target, handover)]
            except (board.RecipeError, OSError, ValueError):
                peers[link[0]] = []
        if identity in peers[link[0]]:
            waits.pop(key)
    if waits:
        return next(reversed(waits.values()))[0]
    raise board.RecipeError("no rail wait is open for that subject")


def lead_session(status):
    person = next(person for person in status["participants"] if person["id"] == "lead")
    claim = person.get("claim")
    if claim is None:
        return None
    return (claim["state"], claim["session"].get("pid"), claim["session"].get("start_time"))


def replay_finished(config, subject, link, handover=None):
    """Replay only the original, proven launcher exit from this board."""
    import os
    from recipe_board import handover_identity
    from worker_queue import LaunchRecord, is_worker_report, message_ref

    config = Path(config)
    credential = handover or os.environ.get("MALAK_HANDOVER") or Path(board_settings(config)["state"]) / "lead.handover"
    status = board.board_status(config)
    session = lead_session(status)
    if session is None or session[0] != "bound":
        raise board.RecipeError("only the board's current bound lead may replay finished")
    board.check_board(config, credential)
    if link[0] != board_name(config) or link[1] != "lead":
        raise board.RecipeError("finished must link the original exit report on this board's lead channel")
    record = board.get_message("lead", str(link[3]), config=config, handover=credential, allow_missing=True)
    if record is None or record["creation"] != link[2] or not is_worker_report(record, record["sender"]):
        raise board.RecipeError("finished requires a proven worker exit report")
    launch = LaunchRecord(Path(board_settings(config)["state"]), record["sender"])
    evidence, _ = launch.observe()
    if not launch.on_board(evidence, handover_identity(credential)) or message_ref(record) not in (
        evidence.get("report"), evidence.get("last_report")
    ):
        raise board.RecipeError("the linked report is not a verified launcher exit")
    channel = board.channel_status(board.board_status(config), "rail")
    import org_state
    with org_state.publication(config, channel["floor"] if channel else 1) as published:
        if channel:
            for position in range(channel["floor"], channel["head"]):
                previous = board.get_message("rail", str(position), config=config, handover=credential, allow_missing=True)
                fields = rail_fields(previous)
                if fields is None or "Subject" not in fields or fields.get("Event") != "finished":
                    continue
                try:
                    same = json.loads(fields["Link"]) == link
                except (KeyError, ValueError):
                    same = False
                if same:
                    published.remember("finished", link)
                    published.save()
                    return {"id": previous["id"], "creation": previous["creation"],
                            "position": previous["position"], "durability": previous["durability"], "duplicate": True}
        if published.contains("finished", link):
            return None
        receipt = emit(config, "finished", subject, link, handover=credential)
        published.remember("finished", link)
        published.save()
        return receipt
