"""Recipes name no agent tool or model, and a new tool is one adapter table."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

RECIPES = Path(__file__).resolve().parents[1]
NAMES = re.compile(r"claude|codex|sonnet|opus|haiku|gpt|anthropic|openai|gemini|durative", re.IGNORECASE)

# A third tool, written as one adapter table and nothing else. Its payloads carry
# `acme_event` and `acme_session`, its replies are `{"acme": {"say": TEXT}}`, and
# it delivers messages through the generic hook format.
ACME = '''
import json

from adapters import Adapter

EVENTS = {"prompt": "UserPromptSubmit", "before_tool": "PreToolUse", "after_tool": "PostToolUse",
          "start": "SessionStart", "end": "SessionEnd", "stop": "Stop"}


def translate(payload):
    if "acme_event" not in payload:
        return payload
    return {"hook_event_name": EVENTS[payload["acme_event"]], "session_id": payload["acme_session"],
            "tool_name": payload.get("acme_tool"), "tool_input": payload.get("acme_input"),
            "tool_use_id": payload.get("acme_call")}


def reply(event, value):
    return {"acme": {"say": value}} if event in ("SessionStart", "UserPromptSubmit") else None


class AskOwner:
    @staticmethod
    def normalize(question):
        return question

    @staticmethod
    def key(question, text):
        return text


ADAPTER = Adapter(
    key="acme-agent",
    display="Acme Agent",
    programs=("acme",),
    rank=2,
    matches=lambda payload: "acme_event" in payload,
    context_events=("SessionStart", "UserPromptSubmit"),
    reply=reply,
    translate=translate,
    delivery_tool="generic",
    config_env="ACME_HOME",
    config_dir=".acme",
    settings_file="acme-hooks.json",
    instruction_file="ACME.md",
    hook_arguments=lambda events, env: ["--acme-hooks", json.dumps(sorted(events))],
    delegation_tools={"spawn_helper": "acme"},
    start_examples={"acme": "acme run ..."},
    question_tools={"ask_owner": AskOwner},
    loop_hint="In Acme Agent use its scheduler.",
    background_hint="an Acme background task",
    background_use="an Acme background task",
)
'''


def run(*command, **options):
    return subprocess.run(command, capture_output=True, text=True, **options)


class NoToolNamesTests(unittest.TestCase):
    def test_no_recipe_file_outside_the_adapters_names_a_tool_or_model(self):
        found = []
        for path in sorted(RECIPES.iterdir()):
            if path.is_dir() or path.name.startswith("adapter"):
                continue
            for number, line in enumerate(path.read_text().splitlines(), 1):
                if NAMES.search(line):
                    found.append(f"{path.name}:{number}: {line.strip()}")
        self.assertEqual(found, [], "move tool and model names into an adapter table")

    def test_the_adapters_are_where_the_names_live(self):
        for path in RECIPES.glob("adapter_*.py"):
            self.assertTrue(path.name in {"adapter_claude.py", "adapter_codex.py", "adapter_codex_notice.py",
                                          "adapter_durative.py", "adapter_generic.py"}, path.name)


class ThirdToolTests(unittest.TestCase):
    """An `acme-agent` adapter, added to a copy of the recipes with no other change."""

    def setUp(self):
        self.temp = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.recipes = self.temp / "recipes"
        shutil.copytree(RECIPES, self.recipes, ignore=shutil.ignore_patterns("tests", "__pycache__"))
        (self.recipes / "adapter_acme.py").write_text(ACME)
        self.home = self.temp / "home"
        (self.home / ".acme").mkdir(parents=True)
        self.env = {**os.environ, "HOME": str(self.home), "TMPDIR": str(self.temp),
                    "MALAK_RECIPE_EXE": "malak", "PYTHONDONTWRITEBYTECODE": "1"}
        for variable in ("MALAK_HANDOVER", "MALAK_RECIPE_CONFIG", "ACME_HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
            self.env.pop(variable, None)

    def python(self, code):
        result = run(sys.executable, "-c", f"import sys, json; sys.path.insert(0, {str(self.recipes)!r})\n{code}",
                     env=self.env, cwd=self.temp)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def test_registration_writes_the_tools_hook_file_and_skill(self):
        result = run(sys.executable, str(self.recipes / "agents"), "--yes", env=self.env, cwd=self.temp)
        self.assertEqual(result.returncode, 0, result.stderr)
        hooks = json.loads((self.home / ".acme/acme-hooks.json").read_text())["hooks"]
        command = hooks["UserPromptSubmit"][0]["hooks"][0]["command"]
        self.assertTrue(command.endswith("recipe observe --reminder --tool acme-agent"), command)
        skill = (self.home / ".acme/skills/malak/SKILL.md").read_text()
        self.assertIn("Acme Agent", skill)

    def test_a_payload_is_recognised_and_answered_in_the_tools_reply_format(self):
        found = self.python(
            "import hook_format\n"
            "payload = {'acme_event': 'prompt', 'acme_session': 's1'}\n"
            "print(json.dumps([hook_format.tool_for(payload), hook_format.tool_for({'event': 'x'}),"
            " hook_format.reply('acme-agent', 'UserPromptSubmit', 'hello')]))")
        self.assertEqual(found, ["acme-agent", "generic", {"acme": {"say": "hello"}}])

    def test_the_installed_reminder_speaks_the_tools_format(self):
        payload = {"acme_event": "prompt", "acme_session": "s1"}
        result = run(sys.executable, str(self.recipes / "observe"), "--reminder", "--tool", "acme-agent",
                     env=self.env, cwd=self.temp, input=json.dumps(payload))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Malak is installed", json.loads(result.stdout)["acme"]["say"])

    def test_the_tools_question_tool_is_recorded(self):
        found = self.python(
            "import question_hooks\n"
            "payload = {'hook_event_name': 'PreToolUse', 'tool_name': 'ask_owner',"
            " 'tool_input': {'questions': [{'question': 'Which build?', 'options': []}]}}\n"
            "print(json.dumps([question_hooks.is_question(payload),"
            " [[i, k] for i, k, _ in question_hooks.question_inputs(payload)]]))")
        self.assertEqual(found, [True, [["0", "Which build?"]]])

    def test_the_tools_delegation_tool_gets_the_worker_start_line(self):
        found = self.python(
            "import delegation\n"
            "payload = {'hook_event_name': 'PreToolUse', 'tool_name': 'spawn_helper'}\n"
            "kind = delegation.starts_worker(payload)\n"
            "print(json.dumps([kind, delegation.reminder(kind, 'malak --board b')]))")
        self.assertEqual(found[0], "acme")
        self.assertIn("recipe worker-start NAME --brief FILE -- <your agent command>` (e.g. acme run ...)", found[1])

    def test_worker_start_wires_the_tools_hooks_through_its_adapter(self):
        found = self.python(
            "import adapters\n"
            "adapter = adapters.native_adapter('acme')\n"
            "print(json.dumps([adapter.key, adapters.hook_tool('acme'),"
            " adapter.hook_arguments({'SessionStart': 'cmd'}, {})]))")
        self.assertEqual(found, ["acme-agent", "generic", ["--acme-hooks", '["SessionStart"]']])


if __name__ == "__main__":
    unittest.main()
