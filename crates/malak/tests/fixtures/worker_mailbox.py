#!/usr/bin/env python3
"""Replay the installed Codex reminder and watch a live lead note as a worker."""

import json
import os
from pathlib import Path
import subprocess
import sys
import time
import tomllib


def wait_for_output(path, phrase):
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline:
        if phrase in path.read_text():
            return
        time.sleep(0.05)
    raise AssertionError(path.read_text())


def hook_command(settings, event):
    return settings["hooks"][event][0]["hooks"][0]["command"]


def run_hook(command, event):
    payload = json.dumps({"session_id": "worker-mailbox", "hook_event_name": event})
    result = subprocess.check_output(command, shell=True, input=payload, text=True)
    return json.loads(result)["hookSpecificOutput"]["additionalContext"]


settings = json.loads(Path(".codex/hooks.json").read_text())
reminder = run_hook(hook_command(settings, "UserPromptSubmit"), "UserPromptSubmit")
assert 'You are alice on board "malak".' in reminder, reminder

assert sys.argv[1] == "-c"
settings = tomllib.loads(sys.argv[2])
task = run_hook(hook_command(settings, "SessionStart"), "SessionStart")
assert "Check the mailbox" in task, task

log = Path("worker-mailbox.log")
with log.open("w") as output, subprocess.Popen(
    ["malak", "--board", "malak", "mailbox", "--plain"], stdout=output,
) as mailbox:
    try:
        wait_for_output(log, "Mailbox activity")
        Path("worker-mailbox-ready").touch()
        wait_for_output(log, "guest sent a note to lead")
    finally:
        mailbox.terminate()
