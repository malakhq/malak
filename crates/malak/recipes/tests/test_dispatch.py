"""The observe dispatcher's reply formats, delegation detection and quiet-gate parity."""

from importlib.machinery import SourceFileLoader
import importlib.util
import json
from pathlib import Path
import re
import sys
import unittest


MALAK = Path(__file__).resolve().parents[2]
RECIPES = MALAK / "recipes"
PAYLOADS = json.loads((RECIPES / "tests" / "observe-payloads.json").read_text())
# Captured from `claude -p` 2.1.292 and `codex exec` 0.169, with ids and paths scrubbed.
LIVE = json.loads((RECIPES / "tests" / "live-payloads.json").read_text())
sys.path.insert(0, str(RECIPES))

import delegation  # noqa: E402
import hook_format  # noqa: E402
import question_hooks  # noqa: E402


def load(name):
    loader = SourceFileLoader(f"recipe_{name}", str(RECIPES / name))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class ReplyFormatTests(unittest.TestCase):
    def test_native_tools_add_context_to_each_event_that_takes_it(self):
        for tool in ("claude", "codex"):
            for event in hook_format.NATIVE_CONTEXT:
                with self.subTest(tool=tool, event=event):
                    self.assertEqual(hook_format.reply(tool, event, "text"), {
                        "hookSpecificOutput": {"hookEventName": event, "additionalContext": "text"},
                    })

    def test_native_tools_pass_a_stop_reply_through(self):
        for tool in ("claude", "codex"):
            for reply in ({"decision": "block", "reason": "r"}, {"systemMessage": "m"}):
                self.assertEqual(hook_format.reply(tool, "Stop", reply), reply)

    def test_the_generic_format_continues_where_native_tools_block(self):
        self.assertEqual(
            hook_format.reply("generic", "Stop", {"decision": "block", "reason": "r"}),
            {"decision": "continue", "context": "r"},
        )
        self.assertEqual(hook_format.reply("generic", "PostToolUse", "text"), {"context": "text"})

    def test_events_without_a_reply_shape_and_empty_replies_print_nothing(self):
        for event in ("SubagentStop", "SessionEnd", "PermissionRequest"):
            self.assertIsNone(hook_format.reply("claude", event, "text"))
        self.assertIsNone(hook_format.reply("codex", "PreToolUse", ""))

    def test_the_tool_comes_from_the_command_then_from_the_payload(self):
        self.assertEqual(hook_format.tool_for(LIVE["codex_session_start"], "codex"), "codex")
        self.assertEqual(hook_format.tool_for(LIVE["claude_agent"]), "claude")
        self.assertEqual(hook_format.tool_for(LIVE["codex_ls"]), "codex")
        self.assertEqual(hook_format.tool_for(PAYLOADS["generic_question_start"], "claude"), "generic")


class DelegationTests(unittest.TestCase):
    def kind(self, command, **changes):
        payload = {"hook_event_name": "PreToolUse", "tool_name": "Bash",
                   "tool_input": {"command": command}, **changes}
        return delegation.starts_worker(payload)

    def test_the_sub_agent_tool_and_the_two_runners_are_delegation(self):
        self.assertEqual(delegation.starts_worker(LIVE["claude_agent"]), "agent")
        self.assertEqual(delegation.starts_worker(
            {**LIVE["codex_ls"], "tool_input": {"command": "codex exec 'Survey the tests'"}}), "codex")
        self.assertEqual(self.kind("durative exec ID -- codex exec go"), "durative")
        self.assertEqual(self.kind("FOO=1 /usr/bin/env codex -m gpt-5.4 exec go"), "codex")
        self.assertEqual(self.kind("cd x && (codex exec go &)"), "codex")
        self.assertEqual(self.kind("bash -c 'codex exec go'"), "codex")

    def test_other_calls_are_not(self):
        for command in ("ls", "echo codex exec", "grep -r 'codex exec' .", "codex resume",
                        "malak recipe worker-start w --brief b -- codex exec go", "codex 'unclosed"):
            with self.subTest(command=command):
                self.assertIsNone(self.kind(command))
        for payload in (LIVE["claude_subagent_bash_done"], LIVE["codex_ls"], LIVE["codex_ls_done"],
                        LIVE["claude_agent_done"], PAYLOADS["claude_background"]):
            self.assertIsNone(delegation.starts_worker(payload))

    def test_a_subagent_is_not_the_lead(self):
        # A live subagent's own tool calls carry agent_id and agent_type.
        self.assertIn("agent_id", LIVE["claude_subagent_bash"])
        self.assertIsNone(delegation.starts_worker({**LIVE["claude_agent"], "agent_id": "child"}))
        self.assertIsNone(delegation.starts_worker(
            {**LIVE["claude_subagent_bash"], "tool_input": {"command": "codex exec go"}}))


class LivePayloadTests(unittest.TestCase):
    def test_live_payloads_use_only_registered_events(self):
        registered = set(load("agents").EVENTS)
        self.assertEqual({payload["hook_event_name"] for payload in LIVE.values()} - registered, set())

    def test_each_tool_has_a_reply_for_the_context_events_it_sent(self):
        for name, payload in LIVE.items():
            tool = name.split("_")[0]
            event = payload["hook_event_name"]
            with self.subTest(name=name):
                reply = hook_format.reply(tool, event, "text")
                if event in hook_format.NATIVE_CONTEXT:
                    self.assertEqual(reply["hookSpecificOutput"]["hookEventName"], event)
                elif event != "Stop":
                    self.assertIsNone(reply)


class QuietGateTests(unittest.TestCase):
    """The Rust gate answers before Python runs, so its lists must match the recipes'."""

    @staticmethod
    def rust_list(name):
        source = (MALAK / "src/cli/recipe/quiet.rs").read_text()
        body = re.search(rf"const {name}: \[&str; \d+\] = \[(.*?)\];", source, re.S).group(1)
        return set(re.findall(r'"(\w+)"', body))

    def test_the_gate_passes_every_event_the_recipe_registers_except_subagent_stop(self):
        registered = set(load("agents").EVENTS)
        self.assertEqual(self.rust_list("EVENTS"), registered - {"SubagentStop"})

    def test_the_gate_passes_every_post_tool_the_recipe_records(self):
        observe = load("observe")
        recorded = observe.TASK_TOOLS | (question_hooks.TOOLS - {"generic"})
        self.assertEqual(self.rust_list("POST_TOOLS"), recorded)


if __name__ == "__main__":
    unittest.main()
