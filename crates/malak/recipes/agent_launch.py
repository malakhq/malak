"""Launch a worker with native Malak hooks when the command supports them."""

import errno
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import subprocess
import sys
import time

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


def hook_arguments(tool, env):
    events = hook_definitions(tool, env)
    if tool == "codex":
        return codex_hook_arguments(events)
    settings = {"hooks": {
        event: [{"hooks": [{"type": "command", "command": command}]}]
        for event, command in events.items()
    }}
    return ["--settings", json.dumps(settings)]


def codex_hook_arguments(events):
    flags = []
    for event, command in events.items():
        settings = "timeout=3" if event == "SessionEnd" else "additionalContextLimit=0"
        flags.extend((
            "-c", f"hooks.{event}=[{{hooks=[{{type=\"command\","
                  f"command={json.dumps(command)},{settings}}}]}}]",
        ))
    return flags


def send_request(server, request):
    server.stdin.write((json.dumps(request) + "\n").encode())


def receive_response(server, identifier, pending, stopping):
    deadline = time.monotonic() + 20
    while not stopping() and (left := deadline - time.monotonic()) > 0:
        end = pending.find(b"\n")
        if end < 0:
            if select.select([server.stdout], [], [], min(left, 0.05))[0]:
                chunk = os.read(server.stdout.fileno(), 65536)
                if not chunk:
                    break
                pending.extend(chunk)
            continue
        reply = json.loads(pending[:end])
        del pending[:end + 1]
        if reply.get("id") == identifier and "result" in reply:
            return reply["result"]
        if reply.get("id") == identifier:
            message = reply.get("error", {}).get("message", "unknown error")
            raise OSError(f"Codex could not list the session hooks: {message}")
    raise OSError("Codex did not return the session hook definitions")


def codex_session_trust(workspace, definitions, *, env, stopping):
    # Inspect the session definitions through Codex. The resulting overrides
    # approve only these hashes for this process and leave user hooks alone.
    with subprocess.Popen(
        ["codex", "app-server", "--stdio", *codex_hook_arguments(definitions)],
        cwd=workspace, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0,
    ) as server:
        pending = bytearray()
        try:
            send_request(server, {"id": 0, "method": "initialize", "params": {
                "clientInfo": {"name": "malak", "version": "0.1.2"},
                "capabilities": {"experimentalApi": True},
            }})
            receive_response(server, 0, pending, stopping)
            send_request(server, {"method": "initialized"})
            send_request(server, {"id": 1, "method": "hooks/list",
                                  "params": {"cwds": [str(workspace)]}})
            result = receive_response(server, 1, pending, stopping)
        finally:
            if server.poll() is None:
                server.terminate()
                try:
                    server.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    server.kill()

    expected = {event[0].lower() + event[1:]: command for event, command in definitions.items()}
    settings = []
    hooks = result["data"][0]["hooks"]
    for hook in hooks:
        event = hook["eventName"]
        if (hook["source"] == "sessionFlags" and hook["handlerType"] == "command"
                and hook["enabled"] and expected.get(event) == hook["command"]):
            key, value = json.dumps(hook["key"]), json.dumps(hook["currentHash"])
            settings.append(f"{key}={{trusted_hash={value}}}")
            del expected[event]
    if expected:
        raise OSError("Codex did not expose all four enabled Malak hooks for session approval")
    return ["-c", f"hooks.state={{{','.join(settings)}}}"]


def worker_command(command, env):
    tool = Path(command[0]).name
    if tool in {"claude", "codex"}:
        env = env | {"MALAK_RECIPE_NATIVE_ONLY": "1"}
        env.pop("MALAK_RECIPE_NATIVE_HOOK", None)
        command = [command[0], *hook_arguments(tool, env), *command[1:]]
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
if tool in {"claude", "codex"}:
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
