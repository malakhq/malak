"""Find test boards and clean up the processes owned by one fixture."""

from contextlib import contextmanager
from dataclasses import dataclass
import os
from pathlib import Path
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time


START = re.compile(r"^(?:.*?/)?malak (?:--config[ =](?P<first>.+?) )?board start(?: |$)")
CONFIG = re.compile(r"(?:^| )--config[ =](.+?)(?= --[\w-]|$)")


@dataclass(frozen=True)
class BoardProcess:
    pid: int
    group: int
    config: Path


def working_directory(pid):
    if sys.platform == "darwin":
        result = subprocess.run(
            ["/usr/sbin/lsof", "-a", "-p", pid, "-d", "cwd", "-Fn"], timeout=5,
            capture_output=True, encoding="utf-8", errors="surrogateescape",
        )
        return next((Path(line[1:]) for line in result.stdout.splitlines()
                     if line.startswith("n/")), None)
    try:
        return Path(os.readlink(f"/proc/{pid}/cwd"))
    except OSError:
        return None


def board_processes(*roots):
    """Snapshot this account's live board starts with a config under the given roots."""
    name = "ucomm" if sys.platform == "darwin" else "comm"
    rows = subprocess.check_output(
        ["ps", "-ww", "-axo", f"pid=,pgid=,uid=,stat=,{name}=,args="],
        encoding="utf-8", errors="surrogateescape",
    )
    processes = set()
    for row in rows.splitlines():
        fields = row.split(None, 5)
        if len(fields) != 6:
            continue
        pid, group, uid, state, executable, args = fields
        if int(uid) != os.geteuid() or state.startswith("Z") or executable != "malak":
            continue
        start = START.match(args)
        if start is None:
            continue
        path = start["first"]
        if path is None:
            option = CONFIG.search(args, start.end() - 1)
            if option is None:
                continue
            path = option[1]
        config = Path(path)
        if not config.is_absolute():
            cwd = working_directory(pid)
            if cwd is None:
                continue
            config = cwd / config
        config = config.resolve()
        if any(config.is_relative_to(root) for root in roots):
            processes.add(BoardProcess(int(pid), int(group), config))
    return processes


def stop_boards(root):
    deadline = time.monotonic() + 5
    while boards := board_processes(root):
        for board in boards:
            try:
                if os.getpgid(board.pid) != board.group:
                    continue
                # A dedicated board group also contains any children it forked.
                # A launcher in the test runner's group must be signalled alone.
                if board.group == board.pid:
                    os.killpg(board.group, signal.SIGKILL)
                else:
                    os.kill(board.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if time.monotonic() >= deadline:
            raise RuntimeError(f"test boards did not exit under {root}: {boards}")
        time.sleep(0.025)


@contextmanager
def start_test_board(command, *, env, timeout=15, cwd=None):
    """Start a foreground board and reap its process group on exit."""
    with tempfile.TemporaryFile() as errors, subprocess.Popen(
        command, env=env, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=errors, start_new_session=True,
    ) as board:
        try:
            ready, _, _ = select.select([board.stdout], [], [], timeout)
            if not ready or board.stdout.readline() != b"ready\n":
                errors.seek(0)
                raise RuntimeError(errors.read().decode(errors="replace") or "board missed readiness")
            yield
        finally:
            try:
                os.killpg(board.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            board.wait()


@contextmanager
def watch_board_home(root):
    """Keep the watcher alive until the fixture closes its pipe or exits."""
    with subprocess.Popen(
        [sys.executable, "-B", __file__, root], stdin=subprocess.PIPE,
        start_new_session=True,
    ) as watcher:
        try:
            yield
        finally:
            watcher.stdin.close()
            if watcher.wait():
                raise RuntimeError(f"could not clean up test boards under {root}")


def main(root):
    # This process owns exactly the home passed to it. EOF works on both normal
    # teardown and a test runner killed before it can run any of its cleanup.
    sys.stdin.buffer.read()
    stop_boards(root)
    try:
        shutil.rmtree(root)
    except FileNotFoundError:
        if root.exists():
            raise
    if root.parent.name == ".m":
        try:
            root.parent.rmdir()
        except OSError:
            # Other fixtures can be using the shared short directory.
            pass


if __name__ == "__main__":
    main(Path(sys.argv[1]).resolve())
