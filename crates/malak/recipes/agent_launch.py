"""Launch a worker with native Malak hooks when the command supports them."""

import errno
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys

import adapters

MALAK_EXE = os.environ.get("MALAK_RECIPE_EXE", "malak")


def hook_definitions(tool, env=None):
    description = json.loads(subprocess.check_output(
        [MALAK_EXE, "hook", "--describe", "--json", "--tool", tool], text=True, env=env,
    ))
    command = "MALAK_RECIPE_NATIVE_HOOK=1 " + shlex.join([MALAK_EXE, "hook", "--tool", tool])
    # turn_end is the lead's: a worker's hook says nothing at Stop.
    return {event["input"]["hook_event_name"]: command
            for event in description["events"]
            if event["input"].get("hook_event_name") != "Stop"}


def hook_arguments(adapter, env):
    events = hook_definitions(adapter.delivery_tool or adapter.key, env)
    return adapter.hook_arguments(events, env)


def worker_command(command, env):
    adapter = adapters.native_adapter(Path(command[0]).name)
    if adapter is not None:
        env = env | {"MALAK_RECIPE_NATIVE_ONLY": "1"}
        env.pop("MALAK_RECIPE_NATIVE_HOOK", None)
        command = [command[0], *hook_arguments(adapter, env), *command[1:]]
    return command, env


def start_worker(command, *, env, stdout, stderr):
    command, env = worker_command(command, env)
    try:
        return subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr)
    except OSError as error:
        if error.errno != errno.ENOEXEC or (script := shutil.which(command[0])) is None:
            raise
        # Preserve the shell's treatment of executable scripts without a shebang.
        return subprocess.Popen(["/bin/sh", script, *command[1:]], env=env,
                                stdout=stdout, stderr=stderr)


def prepare_gate(*, stdout, stderr):
    """Start a process that waits for its handover before starting any worker code."""
    read_gate, write_gate = os.pipe()
    read_ready, write_ready = os.pipe()
    try:
        with os.fdopen(read_gate, "rb"), os.fdopen(write_ready, "wb"):
            process = subprocess.Popen(
                [sys.executable, "-c", LAUNCH_GATE, str(read_gate), str(write_ready)],
                stdout=stdout, stderr=stderr, pass_fds=(read_gate, write_ready),
            )
    except BaseException:
        os.close(write_gate)
        os.close(read_ready)
        raise
    return process, os.fdopen(write_gate, "wb"), os.fdopen(read_ready, "rb")


LAUNCH_GATE = """\
import errno, json, os, shutil, subprocess, sys
with os.fdopen(int(sys.argv[1]), "rb") as gate:
    payload = gate.read()
if not payload:
    sys.exit(3)
command, env, malak, tool = json.loads(payload)
if tool:
    probe = subprocess.run(
        [malak, "hook", "--check", "--tool", tool],
        env=env, stdin=subprocess.DEVNULL, stdout=sys.stderr, stderr=subprocess.STDOUT,
    )
    if probe.returncode:
        sys.exit(3)
with os.fdopen(int(sys.argv[2]), "wb") as ready:
    ready.write(b"1")
try:
    os.execvpe(command[0], command, env)
except OSError as error:
    path = shutil.which(command[0], path=env.get("PATH"))
    if error.errno != errno.ENOEXEC or path is None:
        raise
    os.execve("/bin/sh", ["/bin/sh", path, *command[1:]], env)
"""
