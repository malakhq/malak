"""Lead-side evidence and waiting for the stock worker recipes."""

from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
import secrets
import sys
import time

import board_data as board
import recipe_output as out
from recipe_board import handover_identity

DEFAULT_WAIT = 30 * 60
# Unread messages named in a report before the rest are only counted.
UNREAD_LISTED = 5
TICK = 0.2
NANOSECONDS = 1_000_000_000


class QueueError(board.RecipeError):
    """A bound queue operation could not safely continue."""


class Expired(QueueError):
    """The lead retains the prepared lane."""


class BoardChanged(QueueError):
    """The selected board can no longer prove continuity."""


def message_ref(record):
    fields = {key: record.get(key) for key in (
        "channel", "creation", "position", "id", "sender", "grade",
        "kind", "sent_at", "headers", "body",
    )}
    fingerprint = hashlib.sha256(json.dumps(fields, sort_keys=True).encode()).hexdigest()
    return {key: fields[key] for key in ("channel", "creation", "position", "id", "sent_at")} | {
        "fingerprint": fingerprint,
        "trace": (fields["headers"] or {}).get("trace"),
    }


def unread_words(status, participant, get_sender):
    """Say what a worker left unacknowledged in its inbox, such as "2 unread (lead #3, lead #4)".

    A message counts as unread until its reader acknowledges it, whether or not
    a read already delivered it. Returns None when nothing is left.
    """
    inbox = f"to/{participant}"
    channel = next((row for row in (status or {}).get("channels", []) if row["name"] == inbox), None)
    if channel is None:
        return None
    acknowledged = channel["readers"].get(participant)
    first = channel["floor"] if acknowledged is None else max(channel["floor"], acknowledged + 1)
    count = channel["head"] - first
    if count <= 0:
        return None
    listed = []
    for position in range(first, min(channel["head"], first + UNREAD_LISTED)):
        sender = get_sender(inbox, position)
        listed.append(f"{sender} #{position}" if sender else f"#{position}")
    if count > len(listed):
        listed.append(f"and {count - len(listed)} more")
    return f"{count} unread ({', '.join(listed)})"


def accepted_at(reference):
    seconds, nanos = reference["sent_at"]
    return seconds * NANOSECONDS + nanos


def from_receipt(record, receipt):
    return record is not None and all(
        record[key] == receipt[key] for key in ("id", "position", "creation")
    )


def is_worker_report(record, participant):
    return (record["sender"] == participant and record["grade"] == "proven"
            and record["kind"] == "report" and record["channel"] == "lead")


def active(status, participant):
    return any(person["id"] == participant and not person["revoked"]
               for person in status["participants"])


def named(status, name):
    return next((person["id"] for person in status["participants"]
                 if person["name"] == name and not person["revoked"]), None)


def prepare_worker_access(config, name, participant, additional_grants=()):
    inbox = f"to/{participant}"
    probe = f"selftest/{participant}"
    for channel in (f"relay/{name}", probe):
        board.malak("--config", config, "channel", "create", channel)
    board.malak("--config", config, "grant", participant, "lead:send", "relay/*:send",
                "questions:send", "orders:read,get", f"{inbox}:read,get,ack",
                f"{probe}:send,read,ack", *additional_grants)


@contextmanager
def select_enrolment(state, name):
    # Serialize stock enrolment and claim publication for this name.
    directory = state / "launches"
    directory.mkdir(mode=0o700, exist_ok=True)
    with (directory / f"name.{name}.lock").open("ab") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


class LaunchRecord:
    def __init__(self, state, participant):
        self.directory = state / "launches"
        self.id = participant
        self.path = self.directory / f"{participant}.json"
        self.lock = self.directory / f"{participant}.lock"

    @contextmanager
    def claim(self):
        self.directory.mkdir(mode=0o700, exist_ok=True)
        with self.lock.open("ab") as claimed:
            try:
                fcntl.flock(claimed, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise QueueError(f"lane {self.id} already has a launcher") from error
            yield

    def load(self):
        try:
            return json.loads(self.path.read_bytes())
        except FileNotFoundError:
            return None

    def observe(self):
        try:
            descriptor = self.lock.open("rb")
        except FileNotFoundError:
            return None, False
        with descriptor:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_SH | fcntl.LOCK_NB)
            except BlockingIOError:
                return self.load(), True
            return self.load(), False

    def save(self, evidence):
        temporary = self.path.with_suffix(".new")
        with temporary.open("wb") as output:
            output.write(json.dumps(evidence).encode())
            output.flush()
            os.fsync(output.fileno())
        temporary.replace(self.path)
        directory = os.open(self.directory, os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)

    def on_board(self, evidence, identity):
        return (evidence is not None and evidence["id"] == self.id
                and evidence["board"] == identity[2])


class QueueBoard:
    def __init__(self, config, state):
        self.config = config
        self.state = state
        self.lead = state / "lead.handover"
        self.identity = handover_identity(self.lead)

    def status(self):
        try:
            identity = handover_identity(self.lead)
        except FileNotFoundError as error:
            raise QueueError("board handover is unavailable", 3) from error
        if identity != self.identity:
            raise BoardChanged("board identity changed while the lane was waiting")
        return board.board_status(self.config)

    def get(self, channel, position, handover=None):
        return board.get_message(channel, str(position), config=self.config,
                                 handover=handover or self.lead, allow_missing=True)

    def unread(self, participant):
        """Describe the worker's unacknowledged inbox messages, or return None."""
        def sender(inbox, position):
            record = self.get(inbox, position)
            return record and record.get("sender_name")
        return unread_words(self.status(), participant, sender)

    def exact(self, reference, handover=None):
        record = self.get(reference["channel"], reference["position"], handover)
        if record is None or message_ref(record) != reference:
            raise QueueError(f"required record {reference['channel']} #{reference['position']} "
                             "is missing or does not match")
        return record

    def final_report(self, reference, participant):
        record = self.exact(reference)
        if not is_worker_report(record, participant):
            raise QueueError("the launcher’s final report does not match the worker")
        return record

    def send(self, channel, options, body):
        """Retry a lost reply only while the original creation and retry entry can survive."""
        identifier = secrets.token_hex(16)
        status = self.status()
        initial = board.channel_status(status, channel)
        if initial is None or initial["stopped"]:
            raise QueueError(f"channel {channel} is unavailable")
        request = ["--config", self.config, "send", "--json", "--id", identifier, *options,
                   "--", channel, "-"]
        try:
            result = board.malak(*request, handover=self.lead, body=body)
        except board.RecipeError as error:
            if error.code != 3:
                raise
            result = self.retry(request, channel, status, initial, body, error)
        receipt = json.loads(result.stdout)
        if receipt["id"] != identifier or receipt["creation"] != initial["creation"]:
            raise QueueError(f"cannot confirm the send to {channel}")
        return receipt

    def retry(self, request, channel, status, initial, body, error):
        try:
            current = self.status()
        except board.RecipeError as unavailable:
            raise QueueError(f"delivery to {channel} could not be confirmed: {unavailable}") from error
        now = board.channel_status(current, channel)
        if not board.may_retry_send(status["run"], initial, current["run"], now):
            raise QueueError(f"delivery to {channel} could not be confirmed safely") from error
        try:
            return board.malak(*request, handover=self.lead, body=body)
        except board.RecipeError as unconfirmed:
            if unconfirmed.code == 3:
                raise QueueError(f"delivery to {channel} could not be confirmed") from unconfirmed
            raise


def dependency(connection, status, name):
    participant = named(status, name)
    if not participant or participant == "lead":
        raise QueueError(f"no active predecessor named {name!r}")
    record = LaunchRecord(connection.state, participant).load()
    if (record is None or record["name"] != name or record["board"] != connection.identity[2]
            or not record["ready"]):
        raise QueueError(f"predecessor {name!r} has no ready stock launcher")
    return {"name": name, "id": participant, "trace": (record["brief"] or {}).get("trace")}


def wait_for_report(connection, predecessor, queued_id, deadline, cancelled):
    source = LaunchRecord(connection.state, predecessor["id"])
    while True:
        if cancelled():
            raise QueueError("queue cancelled")
        expired = time.time_ns() >= deadline
        evidence, live = source.observe()
        accepted = evidence and evidence["phase"] == "complete"
        reference = evidence["report"] if accepted else None
        timely = reference is not None and accepted_at(reference) <= deadline
        try:
            status = connection.status()
            if not active(status, queued_id):
                raise QueueError("the queued enrolment was revoked")
            if timely:
                return connection.final_report(reference, predecessor["id"])
        except board.RecipeError as error:
            if error.code != 3:
                raise
            if expired and not timely:
                raise Expired("deadline expired while final-report evidence was unavailable") from error
            time.sleep(TICK)
            continue

        failed = evidence and (evidence["phase"] == "failed" or evidence.get("exit_status"))
        reporting = live and evidence and evidence["phase"] == "reporting" and not failed
        if not reporting:
            if expired:
                raise Expired("deadline expired without an on-time final report")
            if accepted:
                time.sleep(TICK)
                continue
            if not active(status, predecessor["id"]):
                raise QueueError("the predecessor was revoked before a final report")
            if evidence is None or failed:
                raise QueueError((evidence or {}).get("reason", "the predecessor has no final report"))
            if not live and evidence["phase"] != "prepared":
                raise QueueError("the predecessor launcher stopped without a final report")
        time.sleep(TICK)


def await_report(connection, name):
    participant = named(connection.status(), name)
    if not participant or participant == "lead":
        raise QueueError(f"no active worker named {name!r}")
    source = LaunchRecord(connection.state, participant)
    while True:
        evidence, live = source.observe()
        if (evidence is None or evidence["name"] != name or evidence["id"] != participant
                or evidence["board"] != connection.identity[2]):
            raise QueueError(f"worker {name!r} has no stock launcher")
        if reference := evidence.get("report"):
            try:
                connection.status()
                record = connection.final_report(reference, participant)
                # Older stock launchers only recorded reports for a zero exit.
                return record, evidence.get("exit_status", 0)
            except board.RecipeError as error:
                if error.code != 3:
                    raise
        elif evidence["phase"] == "failed" or (not live and evidence["phase"] != "prepared"):
            reason = evidence.get("reason", "the launcher stopped without a final report")
            raise QueueError(f"worker {name!r}: {reason}")
        time.sleep(TICK)


def verify_handoff(connection, brief, handover, forwarded=None):
    """Check both identities together using prefix retention in one board run."""
    participant = brief["channel"].removeprefix("to/")
    while True:
        before = connection.status()
        if not active(before, participant):
            raise QueueError("the queued enrolment was revoked")
        if forwarded is not None:
            # Retention removes a prefix. Read the newer note first; if the
            # older brief is still readable afterward, both remain at that point.
            connection.exact(forwarded, handover)
        connection.exact(brief, handover)
        after = connection.status()
        if before["run"] != after["run"]:
            continue
        inbox = board.channel_status(after, brief["channel"])
        if (not active(after, participant)
                or not inbox or inbox["stopped"] or inbox["creation"] != brief["creation"]
                or inbox["floor"] > brief["position"]):
            raise QueueError("the queued enrolment or its original inbox history is unavailable")
        return


def forward(connection, original, inbox):
    options, body = board.forward_record(original)
    receipt = connection.send(inbox, options, body)
    record = connection.get(inbox, receipt["position"])
    reply = [original["channel"], original["creation"], original["position"]]
    headers = record.get("headers", {}) if record else {}
    if (not from_receipt(record, receipt) or record["sender"] != "lead" or record["grade"] != "proven"
            or record["kind"] != "note" or bytes(record["body"]) != body
            or headers.get("reply_to") != reply
            or headers.get("trace") != original.get("headers", {}).get("trace")):
        raise QueueError("the forwarded report could not be confirmed")
    out.receipt(f"Forwarded lead #{original['position']} to {inbox}.", receipt)
    sys.stdout.flush()
    return message_ref(record)


def announce_outcome(connection, evidence, reason, expired):
    worker = evidence["name"]
    inbox = f"to/{evidence['id']}"
    other = evidence["dependency"]["name"]
    title = f"Queue for {worker} behind {other} ended."
    out.details(title, (("Inbox", inbox), ("Reason", reason)), stream=sys.stderr)
    options = ["--kind", "note" if expired else "report"]
    if trace := (evidence.get("brief") or {}).get("trace"):
        options.extend(("--trace", trace))
    payload = f"Queue for {worker} ({inbox}) behind {other} ended without launch. {reason}"
    try:
        while True:
            try:
                status = connection.status()
                if not expired and active(status, evidence["id"]):
                    board.malak("--config", connection.config, "revoke", evidence["id"])
                break
            except board.RecipeError as error:
                if error.code != 3:
                    raise
                time.sleep(TICK)
        receipt = connection.send("lead", options, payload.encode())
        out.receipt("Sent queue outcome to lead.", receipt)
    except (board.RecipeError, OSError) as error:
        out.details("Queue outcome delivery could not be confirmed.", (("Reason", error),),
                    stream=sys.stderr)
