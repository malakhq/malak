"""Exercise the workspace check's temp snapshot with a disposable checkout."""

import os
from pathlib import Path
import pwd
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "crates/malak/tests/fixtures"
sys.path.insert(0, str(FIXTURES))
from board_processes import board_processes, start_test_board, stop_boards, watch_board_home


CHECK_WITH_TEST_ACCOUNT = """
from pathlib import Path
import pwd
import runpy
import sys
from types import SimpleNamespace
from unittest.mock import patch

check, home = sys.argv[1:]
sys.path.insert(0, str(Path(check).parent))
with patch.object(pwd, "getpwuid", return_value=SimpleNamespace(pw_dir=home)):
    runpy.run_path(check, run_name="__main__")
"""


class TempCheckTests(unittest.TestCase):
    def setUp(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        source = Path(__file__).resolve().parents[1]
        scripts = root / "scripts"
        for tests in (scripts / "tests", root / "crates/malak/recipes/tests"):
            tests.mkdir(parents=True)
            (tests / "test_smoke.py").write_text(
                "import unittest\nclass Smoke(unittest.TestCase):\n    def test_pass(self): pass\n"
            )
        for name in ("check", "process_cleanup.py"):
            shutil.copy2(source / name, scripts / name)
        fixtures = root / "crates/malak/tests/fixtures"
        fixtures.mkdir(parents=True)
        shutil.copy2(FIXTURES / "board_processes.py", fixtures)
        self.check = scripts / "check"
        self.temp = root / "temp"
        self.temp.mkdir()
        self.home = root / "home"
        self.home.mkdir()
        self.cargo = root / "cargo"
        self.env = {**os.environ, "TMPDIR": str(self.temp), "PATH": f"{root}{os.pathsep}{os.environ['PATH']}"}
        # An outer check run must not scope the check under test.
        self.env.pop("MALAK_CHECK_RUN", None)

    def run_check(self, fake_cargo):
        self.cargo.write_text(f"#!/bin/sh\nset -eu\n{fake_cargo}\n")
        self.cargo.chmod(0o700)
        return subprocess.run(
            [sys.executable, "-B", "-c", CHECK_WITH_TEST_ACCOUNT, self.check, self.home],
            env=self.env, text=True, capture_output=True, timeout=30,
        )

    def test_new_leftovers_fail_a_passing_check(self):
        result = self.run_check('touch "$TMPDIR/malak-leftover"')

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("0 before, 1 after, 1 new", result.stderr)
        self.assertIn(str(self.temp / "malak-leftover"), result.stderr)

    def test_preexisting_entries_are_preserved(self):
        existing = self.temp / "malak-already-there"
        existing.write_text("keep")

        result = self.run_check("exit 0")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 before, 1 after, 0 new", result.stderr)
        self.assertEqual(existing.read_text(), "keep")

    def test_failed_checks_still_report_leftovers(self):
        result = self.run_check('touch "$TMPDIR/malak-leftover"\nexit 12')

        self.assertEqual(result.returncode, 12, result.stderr)
        self.assertIn(str(self.temp / "malak-leftover"), result.stderr)

    def test_new_short_homes_fail_a_passing_check(self):
        added = self.home / ".m/leaked"
        result = self.run_check(f"mkdir -p {shlex.quote(str(added))}")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("Short test home check failed", result.stderr)
        self.assertIn(str(added), result.stderr)

    def test_homes_of_another_run_do_not_fail_a_scoped_check(self):
        self.env["MALAK_CHECK_RUN"] = "aaaaaa"
        other = self.home / ".m/bbbbbb/leaked"

        result = self.run_check(f"mkdir -p {shlex.quote(str(other))}")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Short test home check passed (0 before, 0 after, 0 new)", result.stderr)

    def test_a_new_home_of_this_run_fails_a_scoped_check(self):
        self.env["MALAK_CHECK_RUN"] = "aaaaaa"
        added = self.home / ".m/aaaaaa/leaked"

        result = self.run_check(f"mkdir -p {shlex.quote(str(added))}")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("Short test home check failed", result.stderr)
        self.assertIn(str(added), result.stderr)

    def test_a_live_board_in_tmpdir_fails_a_passing_check(self):
        with BoardFixture(short_home=False) as board:
            self.env["TMPDIR"] = str(board.root)
            self.env.update(HOME=board.env["HOME"], XDG_CONFIG_HOME=board.env["XDG_CONFIG_HOME"])
            self.env.pop("MALAK_HANDOVER", None)
            board.detach()
            # Use the existing board as a control, then start it again during
            # the check. It will have a different process ID.
            baseline = self.run_check("exit 0")
            self.assertEqual(baseline.returncode, 0, baseline.stderr)
            self.assertIn("Test board process check passed (1 before, 1 after, 0 new)",
                          baseline.stderr)
            start = shlex.join(map(str, board.command + ["--detach"]))
            stop = shlex.join(map(str, board.command[:-1] + ["stop"]))
            check = self.run_check(
                f'if [ "$1" = fmt ]; then {stop}; {start}; fi'
            )

        self.assertEqual(check.returncode, 1, check.stderr)
        self.assertIn("Test board process check failed", check.stderr)
        self.assertIn(str(board.config), check.stderr)


class BoardFixture:
    def __init__(self, *, short_home=True):
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        self.malak = target.resolve() / "debug/malak"
        if not self.malak.is_file():
            raise unittest.SkipTest("build malak to exercise live board cleanup")
        home = Path(pwd.getpwuid(os.geteuid()).pw_dir).resolve()
        parent = home / ".m" if short_home else home
        if run := os.environ.get("MALAK_CHECK_RUN"):
            parent = parent / run if short_home else parent
        parent.mkdir(mode=0o711, parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="mb " if not short_home else "mb-", dir=parent)
        self.root = Path(self.temp.name)
        self.config = self.root / "b.toml"
        self.config.write_text(
            f'[board]\nstate = "{self.root}/st"\nsocket_folder = "{self.root}/d"\n'
        )
        self.env = {**os.environ, "HOME": str(self.root), "XDG_CONFIG_HOME": str(self.root / "xdg")}
        self.env.pop("MALAK_HANDOVER", None)
        self.command = [self.malak, "--config", self.config, "board", "start"]

    def __enter__(self):
        self.watcher = watch_board_home(self.root)
        self.watcher.__enter__()
        return self

    def __exit__(self, *error):
        try:
            return self.watcher.__exit__(*error)
        finally:
            self.temp.cleanup()

    def detach(self, *flags):
        return subprocess.run(
            [*self.command, "--detach", *flags], check=True, capture_output=True, timeout=15,
            env=self.env,
        )


class BoardCleanupTests(unittest.TestCase):
    def test_a_running_board_with_a_relative_config_is_found(self):
        with BoardFixture() as board:
            command = [board.malak, "board", "start", "--config", board.config.name]
            with start_test_board(command, env=board.env, cwd=board.root):
                self.assertEqual({entry.config for entry in board_processes(board.root)},
                                 {board.config})

    def test_a_failed_fixture_reaps_its_detached_board(self):
        with self.assertRaisesRegex(RuntimeError, "intentional test failure"):
            with BoardFixture() as board:
                board.detach()
                self.assertTrue(board_processes(board.root))
                raise RuntimeError("intentional test failure")

        self.assertFalse(board_processes(board.root))
        self.assertFalse(board.root.exists())

    def test_a_readiness_timeout_kills_the_board_process_group(self):
        with BoardFixture() as board:
            # Delay exec long enough to cross the readiness deadline, with a
            # child in the same group as the launcher.
            command = ["/bin/sh", "-ec", 'sleep 60 & echo "$!" > child; wait']
            with self.assertRaisesRegex(RuntimeError, "missed readiness"):
                with start_test_board(command, env=board.env, timeout=0.5, cwd=board.root):
                    self.fail("the board should miss readiness")
            pid = (board.root / "child").read_text().strip()
            deadline = time.monotonic() + 5
            while True:
                rows = subprocess.run(["ps", "-p", pid, "-o", "stat="], capture_output=True, text=True)
                state = rows.stdout.strip()
                if not state or state.startswith("Z"):
                    break
                self.assertLess(time.monotonic(), deadline, "the board's child is still alive")
                time.sleep(0.025)

    def test_an_interrupted_fixture_reaps_its_detached_board(self):
        board = BoardFixture()
        self.addCleanup(board.temp.cleanup)
        self.addCleanup(stop_boards, board.root)
        # In the subprocess, interruption skips Python cleanup. Its watcher
        # sees EOF exactly as it would if the Cargo test binary were killed.
        code = """
import signal
import subprocess
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from board_processes import watch_board_home
with watch_board_home(Path(sys.argv[2])):
    subprocess.run([*sys.argv[3:], "--detach"], check=True, timeout=15, capture_output=True)
    print("armed", flush=True)
    signal.pause()
"""
        with subprocess.Popen(
            [sys.executable, "-B", "-c", code, FIXTURES, board.root, board.malak, "board", "start"],
            env=board.env, stdout=subprocess.PIPE, text=True, start_new_session=True,
        ) as runner:
            try:
                self.assertTrue(select.select([runner.stdout], [], [], 20)[0])
                self.assertEqual(runner.stdout.readline().strip(), "armed")
                self.assertTrue(board_processes(board.root))
            finally:
                runner.terminate()
                runner.wait(timeout=5)
            deadline = time.monotonic() + 5
            while board.root.exists() and time.monotonic() < deadline:
                time.sleep(0.025)

        self.assertFalse(board_processes(board.root))
        self.assertFalse(board.root.exists())
