"""Start the demo lead in a private workspace and give it one goal."""

from contextlib import ExitStack
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib

from agent_launch import MALAK_EXE, codex_session_trust, hook_definitions
import board_data
from recipe_board import handover_identity
import recipe_output as out


SHUTDOWN_GRACE = 1
POLL_INTERVAL = 0.05


def agent_command(tool, sockets, sandbox, *, lead=False, trust=()):
    if tool == "claude":
        if sandbox == "off":
            return ["claude", "-p", "--no-session-persistence", "--dangerously-skip-permissions", "--"]
        return ["claude", "-p", "--no-session-persistence", "--tools", "Bash,Write,Read",
                "--allowedTools", "Bash(malak *)", "Write", "Read", "--"]
    command = ["codex", "-a", "never", "exec", "--ephemeral", "--skip-git-repo-check",
               "--color", "never", "-c", "allow_login_shell=false", *trust]
    if sandbox == "off":
        return [*command, "--sandbox", "danger-full-access", "--"]
    doors = ("l", "p") if lead else ("p",)
    allowed = ",".join(f'{json.dumps(str(sockets / door / "s"))}="allow"' for door in doors)
    return [*command, "-c", 'default_permissions="malak-demo"',
            "-c", 'permissions.malak-demo.filesystem={":root"="read",'
                  '":workspace_roots"="write",":tmpdir"="write"}',
            "-c", 'permissions.malak-demo.network={enabled=true,mode="limited"}',
            "-c", "features.network_proxy.enabled=true",
            "-c", f"permissions.malak-demo.network.unix_sockets={{{allowed}}}", "--"]


WORKER_PROMPT = """Handle the proven lead task delivered by the Malak hook. If you received no
task from the hook, return 'No Malak task received'. When you need to ask the
lead a question, use `malak recipe ask --blocks TEXT --option A --option B
--recommend A` with the question on stdin. Wait for the lead's answer with
`malak read --wait 30 "$MALAK_HOOK_INBOX"`. Check your hook context for the
answer too. Return only the short task result. The launcher posts it."""


def lead_prompt(goal, names):
    return f"""You are the lead agent on a local Malak board. Run this demonstration using
only the board and files in your current workspace. Stay in this session
until you have a report or a known launch or reporting failure for each worker.

The goal is the following JSON string: {json.dumps(goal, ensure_ascii=False)}

1. Divide the goal into exactly five small tasks. Write one standalone brief
   per task, named task-1.md through task-5.md. Make each task take one short
   response. Include any goal context the worker needs in its brief. If a task
   calls for a clarification, say explicitly to send it on the Malak questions
   channel and wait for a reply before giving the result.
2. Verify all five briefs are nonempty, then write `start` to a file named
   `launch` in this directory. The demo host starts the workers named
   {', '.join(names)}. It records launcher output in worker-N.log.
3. Poll with `malak read --wait 10 lead questions`. Answer a demo worker's
   question using `malak send --kind note --reply-to questions CREATION POSITION
   to/SENDER_ID ANSWER`. Use the creation and position in the question header
   and the sender ID from `malak read --json questions`. Count one result per worker
   from `lead`. If a worker exits with an error, consult the launcher output and
   record that failure. If a worker's report is missing, check worker-N.log
   for a launch or reporting error.
   Leave messages from other participants alone. Do not
   acknowledge through any message unrelated to this demo.
4. Once you have all five results or know why a worker failed, print a short
   summary of the goal and each worker's result to stdout.
"""


def progress(message):
    if sys.stdout.isatty():
        print(message, flush=True)


def start(config, tool, goal, sandbox, yes):
    definitions = None
    if tool == "codex":
        definitions = hook_definitions(tool)
        if out.interactive(sys.stderr):
            out.details("Codex hooks for this demo", tuple(definitions.items()), stream=sys.stderr)
        else:
            print("Codex hooks for this demo", file=sys.stderr)
            for event, command in definitions.items():
                print(f"  {event}: {out.display(command)}", file=sys.stderr)
        if not yes:
            print("trust these for this session? y/N ", end="", file=sys.stderr, flush=True)
            answer = sys.stdin.readline()
            if not sys.stdin.isatty():
                print(file=sys.stderr)
            if answer.strip().lower() not in {"y", "yes"}:
                print("Codex hook trust was declined.", file=sys.stderr)
                return 2
    if sandbox == "off":
        print("Warning: the demo lead and workers have full access as your user.", file=sys.stderr)

    config = Path(config).absolute()
    with config.open("rb") as source:
        board = tomllib.load(source)["board"]
    state = Path(board["state"])
    mailbox = ["malak"]
    if os.environ.get("MALAK_RECIPE_CONFIG"):
        mailbox.extend(["--config", str(config)])
    mailbox.append("mailbox")
    print(f"Open {out.display(shlex.join(mailbox))} in another terminal to watch the demo.",
          flush=True)
    workspace = Path(tempfile.mkdtemp(prefix="demo.", dir=state))
    interrupted = 0
    supervisor = 0

    def request_stop(signum, _frame):
        nonlocal interrupted
        interrupted = interrupted or signum
        if supervisor:
            try:
                os.kill(supervisor, signum)
            except ProcessLookupError:
                pass

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    ready, announce = os.pipe()
    try:
        supervisor = os.fork()
    except OSError:
        os.close(ready)
        os.close(announce)
        raise
    if supervisor:
        os.close(announce)
        with os.fdopen(ready, "rb") as startup:
            if interrupted:
                request_stop(interrupted, None)
            if startup.read(1) and not interrupted:
                out.details(f"Demo lead and five workers started with {tool}.", (
                    ("Lead log", workspace / "lead.log"),
                    ("Outcome", workspace / "outcome.txt"),
                ))
                return 0
        return exit_status(os.waitstatus_to_exitcode(os.waitpid(supervisor, 0)[1]))

    os.close(ready)
    try:
        os.setsid()
        return supervise(config, tool, goal, sandbox, definitions, board, workspace,
                         announce, lambda: interrupted)
    finally:
        os.close(announce)


def supervise(config, tool, goal, sandbox, definitions, board, workspace, announce, interruption):
    state = workspace.parent
    prefix = f"demo-{secrets.token_hex(3)}"
    names = [f"{prefix}-{number}" for number in range(1, 6)]
    env = {**os.environ, "MALAK_HANDOVER": str(state / "lead.handover")}
    env.pop("MALAK_PARTICIPANT_ID", None)
    lead = None
    workers = []
    enrolled = {}
    first_lead_channel = None
    delivered = None
    early = False
    lead_stopped = False
    stop_reason = None

    def stopping():
        return bool(interruption())

    with ExitStack() as stack:
        try:
            env["TMPDIR"] = stack.enter_context(
                tempfile.TemporaryDirectory(prefix="tmp.", dir=workspace))
            progress(f"Starting {tool} lead.")
            trust = codex_session_trust(workspace, definitions) if definitions else []
            if not stopping():
                sockets = Path(board["socket_folder"])
                log = workspace / "lead.log"
                with log.open("xb") as output:
                    lead = subprocess.Popen(
                        [*agent_command(tool, sockets, sandbox, lead=True), lead_prompt(goal, names)],
                        cwd=workspace, env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=output,
                        start_new_session=True,
                    )
                briefs = wait_for_briefs(lead, workspace, len(names), stopping)
                early = not briefs and not stopping()
                if briefs and not stopping() and lead.poll() is None:
                    first_lead_channel = lead_channel(config, env)
                    private = Path(stack.enter_context(
                        tempfile.TemporaryDirectory(prefix="demo-briefs.", dir=state)))
                    command = agent_command(tool, sockets, sandbox, trust=trust)
                    start_workers(config, workspace, private, names, briefs, command,
                                  env, workers, enrolled,
                                  lambda: stopping() or lead.poll() is not None)
                    if len(workers) == len(names) and wait_for_worker_starts(
                        workers, lambda: stopping() or lead.poll() is not None,
                    ) and not stopping():
                        detach_terminal(workspace, announce)
                    wait_for_processes([worker for worker, _ in workers], stopping, lead=lead)
                    if not stopping() and lead.poll() is None:
                        delivered = accepted_reports(config, env, first_lead_channel, workers)
                        if len(delivered) < len(names) and lead.poll() is None:
                            stop_reason = "Demo stopped because worker reports were missing."
                            lead_stopped = True
                            stop_processes([lead])
                wait_for_processes([lead], stopping)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            out.error("run the demo", error, "Check the agent CLI and board state directory.")
            stop_reason = "Demo stopped after a launcher error."
        finally:
            children = [worker for worker, _ in workers]
            cancelled_workers = {worker for worker in children if worker.poll() is None}
            if lead is not None:
                lead_stopped = lead_stopped or lead.poll() is None
                children.append(lead)
            try:
                stop_processes(children)
            except OSError as error:
                out.error("stop the demo processes", error, "Check the lead and worker logs.")
                stop_reason = "Demo stopped after a cleanup error."
            try:
                if delivered is None:
                    delivered = accepted_reports(config, env, first_lead_channel, workers)
            finally:
                cleanup = retire_workers(config, enrolled)
        if not lead_stopped and stop_reason is None and any(
            worker in cancelled_workers and expected not in delivered for worker, expected in workers
        ):
            stop_reason = "Demo stopped waiting workers after the lead exited."
        succeeded = sum(status == 0 for status in delivered.values())
        lead_status = (
            exit_status(lead.returncode)
            if lead is not None and lead.returncode is not None else None
        )
    return write_outcome(workspace, lead_status, succeeded, len(delivered), len(names), cleanup,
                         early=early, interrupted=interruption(), stop_reason=stop_reason,
                         lead_stopped=lead_stopped)


def detach_terminal(workspace, announce):
    # The supervisor must close all of the caller's streams, including pipes,
    # before it tells the foreground process that the demo can finish alone.
    with (workspace / "launcher.log").open("xb") as log, open(os.devnull, "rb") as source:
        sys.stdout.flush()
        sys.stderr.flush()
        os.dup2(log.fileno(), sys.stdout.fileno())
        os.dup2(log.fileno(), sys.stderr.fileno())
        os.dup2(source.fileno(), 0)
    os.write(announce, b"1")


def read_brief(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise OSError(f"{path.name} must be a regular file")
        brief = source.read(32769)
        if not brief or len(brief) > 32768:
            raise OSError(f"{path.name} is empty or exceeds 32 KiB")
        return brief


def wait_for_briefs(lead, workspace, total, stopping):
    # The lead can publish briefs. Command arguments, worker identities and
    # executable selection remain under the host's control.
    progress(f"Waiting for {total} briefs.")
    launch = workspace / "launch"
    paths = [workspace / f"task-{number}.md" for number in range(1, total + 1)]
    unseen = paths.copy() if sys.stdout.isatty() else []
    while not stopping():
        ready = launch.exists()
        for path in unseen[:]:
            try:
                info = path.lstat()
            except OSError:
                continue
            if stat.S_ISREG(info.st_mode) and info.st_size:
                progress(f"Found {path.name}.")
                unseen.remove(path)
        if ready:
            try:
                return [read_brief(path) for path in paths]
            except OSError as error:
                out.error("launch the demo workers", error, "Correct the briefs and try again.")
                # Let the lead notice and fix the bad request before retrying.
                launch.unlink(missing_ok=True)
        if lead.poll() is not None:
            break
        time.sleep(POLL_INTERVAL)
    return []


def wait_for_worker_starts(workers, stopping):
    while True:
        pending = [worker for worker, expected in workers
                   if not expected.with_suffix(".started").is_file()]
        if not pending:
            return True
        if stopping() or any(worker.poll() is not None for worker in pending):
            return False
        time.sleep(POLL_INTERVAL)


def start_workers(config, workspace, private, names, briefs, command, env, workers, enrolled,
                  stopping):
    host_scratch = private / "tmp"
    host_scratch.mkdir(mode=0o700)
    # The launcher extracts its support files here. Give only the agent
    # process the scratch directory that is writable inside its sandbox.
    worker_env = {**env, "TMPDIR": str(host_scratch), "MALAK_DEMO_AGENT_TMPDIR": env["TMPDIR"]}
    for number, (name, task) in enumerate(zip(names, briefs), 1):
        if stopping():
            return
        brief = private / f"task-{number}.md"
        expected = private / f"report-{number}.expected"
        log = workspace / f"worker-{number}.log"
        try:
            with log.open("x") as output:
                try:
                    brief.write_bytes(task)
                    progress(f"Starting worker {name}.")
                    handover = enrol_demo_worker(config, workspace.parent, name, enrolled)
                    if stopping():
                        return
                    worker = subprocess.Popen(
                        [MALAK_EXE, "--config", config, "recipe", "worker-start", name, "--unbound",
                         "--no-watcher", "--brief", str(brief), "--", *command, WORKER_PROMPT],
                        cwd=workspace, env={**worker_env, "MALAK_DEMO_REPORT_RECORD": str(expected),
                                            "MALAK_DEMO_HANDOVER": str(handover)},
                        stdin=subprocess.DEVNULL, stdout=output, stderr=output, start_new_session=True,
                    )
                    workers.append((worker, expected))
                except (board_data.RecipeError, OSError, ValueError, KeyError) as error:
                    out.details("Demo worker could not start.", (("Reason", error),), stream=output)
        except OSError as error:
            out.error(f"write the log for demo worker {number}", error, f"Check access to {log}.")


def enrol_demo_worker(config, state, name, enrolled):
    # The supervisor can finish enrolling even if cancellation arrives before
    # the worker launcher starts. Keep its exact ID before handing off the key.
    private = Path(tempfile.mkdtemp(prefix="worker.", dir=state))
    handover = private / "handover"
    try:
        participant = board_data.enrol_lead_sender(config, name, handover, "--unbound")
    except (board_data.RecipeError, OSError, ValueError, KeyError) as error:
        # The board can save the identity before a handover write or an
        # enrolment reply fails. Use evidence specific to this owner call.
        saved = re.search(r"^  Reason +participant ([a-z0-9]{8,32}) was enrolled but its handover failed:",
                          str(error), re.MULTILINE)
        if saved:
            enrolled[saved[1]] = handover
        elif handover.is_file():
            _, participant, _ = handover_identity(handover)
            enrolled[participant] = handover
        else:
            private.rmdir()
        raise
    enrolled[participant] = handover
    return handover


def retire_workers(config, enrolled):
    revoked = 0
    problems = []
    for participant, handover in enrolled.items():
        try:
            board_data.malak("--config", config, "revoke", participant)
            revoked += 1
        except (board_data.RecipeError, OSError) as error:
            problems.append(("Cleanup", f"Could not revoke worker {participant}. {error}"))
        try:
            handover.unlink(missing_ok=True)
        except OSError as error:
            problems.append(("Cleanup", f"Could not remove worker {participant}'s handover. {error}"))
    return [("Revoked", f"{revoked} of {len(enrolled)} workers"), *problems]


def board_json(config, env, *command):
    result = subprocess.run(
        [MALAK_EXE, "--config", config, *command, "--json"],
        env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=5,
    )
    if result.returncode:
        raise OSError(result.stderr.strip() or f"malak exited with status {result.returncode}")
    return json.loads(result.stdout)


def lead_channel(config, env):
    channels = board_json(config, env, "board", "status")["channels"]
    channel = next((record for record in channels if record["name"] == "lead"), None)
    if channel is None:
        raise OSError("the lead channel is unavailable")
    return channel


def accepted_reports(config, env, first_channel, workers):
    delivered = {}
    expected = {}
    for _, path in workers:
        try:
            sender, message_id, status = path.read_text().splitlines()
            status = int(status)
        except (OSError, ValueError):
            # A launcher may stop before it prepares a report.
            continue
        expected[sender, message_id] = (path, status)
    if not expected:
        return delivered

    # GET includes reports the lead already acknowledged. Inspect only the
    # positions assigned during this run, and match the exact sender and ID.
    try:
        channel = lead_channel(config, env)
        floor = channel["floor"]
        if channel["creation"] == first_channel["creation"]:
            floor = max(floor, first_channel["head"])
        for position in range(channel["head"] - 1, floor - 1, -1):
            record = board_json(config, env, "get", "lead", str(position))
            if (record.get("kind") != "report" or record.get("grade") != "proven"
                    or record.get("creation") != channel["creation"]):
                continue
            found = expected.pop((record.get("sender"), record.get("id")), None)
            if found is not None:
                path, status = found
                delivered[path] = status
                if not expected:
                    break
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        out.error("confirm demo reports on the board", error,
                  "Check the lead log and the board's retained reports.")
    return delivered


def wait_for_processes(processes, stopping, *, lead=None):
    deadline = None
    while not stopping() and any(process.poll() is None for process in processes):
        if lead is not None and lead.poll() is not None:
            # A lead can see the last report just before its launcher exits.
            # Give those launchers a bounded drain before stopping the rest.
            if deadline is None:
                deadline = time.monotonic() + SHUTDOWN_GRACE
            if time.monotonic() >= deadline:
                return
        time.sleep(POLL_INTERVAL)


def signal_group(process, signum):
    try:
        os.killpg(process.pid, signum)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        # Some Unix hosts return EPERM when a group has just stopped.
        # An active launcher still has a directly signalable child PID.
        if process.poll() is not None:
            return False
        if signum:
            process.send_signal(signum)
        return True


def stop_processes(processes):
    active = [process for process in processes if process.poll() is None]
    if not active:
        return
    for process in active:
        signal_group(process, signal.SIGTERM)
    groups = active
    deadline = time.monotonic() + SHUTDOWN_GRACE
    while groups and time.monotonic() < deadline:
        for process in groups:
            process.poll()
        groups = [process for process in groups if signal_group(process, 0)]
        if groups:
            time.sleep(POLL_INTERVAL)
    for process in groups:
        signal_group(process, signal.SIGKILL)
    for process in active:
        process.wait()


def exit_status(status):
    return status if status >= 0 else 128 - status


def write_outcome(workspace, lead_status, succeeded, confirmed, total, cleanup, *,
                  early, interrupted, stop_reason, lead_stopped):
    status = lead_status or (3 if succeeded != total else 0)
    if interrupted:
        status = 128 + interrupted
        title = "Demo interrupted."
    elif stop_reason is not None:
        if lead_stopped or not lead_status:
            status = 3
        title = stop_reason
    elif early:
        title = "Demo lead exited during startup."
    elif lead_status:
        title = "Demo lead failed."
    elif status:
        title = "Demo workers failed."
    else:
        title = "Demo finished."
    log = workspace / "lead.log"
    fields = [("Workers", f"{succeeded} of {total} workers reported"),
              ("Reports", f"{confirmed} of {total} delivered"),
              *cleanup,
              ("Lead exit", lead_status if lead_status is not None else "Unavailable"),
              ("Lead log", log)]
    if early or lead_status:
        fields.append(("Next", "Check the agent sign-in and lead log before retrying."))
    elif status:
        fields.append(("Next", "Check the worker logs beside the lead log."))
    def show_outcome(stream):
        out.details(title, [*fields, ("Demo exit", status)], stream=stream)

    stream = sys.stderr if early else sys.stdout
    try:
        with tempfile.NamedTemporaryFile(mode="w", prefix=".outcome.", dir=workspace,
                                         delete=False) as output:
            pending = Path(output.name)
            try:
                show_outcome(output)
                output.close()
                pending.replace(workspace / "outcome.txt")
            finally:
                pending.unlink(missing_ok=True)
    except OSError as error:
        status = status or 3
        out.error("publish the demo outcome", error, f"Check {workspace / 'outcome.txt'}.")
        stream = sys.stderr
    show_outcome(stream)
    return status


if __name__ == "__main__":
    try:
        parser = out.ArgumentParser(prog="malak recipe demo")
        parser.add_argument("tool", choices=("claude", "codex"))
        parser.add_argument("goal")
        parser.add_argument("--sandbox", choices=("on", "off"), default="on")
        parser.add_argument("--yes", action="store_true")
        options = parser.parse_args(sys.argv[2:])
        sys.exit(start(sys.argv[1], **vars(options)))
    except (OSError, ValueError, KeyError) as error:
        out.error("start the demo lead", error, "Check the agent CLI and board state directory.")
        sys.exit(3)
