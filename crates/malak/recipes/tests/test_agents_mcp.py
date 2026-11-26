"""`recipe agents` registers the MCP server in each tool's own MCP file, and only there."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest

RECIPES = Path(__file__).resolve().parents[1]
EXE = "/opt/malak/bin/malak"


class AgentsMcpTests(unittest.TestCase):
    def setUp(self):
        self.temp = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.home = self.temp / "home"
        for folder in (".claude", ".codex"):
            (self.home / folder).mkdir(parents=True)
        self.env = {**os.environ, "HOME": str(self.home), "TMPDIR": str(self.temp),
                    "MALAK_RECIPE_EXE": EXE, "PYTHONDONTWRITEBYTECODE": "1"}
        for variable in ("MALAK_HANDOVER", "MALAK_RECIPE_CONFIG", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
            self.env.pop(variable, None)
        self.claude = self.home / ".claude.json"
        self.codex = self.home / ".codex/config.toml"

    def agents(self, *flags):
        result = subprocess.run([sys.executable, "-B", str(RECIPES / "agents"), "--yes", *flags],
                                env=self.env, cwd=self.temp, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def write(self, path, text):
        path.write_text(text)
        path.chmod(0o600)

    def entry(self):
        return json.loads(self.claude.read_text())["mcpServers"]["malak"]

    def test_the_server_is_added_to_the_claude_mcp_file_beside_other_servers(self):
        self.write(self.claude, json.dumps({"numStartups": 3, "mcpServers": {"other": {"command": "x"}}}))

        self.agents()

        doc = json.loads(self.claude.read_text())
        self.assertEqual(doc["numStartups"], 3)
        self.assertEqual(doc["mcpServers"]["other"], {"command": "x"})
        self.assertEqual(self.entry(), {"type": "stdio", "command": EXE, "args": ["mcp"], "env": {}})

    def test_the_server_is_added_to_the_codex_config_after_existing_settings(self):
        self.write(self.codex, 'model = "x"\n\n[mcp_servers.other]\ncommand = "y"\n')

        self.agents()

        doc = tomllib.loads(self.codex.read_text())
        self.assertEqual(doc["model"], "x")
        self.assertEqual(doc["mcp_servers"]["other"], {"command": "y"})
        self.assertEqual(doc["mcp_servers"]["malak"], {"command": EXE, "args": ["mcp"]})

    def test_a_tool_without_an_mcp_file_gets_none_created(self):
        self.agents()

        self.assertFalse(self.claude.exists())
        self.assertFalse(self.codex.exists())

    def test_an_existing_malak_server_is_kept_and_not_removed_on_uninstall(self):
        mine = {"command": "/mine/malak", "args": ["mcp", "--board", "work"]}
        self.write(self.claude, json.dumps({"mcpServers": {"malak": mine}}))

        output = self.agents()
        self.agents("--remove")

        self.assertIn("Keeping existing MCP server malak", output)
        self.assertEqual(self.entry(), mine)

    def test_uninstall_removes_only_the_server_it_added(self):
        self.write(self.claude, json.dumps({"mcpServers": {"other": {"command": "x"}}}))
        self.write(self.codex, 'model = "x"\n')
        self.agents()

        self.agents("--remove")

        self.assertEqual(json.loads(self.claude.read_text()), {"mcpServers": {"other": {"command": "x"}}})
        self.assertEqual(self.codex.read_text(), 'model = "x"\n')

    def test_uninstall_drops_an_mcp_object_the_install_created(self):
        self.write(self.claude, json.dumps({"numStartups": 1}))
        self.agents()

        self.agents("--remove")

        self.assertEqual(json.loads(self.claude.read_text()), {"numStartups": 1})

    def test_uninstall_keeps_a_server_the_owner_edited(self):
        self.write(self.claude, "{}")
        self.agents()
        doc = json.loads(self.claude.read_text())
        doc["mcpServers"]["malak"]["args"] = ["mcp", "--board", "work"]
        self.write(self.claude, json.dumps(doc))

        output = self.agents("--remove")

        self.assertIn("Keeping edited or removed MCP server malak", output)
        self.assertEqual(self.entry()["args"], ["mcp", "--board", "work"])

    def test_a_dry_run_changes_no_mcp_file(self):
        self.write(self.claude, "{}")

        output = self.agents("--dry-run")

        self.assertIn("Would add MCP server malak", output)
        self.assertEqual(self.claude.read_text(), "{}")

    def test_an_install_from_before_the_server_gets_it_on_the_next_run(self):
        self.agents()
        self.write(self.claude, "{}")

        output = self.agents()

        self.assertIn("Added MCP server malak", output)
        self.assertEqual(self.entry()["command"], EXE)

    def test_an_upgrade_refresh_leaves_the_mcp_files_alone(self):
        self.agents()
        self.write(self.claude, "{}")

        self.agents("--upgrade-refresh")

        self.assertEqual(self.claude.read_text(), "{}")

    def test_the_claude_file_follows_the_config_folder_variable(self):
        elsewhere = self.temp / "elsewhere"
        (elsewhere / ".claude").mkdir(parents=True)
        self.write(elsewhere / ".claude.json", "{}")
        self.env["CLAUDE_CONFIG_DIR"] = str(elsewhere)

        self.agents()

        self.assertIn("malak", json.loads((elsewhere / ".claude.json").read_text())["mcpServers"])


if __name__ == "__main__":
    unittest.main()
