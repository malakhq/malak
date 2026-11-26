"""Shared enrolment and launch path for worker-start and queue."""

import argparse
from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

import adapters
import board_data as board
from agent_launch import prepare_gate, start_worker, worker_command
from lead_status import watcher_running
from recipe_board import (
    LEAD_WATCHER_CONSEQUENCE, board_config, board_name, handover_identity, lead_watcher_command,
    selected_config,
)
import recipe_output as out
from worker_queue import (
    DEFAULT_WAIT, NANOSECONDS, Expired, LaunchRecord, QueueBoard, QueueError, active,
    announce_outcome, dependency, forward, from_receipt, message_ref, named,
    is_worker_report, prepare_worker_access, select_enrolment, verify_handoff, wait_for_report,
)

RECIPES = Path(__file__).resolve().parent
REPORT_RETRY_SECONDS = 60
# How long a failed launch waits for the board to see its session end.
CLAIM_END_WAIT = 5
# Private status from the CLI's launcher-report send path.
REPORT_CONNECTION_EXIT = 75
SELECT_BOARD = "[--config PATH | --board NAME]"
USAGE = {
    "worker-start": f"malak recipe worker-start NAME [--again | --prepared] {SELECT_BOARD} "
                    "[--brief FILE] [--detach] [--unbound] [--resume CMD] [--no-watcher] -- CMD…",
    "queue": f"malak recipe queue NAME --after OTHER (--brief FILE | --prepared) {SELECT_BOARD} "
             "[--wait SECONDS] [--resume CMD] [--no-watcher] -- CMD…",
}


def report_attempt(command, env, timeout):
    # The shell can pipe the result through a separate malak process. Bound
    # the whole process group so a stalled link cannot hold the launcher.
    with subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          process_group=0) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, stderr = process.communicate()
            return subprocess.CompletedProcess(command, REPORT_CONNECTION_EXIT, b"",
                                               stderr + b"The worker report call timed out.\n")
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def send_final_report(command, env):
    window = int(env.get("MALAK_TEST_REPORT_RETRY_SECONDS", REPORT_RETRY_SECONDS))
    window = min(max(window, 1), REPORT_RETRY_SECONDS)
    call_timeout = min(10, window / 2)
    deadline = time.monotonic() + window
    delay = 0.1
    attempt = 1
    result = report_attempt(command, env, min(call_timeout, max(0, deadline - time.monotonic())))
    while result.returncode == REPORT_CONNECTION_EXIT:
        # Even if scheduling ate the window, give the second call time to start.
        retry_floor = 0.1 if attempt == 1 else 0
        pause = min(delay, deadline - time.monotonic() - retry_floor)
        if pause > 0:
            time.sleep(pause)
        remaining = deadline - time.monotonic()
        if attempt > 1 and remaining <= 0:
            break
        attempt += 1
        out.details("Board connection unavailable. Retrying the worker report.",
                    (("Attempt", attempt),), stream=sys.stderr)
        result = report_attempt(command, env, min(call_timeout, max(retry_floor, remaining)))
        delay = min(delay * 2, 2)
    if result.stderr:
        sys.stderr.buffer.write(result.stderr)
    if result.returncode == REPORT_CONNECTION_EXIT:
        result.returncode = 3
    return result


class Parser(argparse.ArgumentParser):
    def error(self, message):
        raise QueueError(message)


def arguments(mode, words):
    parser = Parser(add_help=False, allow_abbrev=False)
    parser.add_argument("name")
    selectors = parser.add_mutually_exclusive_group()
    selectors.add_argument("--board", metavar="NAME")
    selectors.add_argument("--config", type=Path)
    parser.add_argument("--resume")
    parser.add_argument("--no-watcher", action="store_true")
    if mode == "queue":
        parser.add_argument("--after", required=True)
        parser.add_argument("--wait", type=int, default=DEFAULT_WAIT)
        briefs = parser.add_mutually_exclusive_group(required=True)
        briefs.add_argument("--brief", type=Path)
        briefs.add_argument("--prepared", action="store_true")
    else:
        options = parser.add_mutually_exclusive_group()
        options.add_argument("--again", action="store_true")
        options.add_argument("--prepared", action="store_true")
        parser.add_argument("--brief", type=Path)
        parser.add_argument("--detach", action="store_true")
        parser.add_argument("--unbound", action="store_true")
    try:
        separator = words.index("--")
    except ValueError:
        separator = len(words)
    if any(word in ("-h", "--help") for word in words[:separator]):
        lines = [f"Usage: {USAGE[mode]}"]
        if mode == "queue":
            lines.append(f"The default wait is {DEFAULT_WAIT // 60} minutes.")
        else:
            lines.extend([
                "",
                f"--brief FILE posts a task to the worker's inbox. {adapters.names('and')}",
                "hooks deliver it at session start. The command's prompt need not repeat it.",
                "--again retires the previous enrolment once its launcher has exited.",
                "--detach returns once the command starts and prints how to follow or stop it.",
                "--unbound allows several local processes to use the handover. Required for --resume.",
            ])
        lines.append("--no-watcher allows scripts and tests to run without a lead watcher.")
        out.usage(lines)
        return None
    args = parser.parse_args(words[:separator])
    args.command = words[separator + 1:]
    if not args.command:
        raise QueueError("supply a command after --")
    if not re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}", args.name):
        raise QueueError(f"invalid participant name {args.name!r}")
    if mode == "worker-start" and args.resume and not args.unbound:
        raise QueueError("--resume requires --unbound")
    if args.prepared and args.brief:
        raise QueueError("--prepared uses the original queued brief")
    if mode == "queue":
        if args.wait <= 0:
            raise QueueError("--wait must be a positive whole number of seconds")
        if args.after == args.name:
            raise QueueError("a lane cannot wait on itself")
    if args.brief is not None and (not args.brief.is_file() or not os.access(args.brief, os.R_OK)):
        raise QueueError(f"cannot read worker brief {args.brief}. Use --brief with a readable file.")
    args.config = (selected_config(args.board, args.config) or board_config()).absolute()
    return args


class Worker:
    def __init__(self, connection, args, mode):
        self.connection = connection
        self.config = connection.config
        self.args = args
        self.mode = mode
        self.hook_state = os.environ.pop("MALAK_RECIPE_HOOK_STATE", None)
        self.demo_report = os.environ.pop("MALAK_DEMO_REPORT_RECORD", None)
        self.demo_tmp = os.environ.pop("MALAK_DEMO_AGENT_TMPDIR", None)
        self.demo_handover = os.environ.pop("MALAK_DEMO_HANDOVER", None)
        self.parent = os.environ.pop("MALAK_QUEUE_PARENT_PID", None)
        self.detached_bundle = os.environ.pop("MALAK_RECIPE_DETACHED_BUNDLE", None)
        if not self.hook_state:
            raise QueueError(f"start the worker through malak recipe {mode}")
        if getattr(args, "detach", False) != bool(self.detached_bundle):
            raise QueueError("start detached workers through malak recipe worker-start")
        self.bound = (mode == "worker-start" and not args.unbound
                      and not args.prepared and not self.demo_handover)
        self.gate = None
        self.started = False
        self.cancelled = False
        self.brief_confirmed = None

    def cancel(self, *_):
        self.cancelled = True

    def interrupted(self):
        if self.parent and str(os.getppid()) != self.parent:
            # The foreground command was killed. Release the claim without
            # simulating an acknowledged cancellation.
            raise SystemExit(3)
        return self.cancelled

    def select_prepared(self, status):
        participant = named(status, self.args.name)
        if not participant:
            raise QueueError(f"no active prepared worker named {self.args.name!r}")
        self.record = LaunchRecord(self.connection.state, participant)
        return self.record.claim()

    def load_prepared(self):
        self.evidence = self.record.load()
        if (not self.evidence or self.evidence["board"] != self.connection.identity[2]
                or not self.evidence["dependency"] or not self.evidence["brief"]
                or self.evidence["phase"] not in ("prepared", "waiting")):
            raise QueueError(f"lane {self.args.name!r} is not eligible as a prepared worker")
        if self.mode == "queue" and self.args.after != self.evidence["dependency"]["name"]:
            raise QueueError("--prepared must use the original predecessor")
        self.private = Path(self.evidence["directory"])
        self.handover = self.private / "handover"
        verify_handoff(self.connection, self.evidence["brief"], self.handover)

    def retry_previous(self, status):
        history = [person for person in status["participants"]
                   if person["name"] == self.args.name and person["id"] != "lead"]
        if not history:
            raise QueueError(f"no earlier enrolment for worker {self.args.name!r}")
        for person in history:
            evidence, live = LaunchRecord(self.connection.state, person["id"]).observe()
            if live:
                pid = (evidence or {}).get("launcher_pid")
                launcher = f"launcher PID {pid}" if pid else "a launcher (PID unavailable)"
                raise QueueError(f"worker {self.args.name!r} is still running under {launcher}")
        current = named(status, self.args.name)
        if not self.args.brief:
            latest = max(history, key=lambda person: (person.get("enrolled_at_ms") or 0,
                                                      person.get("revoked_at_ms") or 0))
            saved = self.private / "retry-brief"
            board.recover_worker_brief(self.config, self.connection.state, status, current or latest["id"], saved)
            if saved.is_file():
                self.args.brief = saved
        if current:
            board.malak("--config", self.config, "revoke", current)

    def enrol(self, status):
        self.private = Path(tempfile.mkdtemp(prefix="worker.", dir=self.connection.state))
        self.handover = self.private / "handover"
        try:
            if getattr(self.args, "again", False):
                self.retry_previous(status)
            binding = ["--unbound"]
            if self.bound:
                self.gated_run = Path(tempfile.mkdtemp(prefix="result.", dir=self.private))
                with (self.gated_run / "out").open("wb") as output, (self.gated_run / "err").open("wb") as errors:
                    self.gate = prepare_gate(stdout=output, stderr=errors)
                binding = ["--session", str(self.gate[0].pid),
                           "--launch-program", Path(self.args.command[0]).name]
            return board.enrol_lead_sender(self.config, self.args.name, self.handover, *binding)
        except (board.RecipeError, OSError):
            self.close_gate()
            shutil.rmtree(self.private)
            raise

    def use_demo_enrolment(self, status):
        self.handover = Path(self.demo_handover)
        self.private = self.handover.parent
        socket, participant, pin = handover_identity(self.handover)
        if ((socket, "lead", pin) != self.connection.identity
                or named(status, self.args.name) != participant):
            raise QueueError("the demo worker handover does not match its enrolment")
        return participant

    def claim_enrolment(self, participant, predecessor):
        self.record = LaunchRecord(self.connection.state, participant)
        self.evidence = {
            "id": participant, "name": self.args.name, "directory": str(self.private),
            "board": self.connection.identity[2], "dependency": predecessor, "brief": None,
            "ready": False, "phase": "preparing",
        }
        return self.record.claim()

    @property
    def inbox(self):
        return f"to/{self.record.id}"

    def prepare(self):
        try:
            prepare_worker_access(self.config, self.args.name, self.record.id)
            if self.args.brief is None:
                return
            payload = f"Your inbox: {self.inbox}\n".encode() + self.args.brief.read_bytes()
            options = ["--kind", "task"]
            if trace := (self.evidence["dependency"] or {}).get("trace"):
                options.extend(("--trace", trace))
            receipt = self.connection.send(self.inbox, options, payload)
            self.brief_confirmed = time.time_ns()
            record = self.connection.get(self.inbox, receipt["position"],
                                         None if self.bound else self.handover)
            if (not from_receipt(record, receipt) or record["sender"] != "lead"
                    or record["grade"] != "proven" or record["kind"] != "task"
                    or bytes(record["body"]) != payload):
                raise QueueError("the original brief could not be confirmed")
            self.evidence["brief"] = message_ref(record)
        finally:
            (self.private / "retry-brief").unlink(missing_ok=True)

    def phase(self, phase, **fields):
        self.evidence.update(phase=phase, **fields)
        self.record.save(self.evidence)

    def wait(self):
        self.phase("waiting", ready=True)
        brief = self.evidence["brief"]
        predecessor = self.evidence["dependency"]
        out.details(
            f"Worker {self.args.name} waiting for {predecessor['name']}.",
            (("Inbox", self.inbox), ("Brief", f"#{brief['position']}"),
             ("Creation", brief["creation"]), ("Trace", brief["trace"] or "")),
        )
        sys.stdout.flush()
        start = self.brief_confirmed if self.brief_confirmed is not None else time.time_ns()
        original = wait_for_report(
            self.connection, predecessor, self.record.id, start + self.args.wait * NANOSECONDS,
            self.interrupted,
        )
        self.connection.exact(brief, self.handover)
        forwarded = forward(self.connection, original, self.inbox)
        verify_handoff(self.connection, brief, self.handover, forwarded)

    def handoff(self):
        if self.mode != "queue":
            self.phase("handed-off", ready=True)
            return
        signals = {signal.SIGINT, signal.SIGTERM}
        previous = signal.pthread_sigmask(signal.SIG_BLOCK, signals)
        try:
            if self.interrupted() or signals & signal.sigpending():
                raise QueueError("queue cancelled")
            self.phase("handed-off", ready=True)
        finally:
            signal.pthread_sigmask(signal.SIG_SETMASK, previous)

    def execute(self):
        if not self.args.no_watcher and not self.watcher_allows_launch():
            return 3
        with ExitStack() as claims:
            with select_enrolment(self.connection.state, self.args.name):
                self.claim(status=self.connection.status(), claims=claims)
            return self.execute_claimed()

    def watcher_allows_launch(self):
        running = watcher_running(self.config)
        if running is None:
            out.details("Warning: Could not tell whether the lead watcher is running. Continuing.",
                        stream=sys.stderr)
        if running is not False:
            return True
        name = board_name(self.config)
        command = lead_watcher_command(self.config, name)
        out.error(
            f"run {self.mode}",
            f"no lead watcher is running on board {name}; {LEAD_WATCHER_CONSEQUENCE}",
            f"start it in the background with `{command}`, then run {self.mode} again (or pass --no-watcher).",
        )
        return False

    def claim(self, status, claims):
        if self.args.prepared:
            claims.enter_context(self.select_prepared(status))
            self.load_prepared()
        else:
            predecessor = None
            if self.mode == "queue":
                predecessor = dependency(self.connection, status, self.args.after)
                if named(status, self.args.name) is not None:
                    raise QueueError(f"worker {self.args.name!r} already has an active enrolment")
            participant = (self.use_demo_enrolment(status) if self.demo_handover
                           else self.enrol(status))
            claims.enter_context(self.claim_enrolment(participant, predecessor))
        self.phase(self.evidence["phase"], launcher_pid=os.getpid(), agent_pid=None,
                   started_at_ms=int(time.time() * 1000), last_exit_status=None)

    def execute_claimed(self):
        if self.mode == "queue":
            signal.signal(signal.SIGINT, self.cancel)
            signal.signal(signal.SIGTERM, self.cancel)
        try:
            if not self.args.prepared:
                self.prepare()
            if self.mode == "queue":
                self.wait()
            self.handoff()
        except (board.RecipeError, OSError, KeyError, ValueError) as error:
            failure = QueueError("queue cancelled") if self.cancelled else error
            self.phase("prepared" if isinstance(failure, Expired) else "failed", reason=str(failure))
            if self.mode != "queue":
                self.revoke()
                raise
            if not isinstance(failure, Expired):
                self.handover.unlink(missing_ok=True)
            announce_outcome(self.connection, self.evidence, str(failure), isinstance(failure, Expired))
            return 3
        if self.mode == "queue":
            signal.signal(signal.SIGINT, signal.SIG_DFL)
            signal.signal(signal.SIGTERM, signal.SIG_DFL)
        if not self.args.prepared and self.mode == "worker-start" and not self.detached_bundle:
            fields = [("Inbox", self.inbox), ("Handover", self.handover)]
            if self.evidence["brief"]:
                fields.append(("Brief", "Posted"))
            out.details(f"Worker {self.args.name} ready.", fields)
            sys.stdout.flush()
        result, sent = self.run(self.args.command, self.cursor(), initial=True)
        if self.started and self.args.resume and sent == 0 and not self.cancelled:
            return self.resume()
        return result

    def cursor(self):
        if brief := self.evidence["brief"]:
            return f"{brief['creation']}:{brief['position']}:{brief['trace'] or ''}"
        return ":0:"

    def revoke(self):
        # The demo supervisor verifies retained reports before revoking its workers.
        if self.demo_handover:
            return
        self.end_claim()
        try:
            board.malak("--config", self.config, "revoke", self.record.id)
            out.details(f"Revoked worker {self.args.name} after launch failure.",
                        (("Inbox", self.inbox),), stream=sys.stderr)
        except (board.RecipeError, OSError) as error:
            out.details(f"Error: Could not confirm revocation of worker {self.args.name}.",
                        (("Inbox", self.inbox), ("Reason", error),
                         ("Next", f"Use malak revoke {self.record.id} on the selected board.")),
                        stream=sys.stderr)
        finally:
            self.handover.unlink(missing_ok=True)

    def end_claim(self):
        """Let the board see the launch's session end before the worker is revoked.

        The board records the end of a claim only for an enrolment that is not
        revoked, so revoking first leaves the claim bound to a process that is gone.
        """
        if not self.bound:
            return
        self.close_gate()
        deadline = time.monotonic() + CLAIM_END_WAIT
        while time.monotonic() < deadline:
            try:
                people = self.connection.status()["participants"]
            except (board.RecipeError, OSError, KeyError, ValueError):
                return
            claim = next((person.get("claim") for person in people if person["id"] == self.record.id), None)
            if (claim or {}).get("state") != "bound":
                return
            time.sleep(0.05)

    def environment(self, cursor):
        creation, position, trace = cursor.split(":", 2)
        env = os.environ | {
            "MALAK_HANDOVER": str(self.handover), "MALAK_PARTICIPANT_ID": self.record.id,
            "MALAK_HOOK_INBOX": self.inbox, "MALAK_HOOK_STATE": self.hook_state,
            "MALAK_TASK_INBOX": self.inbox, "MALAK_TASK_CREATION": creation,
            "MALAK_TASK_POSITION": position, "MALAK_TASK_TRACE": trace,
        }
        if self.demo_tmp:
            env["TMPDIR"] = self.demo_tmp
        return env

    def report(self, run, status, env, initial):
        identifier = f"{self.record.id}-{secrets.token_hex(16)}"
        if self.demo_report:
            Path(self.demo_report).write_text(f"{self.record.id}\n{identifier}\n{status}\n")
        failure = None
        if not self.started:
            failure = "the predecessor failed to launch"
        elif status:
            failure = f"the predecessor exited with status {status}"
        # Await and queue keep the first report. Status follows the latest child.
        if initial:
            self.phase("reporting", exit_status=status, last_exit_status=status, reason=failure)
        else:
            self.phase(self.evidence["phase"], last_exit_status=status)
        env = env | {
            "MALAK_WORKER_HANDOVER": str(self.handover), "MALAK_REPORT_ID": identifier,
            "MALAK_RECIPE_CONFIG": str(self.config), "MALAK_REPORT_JSON": "1",
            "MALAK_RECIPE_REPORT_RETRY": "1",
        }
        if self.bound:
            command = [board.MALAK_EXE, "--config", self.config, "recipe", "__worker-exit",
                       self.record.id, str(status), run / "out", env["MALAK_TASK_TRACE"]]
        else:
            command = [RECIPES / "report.sh", str(status), run / "out"]
        result = send_final_report(command, env)
        if not result.returncode:
            receipt = json.loads(result.stdout)
            if self.bound:
                identifier = receipt["id"]
            fields = (("Trace", receipt["trace"]),) if receipt.get("trace") else ()
            action = "already recorded in" if receipt["duplicate"] else "sent to"
            out.receipt(f"Report {action} lead #{receipt['position']}.", receipt, fields)
            sys.stdout.flush()
            try:
                record = (receipt["report"] if self.bound
                          else self.connection.get("lead", receipt["position"]))
                if (not from_receipt(record, receipt) or receipt["id"] != identifier
                        or not is_worker_report(record, self.record.id)):
                    raise QueueError("the launcher's final report could not be verified")
                reference = message_ref(record)
                if initial:
                    self.phase("failed" if failure else "complete", report=reference)
                else:
                    self.phase(self.evidence["phase"], last_report=reference)
            except (board.RecipeError, OSError, KeyError, ValueError) as error:
                out.details("Could not verify the launcher’s final report.",
                            (("Reason", error),), stream=sys.stderr)
                if initial:
                    self.phase("failed", reason=failure or str(error))
        if result.returncode and initial:
            self.phase("failed", reason=failure or "the predecessor's final report could not be confirmed")
        if result.returncode and self.started:
            out.details("Error: Could not send the worker report to lead.",
                        (("Result", run / "out"), ("Worker exit", status), ("Report ID", identifier),
                         ("Report exit", result.returncode), ("Next", "Retry the report with the same ID.")),
                        stream=sys.stderr)
        elif result.returncode:
            out.details("Error: Could not send the launch failure to lead.",
                        (("Inbox", self.inbox), ("Worker exit", status)), stream=sys.stderr)
        return result.returncode

    def announce_detached(self, run, errors):
        selector = (shlex.join(["--board", self.args.board]) if self.args.board
                    else shlex.join(["--config", str(self.config)]))
        group = os.getpgrp()
        await_command = f"malak {selector} recipe await {shlex.quote(self.args.name)}"
        # The external kill accepts the option separator before a negative PID.
        stop_command = f"/bin/kill -TERM -- -{group}"
        sys.stdout.flush()
        sys.stderr.flush()
        signal.signal(signal.SIGTERM, self.cancel)
        try:
            with os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8") as readiness:
                os.dup2(errors.fileno(), sys.stdout.fileno())
                os.dup2(errors.fileno(), sys.stderr.fileno())
                print("malak-worker-started", file=readiness)
                out.details(
                    f"Worker {self.args.name} started in the background.",
                    (("Inbox", self.inbox), ("Process group", group),
                     ("Follow", shlex.join(["tail", "-f", str(run / "out"), str(run / "err")])),
                     ("Wait", await_command),
                     ("Revoke", f"malak {selector} revoke {self.record.id}"),
                     ("Stop", stop_command)),
                    stream=readiness,
                )
        except BrokenPipeError:
            # The caller can go away once the worker is running.
            pass

    def close_gate(self):
        if self.gate is not None:
            process, gate, ready = self.gate
            gate.close()
            ready.close()
            if process.poll() is None:
                try:
                    process.terminate()
                except ProcessLookupError:
                    pass
            process.wait()
            self.gate = None

    def run_bound(self, command, cursor):
        run = self.gated_run
        agent, gate, ready = self.gate
        env = self.environment(cursor)
        self.started = False
        status = 3
        try:
            tool = adapters.hook_tool(Path(command[0]).name)
            if tool is None:
                out.details(f"No native Malak hooks configured for worker {self.args.name}.",
                            stream=sys.stderr)
            if shutil.which(command[0], path=env.get("PATH")) is None:
                raise OSError(f"worker executable {command[0]} is unavailable")
            command, env = worker_command(command, env)
            self.phase(self.evidence["phase"], agent_pid=agent.pid)
            with gate:
                gate.write(json.dumps([command, env, board.MALAK_EXE, tool]).encode())
            with ready:
                self.started = ready.read(1) == b"1"
            if self.started and self.detached_bundle:
                with (run / "err").open("ab") as errors:
                    self.announce_detached(run, errors)
            if self.started and self.detached_bundle and self.cancelled:
                os.killpg(os.getpgrp(), signal.SIGTERM)
            status = agent.wait()
            if status < 0:
                status = 128 - status
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            with (run / "err").open("a") as errors:
                out.error("start the worker command", error, "Check the executable and its launch settings.", stream=errors)
        finally:
            self.close_gate()
        if status and not self.started and tool is not None:
            sys.stderr.buffer.write((run / "err").read_bytes())
            out.details(f"Worker {self.args.name} failed malak hook --check --tool {tool}.",
                        (("Inbox", self.inbox), ("Worker stderr", run / "err"),
                         ("Next", "Check the worker's hook delivery before retrying.")), stream=sys.stderr)
        elif status:
            out.details(f"Worker {self.args.name} exited with status {status}.",
                        (("Inbox", self.inbox), ("Worker stderr", run / "err")), stream=sys.stderr)
        try:
            sent = self.report(run, status, env, initial=True)
            return status or sent, sent
        finally:
            if not self.started:
                self.revoke()

    def run(self, command, cursor, initial=False):
        if self.gate is not None:
            return self.run_bound(command, cursor)
        run = Path(tempfile.mkdtemp(prefix="result.", dir=self.private))
        env = self.environment(cursor)
        tool = adapters.hook_tool(Path(command[0]).name)
        self.started = False
        if not initial:
            self.phase(self.evidence["phase"], agent_pid=None, last_exit_status=None)
        status = 0
        try:
            with (run / "err").open("wb") as errors:
                if tool is not None:
                    checked = subprocess.run([board.MALAK_EXE, "hook", "--check", "--tool", tool],
                                             env=env, stdin=subprocess.DEVNULL, stdout=errors,
                                             stderr=subprocess.STDOUT)
                    if checked.returncode:
                        status = 3
                else:
                    out.details(f"No native Malak hooks configured for worker {self.args.name}.",
                                stream=sys.stderr)
                if not status:
                    with (run / "out").open("wb") as output:
                        try:
                            agent = start_worker(command, env=env, stdout=output, stderr=errors)
                        except (OSError, subprocess.SubprocessError, KeyError, ValueError) as error:
                            with (run / "err").open("a") as log:
                                out.error("start the worker command", error,
                                          "Check the executable and its launch settings.", stream=log)
                            status = 3
                        else:
                            self.started = True
                            with agent:
                                self.phase(self.evidence["phase"], agent_pid=agent.pid)
                                if initial and self.detached_bundle:
                                    self.announce_detached(run, errors)
                                if self.detached_bundle and self.cancelled:
                                    # A resume may have spawned during the original group signal.
                                    os.killpg(os.getpgrp(), signal.SIGTERM)
                                if initial and self.demo_report:
                                    Path(self.demo_report).with_suffix(".started").touch()
                                status = agent.wait()
                            if status < 0:
                                status = 128 - status
            if status:
                fields = [("Inbox", self.inbox), ("Handover", self.handover)]
                if (run / "err").stat().st_size or (run / "out").exists():
                    fields.append(("Worker stderr", run / "err"))
                if not (run / "out").exists():
                    sys.stderr.buffer.write((run / "err").read_bytes())
                    out.details(f"Worker {self.args.name} failed malak hook --check --tool {tool}.",
                                fields + [("Next", "Check the worker's hook delivery before retrying.")],
                                stream=sys.stderr)
                else:
                    out.details(f"Worker {self.args.name} exited with status {status}.", fields, stream=sys.stderr)
            sent = self.report(run, status, env, initial)
            return status or sent, sent
        finally:
            if not self.started:
                self.revoke()

    def resume(self):
        cursor = self.cursor()
        while True:
            cursor = board.wait_task(self.config, self.handover, self.record.id, cursor,
                                     lambda: self.cancelled)
            if cursor is None or self.cancelled:
                return 128 + signal.SIGTERM
            result, sent = self.run(["sh", "-c", self.args.resume], cursor)
            if not self.started or sent:
                return result


def main(mode, words):
    args = arguments(mode, words)
    if args is None:
        return 0
    with args.config.open("rb") as source:
        connection = QueueBoard(args.config, board.board_state(source).absolute())
    if selected := os.environ.get("MALAK_HANDOVER"):
        if mode == "queue" or args.prepared:
            if handover_identity(selected) != connection.identity:
                raise QueueError("a lead handover for the selected board is required")
        board.check_board(args.config, selected)
    worker = Worker(connection, args, mode)
    try:
        return worker.execute()
    finally:
        worker.close_gate()
        if worker.detached_bundle:
            shutil.rmtree(worker.hook_state, ignore_errors=True)
            shutil.rmtree(worker.detached_bundle, ignore_errors=True)


if __name__ == "__main__":
    os.umask(0o077)
    try:
        sys.exit(main(sys.argv[1], sys.argv[2:]))
    except (board.RecipeError, OSError, KeyError, ValueError) as error:
        out.error(f"run {sys.argv[1]}", error, "Check the board and the recipe inputs before retrying.")
        sys.exit(getattr(error, "code", 3))
