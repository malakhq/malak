"""Exercise the private folder and run id each check run gets."""

import os
from pathlib import Path
import pwd
import stat
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from process_cleanup import private_check_run


class PrivateCheckRunTests(unittest.TestCase):
    def setUp(self):
        self.home = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.enterContext(patch.object(pwd, "getpwuid", return_value=SimpleNamespace(pw_dir=str(self.home))))
        self.cache = self.home / ("Library/Caches" if sys.platform == "darwin" else ".cache")

    def test_the_run_gets_a_private_temp_folder_under_the_home(self):
        with private_check_run({}) as env:
            temporary = Path(env["TMPDIR"])
            self.assertEqual(temporary.parent, self.cache)
            self.assertEqual(stat.S_IMODE(temporary.stat().st_mode), 0o700)

    def test_the_temp_folder_is_named_for_the_run_id(self):
        with private_check_run({}) as env:
            self.assertEqual(Path(env["TMPDIR"]).name, f"malak-check-{env['MALAK_CHECK_RUN']}")

    def test_the_environment_keeps_the_callers_variables(self):
        with private_check_run({"KEPT": "yes", "TMPDIR": "/tmp"}) as env:
            self.assertEqual(env["KEPT"], "yes")
            self.assertNotEqual(env["TMPDIR"], "/tmp")

    def test_each_run_has_its_own_id_and_folder(self):
        with private_check_run({}) as first, private_check_run({}) as second:
            self.assertNotEqual(first["MALAK_CHECK_RUN"], second["MALAK_CHECK_RUN"])
            self.assertNotEqual(first["TMPDIR"], second["TMPDIR"])

    def test_the_temp_folder_and_the_runs_homes_are_removed_afterwards(self):
        with private_check_run({}) as env:
            home = self.home / ".m" / env["MALAK_CHECK_RUN"] / "fixture"
            home.mkdir(parents=True)
            (Path(env["TMPDIR"]) / "malak-scratch").write_text("data")

        self.assertEqual(list(self.cache.iterdir()), [])
        self.assertFalse((self.home / ".m").exists())

    def test_another_runs_homes_survive(self):
        other = self.home / ".m/other/fixture"
        other.mkdir(parents=True)

        with private_check_run({}):
            pass

        self.assertTrue(other.is_dir())

    def test_the_folder_is_removed_when_the_run_fails(self):
        with self.assertRaises(RuntimeError):
            with private_check_run({}):
                raise RuntimeError("check failed")

        self.assertEqual(list(self.cache.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
