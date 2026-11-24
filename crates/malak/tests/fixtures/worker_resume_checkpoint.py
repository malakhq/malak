"""Mark the next inbox scan after the worker skips the note."""

import json
from pathlib import Path
import sys


if Path(sys.argv[0]).name == "worker.py" and sys.argv[1:2] == ["worker-start"]:
    sys.path.insert(0, str(Path(sys.argv[0]).parent))
    import board_data
    sys.path.pop(0)

    real_wait = board_data.wait_task

    def wait_task(*args):
        real_malak = board_data.malak
        read_note = False

        def observe(*words, **kwargs):
            nonlocal read_note
            if read_note and words[2:4] == ("board", "status"):
                Path("resume-note-checked").touch()
                read_note = False
            result = real_malak(*words, **kwargs)
            if words[2:5] == ("get", "--json", "--agent"):
                # The test withholds the task until the next scan begins.
                read_note = bool(json.loads(result.stdout)["messages"])
            return result

        board_data.malak = observe
        try:
            return real_wait(*args)
        finally:
            board_data.malak = real_malak

    board_data.wait_task = wait_task
