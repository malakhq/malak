"""Exercise the adopt picker and its output without a live board."""

from io import StringIO
import os
from pathlib import Path
import runpy
import shlex
import sys
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch


RECIPES = Path(__file__).resolve().parents[1]
with patch.object(sys, "path", [str(RECIPES), *sys.path]):
    adopt = runpy.run_path(str(RECIPES / "adopt"))


class AdoptTests(unittest.TestCase):
    def test_lists_claude_and_codex_processes_only(self):
        ps = """\
 501 64 Ss Tue Nov 24 10:00:00 2026 /Applications/Claude Code/Contents/MacOS/claude
 501 17 S+ Mon Nov 23 09:00:00 2026 codex
 501 83 S  Tue Nov 24 10:01:00 2026 codex-code-mode-host
 501 84 Z  Tue Nov 24 10:02:00 2026 codex
 502 85 S  Tue Nov 24 10:03:00 2026 claude
"""
        with (patch.object(os, "geteuid", return_value=501),
              patch("subprocess.run", return_value=SimpleNamespace(stdout=ps))):
            sessions = adopt["running_sessions"]()
        self.assertEqual(sessions, [(17, "codex", "Mon Nov 23 09:00:00 2026"),
                                    (64, "claude", "Tue Nov 24 10:00:00 2026")])

    def test_picker_displays_metadata_and_selects_the_number_entered(self):
        sessions = [adopt["Session"](17, "claude", "Mon Nov 23 09:00:00 2026"),
                    adopt["Session"](24, "codex", "Tue Nov 24 10:00:00 2026")]
        menu = StringIO()
        with patch.object(sys, "stdin", StringIO("3\n2\n")), patch.object(sys, "stderr", menu):
            selected = adopt["pick_session"](sessions, {17: "/work/first", 24: "/work/second"})
        self.assertEqual(selected, sessions[1])
        self.assertIn("24  codex  Tue Nov 24 10:00:00 2026  /work/second", menu.getvalue())

    def test_picker_refuses_a_pid_reused_after_listing(self):
        select = adopt["select_session"]
        session = adopt["Session"](17, "codex", "started")
        with (patch.dict(select.__globals__, {"running_sessions": lambda: [session]}),
              patch.object(adopt["board"], "malak", side_effect=[
                  SimpleNamespace(stdout='{"pid": 17, "start_time": 101, "program": "codex", "cwd": "/work"}'),
                  SimpleNamespace(stdout='{"pid": 17, "start_time": 102, "program": "codex", "cwd": "/work"}'),
              ]),
              patch.object(sys, "stdin", StringIO("1\n")), patch.object(sys, "stderr", StringIO()),
              self.assertRaisesRegex(adopt["board"].RecipeError, "ended before adoption")):
            select()

    def test_refuses_to_prompt_without_a_terminal(self):
        error = StringIO()
        with (patch.object(sys, "argv", ["adopt", "researcher"]),
              patch.object(sys, "stdin", StringIO()), patch.object(sys, "stderr", error),
              self.assertRaises(SystemExit) as exit):
            adopt["arguments"]()
        self.assertEqual(exit.exception.code, 2)
        self.assertIn("--session PID", error.getvalue())

    def test_issues_the_handover_to_the_selected_pid(self):
        with patch.object(adopt["board"], "malak",
                          return_value=SimpleNamespace(stdout='{"id":"worker-id"}')) as enrol:
            participant = adopt["issue"](Path("board.toml"), "researcher", 24, Path("handover"),
                                         ["lead:send", "extra:read"])
        self.assertEqual(participant, "worker-id")
        enrol.assert_called_once_with("--config", Path("board.toml"), "enrol", "researcher",
                                      "--session", "24", "--out", Path("handover"), "--json",
                                      "--grant", "lead:send", "--grant", "extra:read")

    def test_bootstrap_quotes_the_handover_path_without_reading_its_key(self):
        with TemporaryDirectory() as root:
            handover = Path(root) / "priv 'ate" / "handover"
            handover.parent.mkdir()
            handover.write_bytes(b"secret_key_material")
            text = adopt["bootstrap"]("--board team", "researcher", handover, "to/worker-id")
        export = next(line for line in text.splitlines() if line.startswith("export "))
        self.assertEqual(shlex.split(export), ["export", f"MALAK_HANDOVER={handover}"])
        self.assertNotIn("secret_key_material", text)
        self.assertIn("malak --board team read --ack to/worker-id", text)
        self.assertIn("malak --board team send lead -", text)

    def test_adopt_again_uses_the_existing_participant_and_new_handover(self):
        issued = []
        participants = []
        participant = "worker-id"

        def malak(_selector, _config, command, *args):
            if command == "enrol":
                pid = int(args[args.index("--session") + 1])
                handover = args[args.index("--out") + 1]
                handover.write_bytes(b"secret_key_material")
                issued.append((pid, handover))
                if not participants:
                    participants.append({"id": participant, "name": "researcher", "revoked": False})
                return SimpleNamespace(stdout='{"id":"worker-id"}')
            self.assertIn(command, {"channel", "grant"})

        with TemporaryDirectory() as root, patch.object(adopt["board"], "malak", malak):
            state = Path(root)
            connection = SimpleNamespace(config=state / "board.toml", state=state,
                                         identity=(None, "lead", "board-key"),
                                         status=lambda: {"participants": participants})
            first, inbox = adopt["adopt_worker"](connection, "researcher", 24, [])
            second, second_inbox = adopt["adopt_worker"](connection, "researcher", 29, [])
            record = adopt["LaunchRecord"](state, participant).load()

        self.assertEqual(inbox, "to/worker-id")
        self.assertEqual(second_inbox, inbox)
        self.assertNotEqual(first, second)
        self.assertEqual(issued, [(24, first), (29, second)])
        self.assertEqual(record["agent_pid"], 29)

    def test_a_refused_session_leaves_no_handover_directory(self):
        with TemporaryDirectory() as root:
            state = Path(root)
            connection = SimpleNamespace(config=state / "board.toml", state=state,
                                         status=lambda: {"participants": []})
            refusal = adopt["board"].RecipeError("session PID is no longer running")
            with (patch.object(adopt["board"], "malak", side_effect=refusal),
                  self.assertRaisesRegex(adopt["board"].RecipeError, "no longer running")):
                adopt["adopt_worker"](connection, "researcher", 24, [])
            self.assertEqual(list(state.glob("adopt.*")), [])
