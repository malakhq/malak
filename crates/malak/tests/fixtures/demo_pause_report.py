#!/usr/bin/env python3
"""Pause the first worker launcher while its report subprocess sends."""

import os
from pathlib import Path
import signal
import subprocess
import sys


if len(sys.argv) > 1:
    os.execv("/bin/cat", ["cat", *sys.argv[1:]])

body = sys.stdin.buffer.read()
if body == b"Fixture worker 1\n":
    ancestor = os.getppid()
    while ancestor > 1:
        row = subprocess.check_output(
            ["/bin/ps", "-ww", "-o", "ppid=,command=", "-p", str(ancestor)], text=True,
        ).strip()
        parent, command = row.split(maxsplit=1)
        if "/bundle/worker.py worker-start " in command:
            os.kill(ancestor, signal.SIGSTOP)
            Path("report-launcher-paused").touch()
            break
        ancestor = int(parent)
    else:
        sys.exit("could not find the worker launcher")
sys.stdout.buffer.write(body)
