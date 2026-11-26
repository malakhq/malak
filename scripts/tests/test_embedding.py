"""Prove the board is separate from its tooling, and that examples use the public API."""

import os
from pathlib import Path
import re
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import public_api

TOOLING = {"clap", "ratatui", "rusqlite", "libsqlite3-sys", "toml", "serde_json", "crossterm"}
FEATURES = ("markdown", "sqlite", "unix", "key")
# A feature brings in only what its layer needs: the SQLite store brings rusqlite.
OWN = {"sqlite": {"rusqlite", "libsqlite3-sys"}}


def dependencies(*features):
    command = ["cargo", "tree", "--locked", "-p", "malak", "--no-default-features",
               "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
    if features:
        command += ["--features", ",".join(features)]
    output = subprocess.run(command, cwd=ROOT, check=True, capture_output=True, text=True).stdout
    return {line.split()[0] for line in output.splitlines() if line.strip()}


class BoardOnlyTests(unittest.TestCase):
    def test_the_board_only_build_has_no_tooling_dependency(self):
        self.assertEqual(dependencies() & TOOLING, set())

    def test_each_layer_feature_brings_no_tooling_beyond_its_own_layer(self):
        for feature in FEATURES:
            with self.subTest(feature=feature):
                self.assertEqual(dependencies(feature) & TOOLING, OWN.get(feature, set()))

    def test_the_command_line_feature_brings_the_tooling(self):
        self.assertLessEqual({"clap", "ratatui", "rusqlite", "toml"}, dependencies("cli"))


class WithoutPythonTests(unittest.TestCase):
    def run_without_python(self, *command):
        return subprocess.run([str(ROOT / "scripts/without-python"), *command], cwd=ROOT,
                              capture_output=True, text=True)

    def test_the_wrapper_removes_python_from_the_path(self):
        result = self.run_without_python("sh", "-c", "command -v python3 || command -v python")
        self.assertNotEqual(result.returncode, 0)

    def test_the_in_process_example_runs_without_python(self):
        result = self.run_without_python(
            "cargo", "run", "--locked", "-q", "-p", "malak", "--example", "team", "--no-default-features")
        self.assertEqual(result.returncode, 0, result.stderr)


class ExampleTests(unittest.TestCase):
    EXAMPLES = sorted((ROOT / "crates/malak/examples").glob("*.rs"))

    def test_examples_name_no_crate_but_malak(self):
        for example in self.EXAMPLES:
            with self.subTest(example=example.name):
                self.assertEqual(set(re.findall(r"\bmalak_\w+", example.read_text())), set())

    def test_examples_use_no_doc_hidden_item(self):
        hidden = public_api.hidden_names() - public_api.public_names(public_api.SNAPSHOT.read_text())
        self.assertLessEqual({"create_local", "local_entrances"}, hidden)
        for example in self.EXAMPLES:
            with self.subTest(example=example.name):
                used = set(re.findall(r"\b\w+\b", example.read_text()))
                self.assertEqual(used & hidden, set())


if __name__ == "__main__":
    unittest.main()
