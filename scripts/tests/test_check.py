"""Exercise the workspace check's temp snapshot with a disposable checkout."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


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
        self.check = scripts / "check"
        self.temp = root / "temp"
        self.temp.mkdir()
        self.cargo = root / "cargo"
        self.env = {**os.environ, "TMPDIR": str(self.temp), "PATH": f"{root}{os.pathsep}{os.environ['PATH']}"}

    def run_check(self, fake_cargo):
        self.cargo.write_text(f"#!/bin/sh\nset -eu\n{fake_cargo}\n")
        self.cargo.chmod(0o700)
        return subprocess.run([self.check], env=self.env, text=True, capture_output=True)

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
