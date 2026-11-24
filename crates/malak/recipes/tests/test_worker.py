"""Check the launcher retry deadline without relying on process scheduling."""

from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import call, patch

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    import worker


class WorkerReportTests(unittest.TestCase):
    def test_retries_once_when_the_first_call_outlasts_the_window(self):
        command = ["report.sh"]
        env = {"MALAK_TEST_REPORT_RETRY_SECONDS": "6", "MALAK_REPORT_ID": "fixture-id"}
        now = 0

        def stalled_report(command, env, timeout):
            nonlocal now
            now = 7
            return subprocess.CompletedProcess(command, worker.REPORT_CONNECTION_EXIT, b"", b"")

        clock = SimpleNamespace(monotonic=lambda: now)
        with (patch.object(worker, "time", clock),
              patch.object(worker.out, "details"),
              patch.object(worker, "report_attempt", side_effect=stalled_report) as attempt):
            result = worker.send_final_report(command, env)

        self.assertEqual(result.returncode, 3)
        self.assertEqual(attempt.call_args_list, [call(command, env, 3), call(command, env, 0.1)])
