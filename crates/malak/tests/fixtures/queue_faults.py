#!/usr/bin/env python3
"""Interrupt real recipe CLI calls at selected test boundaries."""

import json
import os
from pathlib import Path
import subprocess
import sys
import time

real = os.environ["TEST_MALAK"]
mode = os.environ["TEST_QUEUE_FAULT"]
arguments = sys.argv[1:]
offset = 2 if arguments[:1] == ["--config"] else 0
config = arguments[:offset]
command = arguments[offset:]
if mode in {"hook", "hold-hook"} and command[:2] == ["hook", "--check"]:
    if mode == "hold-hook":
        Path("hook-check-waiting").touch()
        while not Path("finish-hook-check").exists():
            time.sleep(0.02)
    print("fixture hook check refused", file=sys.stderr)
    sys.exit(2)
if command[:1] != ["send"] or "--kind" not in command:
    os.execv(real, [real, *arguments])
kind = command[command.index("--kind") + 1]
channel = command[-2]
brief = kind == "task" and channel.startswith("to/")
forward = kind == "note" and channel.startswith("to/")
outcome = kind == "note" and channel == "lead"
if (mode == "fail-brief" and brief) or (mode == "fail-forward" and forward):
    print(f"fixture {mode.removeprefix('fail-')} refused", file=sys.stderr)
    sys.exit(2)
lose = {"lose-brief": brief, "lose-forward": forward, "lose-outcome": outcome}.get(mode)
marker = Path(f"fault-{mode}")
if (not lose and not (forward and mode in {"hold", "trim"})) or marker.exists():
    os.execv(real, [real, *arguments])

result = subprocess.run([real, *arguments], stdout=subprocess.PIPE)
if result.returncode:
    sys.exit(result.returncode)
marker.touch()
if lose:
    print("fixture lost reply after commit", file=sys.stderr)
    sys.exit(3)
if mode == "hold":
    while not Path("finish-forward").exists():
        time.sleep(0.02)
if mode == "trim":
    receipt = json.loads(result.stdout)
    subprocess.run([real, *config, "trim", channel, str(receipt["position"])], check=True,
                   stdout=subprocess.DEVNULL)
sys.stdout.buffer.write(result.stdout)
