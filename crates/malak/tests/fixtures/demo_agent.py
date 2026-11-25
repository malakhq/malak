#!/usr/bin/env python3
"""Stand in for a native agent while using Malak's real board and launcher."""

import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
import tomllib

WATCHDOG = int(os.environ["MALAK_TEST_WATCHDOG"])


def expired_probe(_signum, _frame):
    Path("codex-probe-expired").touch()
    sys.exit(93)


def app_server():
    stalls = os.environ.get("MALAK_TEST_CODEX_PROBE_STALLS") == "1"
    if stalls:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    hooks = []
    for arg in sys.argv[2:]:
        if arg.startswith("hooks."):
            for event, groups in tomllib.loads(arg)["hooks"].items():
                hooks.append({
                    "key": event, "eventName": event[0].lower() + event[1:],
                    "source": "sessionFlags", "handlerType": "command", "enabled": True,
                    "command": groups[0]["hooks"][0]["command"], "currentHash": "fixture",
                })
    for line in sys.stdin:
        request = json.loads(line)
        match request.get("method"):
            case "initialize":
                print(json.dumps({"id": 0, "result": {}}), flush=True)
            case "hooks/list":
                if stalls:
                    Path("codex-probe.pid").write_text(str(os.getpid()))
                print(json.dumps({"id": 1, "result": {"data": [{"hooks": hooks}]}}), flush=True)
                if stalls:
                    signal.signal(signal.SIGALRM, expired_probe)
                    signal.alarm(WATCHDOG)
                    wait_to_be_stopped()


def malak(*args):
    return subprocess.check_output(["malak", *args], text=True)


def notify_test(event):
    address = os.environ.get("MALAK_TEST_DEMO_EVENTS")
    if address:
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as events:
            events.sendto(event.encode(), address)


def report_outcome(mode):
    channels = ["lead", "questions"] if mode == "dead-lead" else ["lead"]
    deadline = time.monotonic() + WATCHDOG
    while time.monotonic() < deadline:
        shown = malak("read", "--json", *channels)
        bodies = [row["text"] for line in shown.splitlines()
                  if "text" in (row := json.loads(line))]
        reports = sum("Fixture worker" in body for body in bodies)
        asked = any("Waiting worker clarification?" in body for body in bodies)
        if mode == "dead-lead" and reports == 4 and asked:
            return 29
        if reports == 5:
            if mode == "pause-report":
                malak("read", "--ack", "lead")
            print("Fixture demo summary")
            return int(os.environ["MALAK_TEST_LEAD_EXIT"]) if mode == "worker-status" else 0
        time.sleep(0.05)
    return 92


def wait_to_be_stopped():
    while True:
        signal.pause()


def brief_bodies(mode):
    if mode == "worker-status":
        yield from os.environ["MALAK_TEST_WORKER_STATUSES"].split()
    elif mode == "progress":
        address = os.environ["MALAK_TEST_DEMO_EVENTS"]
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as gate:
            gate.bind(str(Path(address).with_name("demo-gate")))
            gate.settimeout(WATCHDOG)
            gate.sendto(b"lead", address)
            for body in range(1, 6):
                gate.recv(1)
                yield body
            gate.recv(1)  # Publish launch only after the test has seen every brief.
    else:
        yield from range(1, 6)


def main():
    if sys.argv[1] == "app-server":
        app_server()
        return 0
    mode = os.environ["MALAK_TEST_DEMO_MODE"]
    if "MALAK_PARTICIPANT_ID" in os.environ:
        assert Path(os.environ["TMPDIR"]).resolve().is_relative_to(Path.cwd().resolve())
        assert "MALAK_DEMO_REPORT_RECORD" not in os.environ
        assert "MALAK_DEMO_AGENT_TMPDIR" not in os.environ
        assert "MALAK_DEMO_HANDOVER" not in os.environ
        inbox = os.environ["MALAK_HOOK_INBOX"]
        task = json.loads(malak("get", "--json", inbox, "1"))
        number = int(task["text"].splitlines()[-1])
        if mode == "worker-status":
            print(f"Fixture worker exit {number}")
            return number
        if mode == "dead-lead" and number == 1:
            malak("read", "--ack", inbox)
            Path("worker-1.running").write_text(str(os.getpid()))
            subprocess.run(
                ["malak", "recipe", "ask", "--blocks", "the worker task",
                 "--option", "A", "--option", "B", "--recommend", "A"],
                input="Waiting worker clarification?\n", text=True, check=True,
                stdout=subprocess.DEVNULL,
            )
            while True:
                malak("read", "--wait", "30", inbox)
        if mode == "signal":
            if number == 1:
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
            Path(f"worker-{number}.running").write_text(str(os.getpid()))
            notify_test(f"worker-{number}")
            wait_to_be_stopped()
        if mode == "refused" and number == 1:
            sys.stdout.write("x" * 262145)
        else:
            print(f"Fixture worker {number}")
        return 0

    for number, body in enumerate(brief_bodies(mode), 1):
        Path(f"task-{number}.md").write_text(f"{body}\n")
    if mode == "unopened-log":
        Path("worker-1.log").mkdir()
    Path("lead.running").write_text(str(os.getpid()))
    if mode == "start-signal":
        Path("host.running").write_text(str(os.getppid()))
        notify_test("lead")
        wait_to_be_stopped()
    Path("launch").write_text("start")
    if mode == "signal":
        notify_test("lead")
    if mode == "background":
        notify_test("lead")
        deadline = time.monotonic() + WATCHDOG
        while not Path("finish-demo").exists():
            if time.monotonic() >= deadline:
                return 92
            time.sleep(0.05)
    if mode in {"receipt-without-send", "refused", "unopened-log", "signal"}:
        # The lead remains live until the launcher handles the missing
        # reports or the test sends SIGTERM.
        wait_to_be_stopped()
    return report_outcome(mode)


sys.exit(main())
