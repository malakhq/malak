"""Replay sanitized startup captures and vendor hook examples on local boards."""

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
OBSERVE = ROOT / "recipes" / "observe"
PAYLOADS = json.loads(Path(__file__).with_name("observe-payloads.json").read_text())


def run_observe(payload, env):
    process = subprocess.run(
        [OBSERVE], input=json.dumps(payload), env=env, text=True,
        capture_output=True, check=True, timeout=5,
    )
    return json.loads(process.stdout)["hookSpecificOutput"] if process.stdout else None


def sample_handover(path):
    # A local v1 general handover with inert test keys. The five-board
    # integration tests below also use real CLI-generated handovers.
    address = b"unix:/mock-malaks/current/p/s"
    path.write_bytes(
        b"\x89\x01\x01\x01\x78" + bytes([len(address)]) + address
        + b"\x58\x20" + b"X" * 32 + b"\x6bparticipant"
        + b"\x63key\x40\x58\x20" + b"Y" * 32
    )
    path.chmod(0o600)


class ObserveTests(unittest.TestCase):
    def setUp(self):
        self.temp = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.env = {
            **os.environ, "MALAK_OBSERVE": "1", "TMPDIR": str(self.temp),
            "HOME": str(self.temp), "XDG_CONFIG_HOME": str(self.temp / "xdg"),
        }
        self.env.pop("MALAK_HANDOVER", None)
        self.env.pop("MALAK_RECIPE_CONFIG", None)

    def on_board(self):
        handover = self.temp / "participant.handover"
        sample_handover(handover)
        self.env["MALAK_HANDOVER"] = str(handover)

    def invoke(self, name, **changes):
        return run_observe({**PAYLOADS[name], **changes}, self.env)

    def markers(self):
        return list(self.temp.glob("malak-observe-*"))

    def test_the_guard_requires_one(self):
        self.on_board()
        for value in (None, "0", "true"):
            with self.subTest(value=value):
                if value is None:
                    self.env.pop("MALAK_OBSERVE")
                else:
                    self.env["MALAK_OBSERVE"] = value
                self.assertIsNone(self.invoke("claude_start"))
                self.assertEqual(self.markers(), [])

    def test_top_level_gets_one_startup_line_without_a_handover(self):
        for name in ("claude_start", "codex_start"):
            with self.subTest(name=name):
                output = self.invoke(name)
                self.assertEqual(output["hookEventName"], "SessionStart")
                self.assertEqual(output["additionalContext"], (
                    "Malak is installed. If this chat already leads a board, keep leading "
                    "it with its --board name. Otherwise ask the owner before leading or joining a board."
                ))
                self.assertIsNone(self.invoke(name, source="resume"))

    def test_subagents_without_a_handover_are_silent(self):
        for name in ("claude_subagent", "codex_subagent"):
            with self.subTest(name=name):
                self.assertIsNone(self.invoke(name))

    def test_session_start_inside_a_subagent_does_not_get_the_startup_line(self):
        self.assertIsNone(self.invoke("claude_start", agent_id="agent-abc123"))

    def test_tool_calls_without_a_handover_are_silent(self):
        for name in ("claude_background", "codex_spawn", "codex_bash", "codex_plan"):
            with self.subTest(name=name):
                self.assertIsNone(self.invoke(name))

    def test_subagent_context_uses_the_inherited_identity(self):
        self.on_board()
        for name in ("claude_subagent", "codex_subagent"):
            with self.subTest(name=name):
                output = self.invoke(name)
                self.assertEqual(output["hookEventName"], "SubagentStart")
                self.assertIn('You are participant on board "/mock-malaks/current"', output["additionalContext"])

    def test_unrelated_shell_commands_are_silent(self):
        self.on_board()
        for command in ("ls", "printf '%s' '&'", r"echo \&", "echo done # &"):
            with self.subTest(command=command):
                self.assertIsNone(self.invoke("codex_bash", tool_input={"command": command}))

    def test_relevant_tool_calls_return_vendor_context(self):
        self.on_board()
        calls = (
            ("claude_background", {}),
            ("claude_task", {}),
            ("codex_spawn", {}),
            ("codex_bash", {}),
            ("codex_bash", {"tool_input": {"command": "malak board start --detach"}}),
            ("codex_bash", {"tool_input": {"command": "malak --board team board start --detach"}}),
            ("codex_plan", {}),
        )
        for session, (name, changes) in enumerate(calls):
            with self.subTest(name=name, changes=changes):
                payload = {**PAYLOADS[name], **changes, "session_id": str(session)}
                output = run_observe(payload, self.env)
                self.assertEqual(output["hookEventName"], payload["hook_event_name"])

    def test_activity_is_suppressed_until_the_cooldown_elapses(self):
        self.on_board()
        self.assertIsNotNone(self.invoke("codex_bash"))
        self.assertIsNone(self.invoke("codex_plan"))
        activity = self.markers()[0] / "activity"
        first = time.time() - 61
        os.utime(activity, (first, first))
        self.assertIsNotNone(self.invoke("codex_plan"))

    def test_parent_cooldown_does_not_suppress_a_new_subagent(self):
        self.on_board()
        self.assertIsNotNone(self.invoke("codex_spawn"))
        self.assertIsNotNone(self.invoke("codex_subagent"))

    def test_simultaneous_startup_hooks_show_one_nudge(self):
        with ThreadPoolExecutor(max_workers=6) as pool:
            outputs = list(pool.map(run_observe, [PAYLOADS["codex_start"]] * 6, [self.env] * 6))
        self.assertEqual(sum(output is not None for output in outputs), 1)

    def test_session_markers_store_only_timestamps(self):
        self.on_board()
        for name in ("codex_start", "codex_plan", "codex_subagent"):
            self.invoke(name)
        directory, = self.markers()
        self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
        self.assertNotIn(PAYLOADS["codex_start"]["session_id"], directory.name)
        markers = list(directory.iterdir())
        self.assertEqual(len(markers), 3)
        for marker in markers:
            self.assertNotIn(PAYLOADS["codex_subagent"]["agent_id"], marker.name)
            self.assertEqual(marker.read_bytes(), b"")
            self.assertLess(abs(time.time() - marker.stat().st_mtime), 60)

    def test_session_end_removes_its_markers(self):
        for vendor in ("claude", "codex"):
            with self.subTest(vendor=vendor):
                self.invoke(f"{vendor}_start")
                self.assertIsNone(self.invoke(f"{vendor}_end"))
                self.assertEqual(self.markers(), [])

    def test_unreadable_handover_does_not_fall_back_to_the_no_board_nudge(self):
        for path in ("", str(self.temp / "missing.handover")):
            with self.subTest(path=path):
                self.env["MALAK_HANDOVER"] = path
                self.assertIsNone(self.invoke("codex_start"))


class RunningBoardsTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        built = ROOT.parents[1] / "target/debug/malak"
        cls.malak = built if built.is_file() else shutil.which("malak")
        if cls.malak is None:
            raise RuntimeError("Build malak with cargo build -p malak or add malak to PATH")
        # Board sockets live under this folder, so it must not depend on how long
        # the checkout path is (SUN_LEN is 104 on macOS).
        cls.root = Path(cls.enterClassContext(tempfile.TemporaryDirectory(
            prefix="mk-", dir=os.path.realpath(tempfile.gettempdir()))))
        cls.real_registry = Path.home() / ".malak" / "boards.jsonl"
        cls.registry_before = cls.registry_snapshot()
        cls.addClassCleanup(cls.check_real_registry)
        cls.env = {
            **os.environ,
            "HOME": str(cls.root),
            "XDG_CONFIG_HOME": str(cls.root / "xdg"),
            "MALAK_RECIPE_EXE": str(cls.malak),
        }
        cls.env.pop("MALAK_HANDOVER", None)
        cls.env.pop("MALAK_RECIPE_CONFIG", None)
        cls.configs = []
        cls.sockets = []
        for name in ("north", "south", "east", "west", "central"):
            config = cls.root / f"{name}.toml"
            state = cls.root / f"{name}-state"
            socket = cls.root / name
            config.write_text(
                f"[board]\nname = {json.dumps(name)}\nstate = {json.dumps(str(state))}\n"
                f"socket_folder = {json.dumps(str(socket))}\n"
            )
            cls.addClassCleanup(cls.owner, config, "board", "stop", check=False)
            cls.owner(config, "board", "start", "--detach")
            cls.configs.append(config)
            cls.sockets.append(socket)

        cls.handover = cls.root / "worker.handover"
        cls.owner(cls.configs[2], "enrol", "reviewer", "--out", cls.handover)

    @classmethod
    def owner(cls, config, *words, check=True):
        process = subprocess.run(
            [cls.malak, "--config", config, *words], capture_output=True, text=True, timeout=12,
            env=cls.env,
        )
        if check and process.returncode != 0:
            raise RuntimeError(process.stderr or process.stdout)
        return process.stdout

    @classmethod
    def registry_snapshot(cls):
        try:
            with cls.real_registry.open("rb") as registry:
                return os.fstat(registry.fileno()).st_ino, registry.read()
        except FileNotFoundError:
            return None

    @classmethod
    def check_real_registry(cls):
        if cls.registry_snapshot() != cls.registry_before:
            raise AssertionError(f"a test changed the real board list: {cls.real_registry}")

    def setUp(self):
        self.env = {
            **type(self).env,
            "MALAK_OBSERVE": "1",
            "MALAK_HANDOVER": str(self.handover),
            "TMPDIR": self.enterContext(tempfile.TemporaryDirectory(dir=self.root)),
        }

    def test_nudge_identifies_only_its_handover_board_among_five_running_boards(self):
        context = run_observe(PAYLOADS["codex_start"], self.env)["additionalContext"]
        self.assertIn('You are reviewer on board "east"', context)
        for other in self.sockets[:2] + self.sockets[3:]:
            self.assertNotIn(other.name, context)

    def test_board_starts_register_in_the_fixture_home(self):
        with (self.root / ".malak" / "boards.jsonl").open() as registry:
            entries = {Path(json.loads(line)) for line in registry if line.strip()}
        self.assertEqual(entries, set(self.configs))

    def test_the_leads_startup_nudge_names_the_lead_monitor(self):
        self.env["MALAK_HANDOVER"] = str(self.root / "east-state" / "lead.handover")
        self.env["CODEX_SESSION_ID"] = PAYLOADS["codex_start"]["session_id"]
        subprocess.run(
            [self.malak, "--config", self.configs[2], "send", "lead", "Lead check"],
            capture_output=True, check=True, timeout=12, env=self.env,
        )
        context = run_observe(PAYLOADS["codex_start"], self.env)["additionalContext"]
        self.assertIn(" mailbox --plain --for lead --next`", context)

    def test_observe_posts_nothing_to_any_running_board(self):
        def channels():
            return [
                json.loads(self.owner(config, "board", "status", "--json"))["channels"]
                for config in self.configs
            ]

        before = channels()
        for payload in PAYLOADS.values():
            run_observe(payload, self.env)
        self.assertEqual(channels(), before)


if __name__ == "__main__":
    unittest.main()
