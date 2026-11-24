#!/usr/bin/env python3
"""Simulate a replaced reporter's output inside a disposable test fixture."""

import os
from pathlib import Path
import sys


if sys.argv[3:5] != ["recipe", "worker-start"]:
    malak = os.environ["MALAK_TEST_EXE"]
    os.execv(malak, [malak, *sys.argv[1:]])

if "MALAK_DEMO_REPORT_RECORD" in os.environ:
    record = Path(os.environ["MALAK_DEMO_REPORT_RECORD"])
    receipt = record.with_suffix("")
    workspace = Path.cwd().resolve()
    assert not record.resolve().is_relative_to(workspace)
    assert not Path(os.environ["TMPDIR"]).resolve().is_relative_to(workspace)
    assert Path(os.environ["MALAK_DEMO_AGENT_TMPDIR"]).resolve().is_relative_to(workspace)
else:
    receipt = Path(os.environ["MALAK_DEMO_WORKER_RECEIPT"])
    record = receipt.with_suffix(".expected")

name = sys.argv[5]
record.write_text(f"fixtureworker\n{name}-report\n0\n")
receipt.write_text("sent\n")
Path(f"synthetic-receipt-{name}").touch()
