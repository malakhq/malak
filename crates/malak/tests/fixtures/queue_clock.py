"""Supply a test clock only to the queue recipe's Python process."""

import os
from pathlib import Path
import sys
import time


if Path(sys.argv[0]).name == "worker.py" and sys.argv[1:2] == ["queue"]:
    clock = Path(os.environ["TEST_QUEUE_CLOCK"])
    start = int(os.environ["TEST_QUEUE_START_NS"])

    def read_clock():
        while (value := clock.read_text()) == "paused":
            time.sleep(0.02)
        return int(value)

    def start_clock():
        # Queue preparation records the start before polling the deadline.
        time.time_ns = read_clock
        return start

    time.time_ns = start_clock
