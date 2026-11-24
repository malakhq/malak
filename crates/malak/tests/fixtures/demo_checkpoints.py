"""Use the test watchdog for demo checkpoints around real processes."""

import os
from pathlib import Path
import secrets
import signal
import subprocess
import sys


if Path(sys.argv[0]).name == "demo.py":
    watchdog = int(os.environ["MALAK_TEST_WATCHDOG"])
    if prefix := os.environ.get("MALAK_TEST_DEMO_PREFIX"):
        secrets.token_hex = lambda _size: prefix
    real_run = subprocess.run
    refused_revoke = False

    def run(command, *args, **kwargs):
        global refused_revoke
        owner_command = command[3] if command[1:2] == ["--config"] else None
        # Allow the test board queries to finish under concurrent checks.
        if owner_command and kwargs.get("timeout") is not None:
            kwargs["timeout"] = watchdog
        if (owner_command == "revoke" and not refused_revoke
                and os.environ.get("MALAK_TEST_DEMO_REFUSE_REVOKE")):
            refused_revoke = True
            return subprocess.CompletedProcess(command, 3, "", "Fixture revocation failure")
        result = real_run(command, *args, **kwargs)
        if (owner_command == "enrol" and result.returncode == 0
                and os.environ.get("MALAK_TEST_DEMO_INTERRUPT_ENROLMENT")):
            os.kill(os.getpid(), signal.SIGINT)
        return result

    subprocess.run = run

if Path(sys.argv[0]).name == "demo.py" and os.environ.get("MALAK_TEST_DEMO_WAIT_FOR_LEAD_EXIT"):
    class WaitForLeadExit(subprocess.Popen):
        def __init__(self, command, *args, **kwargs):
            super().__init__(command, *args, **kwargs)
            if kwargs.get("start_new_session") and command[0] == "claude":
                try:
                    self.wait(timeout=watchdog)
                except subprocess.TimeoutExpired:
                    self.kill()
                    self.wait()
                    raise

    subprocess.Popen = WaitForLeadExit
