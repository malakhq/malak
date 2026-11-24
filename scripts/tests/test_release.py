"""Exercise the release command with isolated build and release repositories."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
AUTHOR = "Malak Maintainer"
AUTHOR_EMAIL = "owner@" + "example.com"
TAG = "v1.2.3"
BRANCH = "release/v1.2.3"
# Assemble planted references at runtime because this test file ships.
PRIVATE_SPEC = "SPEC" + ".md"


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(tempfile.TemporaryDirectory()))
        home = scratch / "home"
        home.mkdir()
        self.build = scratch / "build"
        self.destination = scratch / "release repo"
        self.env = {**os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(home / "config"),
                    "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1",
                    "GIT_ALLOW_PROTOCOL": "file"}
        for role in ("AUTHOR", "COMMITTER"):
            for field in ("NAME", "EMAIL", "DATE"):
                self.env.pop(f"GIT_{role}_{field}", None)
        self.build.mkdir()
        self.git(self.build, "init", "-q", "--initial-branch=main")
        self.git(self.build, "config", "user.name", AUTHOR)
        self.git(self.build, "config", "user.email", AUTHOR_EMAIL)
        (self.build / "scripts").mkdir()
        shutil.copy2(ROOT / "scripts/release", self.build / "scripts/release")
        shutil.copy2(ROOT / ".release-exclude", self.build / ".release-exclude")
        self.write("README", "tagged contents\n")
        self.write_check("exit 0")

    def git(self, repo, *args):
        return subprocess.check_output(["git", "-C", repo, *args], env=self.env)

    def write(self, path, contents):
        target = self.build / path
        target.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(contents, bytes):
            target.write_bytes(contents)
        else:
            target.write_text(contents)

    def write_check(self, shell_code):
        self.write("scripts/check", f"#!/bin/sh\n{shell_code}\n")
        (self.build / "scripts/check").chmod(0o755)

    def commit(self, message):
        self.git(self.build, "add", "--all")
        self.git(self.build, "commit", "-qm", message)

    def tag(self, version=TAG):
        self.commit(f"Build {version}")
        self.git(self.build, "tag", version)

    def release(self, version=TAG, success=True):
        return self.run_release(version, "--release-repo", self.destination, success=success)

    def run_release(self, *args, success=True):
        result = subprocess.run(
            [self.build / "scripts/release", *args],
            cwd=self.build,
            env=self.env,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def clone_public(self):
        public = self.build.parent / "public"
        public.mkdir()
        self.git(public, "init", "-q", "--initial-branch=main")
        self.git(public, "config", "user.name", AUTHOR)
        self.git(public, "config", "user.email", AUTHOR_EMAIL)
        (public / "obsolete.txt").write_text("old public file\n")
        self.git(public, "add", "obsolete.txt")
        self.git(public, "commit", "-qm", "Previous public release")
        self.git(public, "clone", "--quiet", public, self.destination)
        self.git(self.destination, "config", "user.name", AUTHOR)
        self.git(self.destination, "config", "user.email", AUTHOR_EMAIL)
        return self.git(self.destination, "rev-parse", "main")

    def incremental(self, *fixes, success=True):
        return self.run_release(
            TAG, "--onto", self.destination, "--branch", BRANCH, *fixes, success=success,
        )

    def sync(self, *fixes, success=True):
        return self.run_release(
            "--sync", "--onto", self.destination, "--branch", "sync/test", *fixes, success=success,
        )

    def test_exports_the_tag_instead_of_the_build_worktree(self):
        self.tag()
        self.write("README", "uncommitted work\n")
        self.write_check("exit 1")
        self.release()
        self.assertEqual(self.git(self.destination, "show", f"{TAG}:README"), b"tagged contents\n")

    def test_missing_tag_leaves_the_destination_absent(self):
        self.tag()
        result = self.release("v9.9.9", success=False)
        self.assertIn("does not exist", result.stderr)
        self.assertFalse(self.destination.exists())

    def test_failed_tag_check_leaves_the_destination_absent(self):
        self.write_check("exit 42")
        self.tag()
        self.write_check("exit 0")
        result = self.release(success=False)
        self.assertIn("scripts/check failed at the build tag", result.stderr)
        self.assertFalse(self.destination.exists())

    def test_detects_a_planted_secret_without_printing_its_value(self):
        secret = "ghp_" + "A" * 36
        self.write("plain.txt", secret)
        self.tag()
        result = self.release(success=False)
        self.assertIn("'plain.txt': secret", result.stderr)
        self.assertNotIn(secret, result.stderr)
        self.assertFalse(self.destination.exists())

    def test_detects_a_secret_in_a_configuration_assignment(self):
        self.write("settings.toml", "secret_key = " + '"paper-lamp-42"\n')
        self.tag()
        result = self.release(success=False)
        self.assertIn("'settings.toml': secret", result.stderr)

    def test_detects_a_planted_absolute_home_path(self):
        self.write("plain.txt", "/home" + "/malak-example/private")
        self.tag()
        result = self.release(success=False)
        self.assertIn("'plain.txt': absolute home path", result.stderr)
        self.assertFalse(self.destination.exists())

    def test_detects_a_private_key(self):
        self.write("plain.txt", "-----BEGIN OPENSSH " + "PRIVATE KEY-----\n")
        self.tag()
        result = self.release(success=False)
        self.assertIn("'plain.txt': private key", result.stderr)

    def test_detects_a_handover_even_if_renamed(self):
        address = b"unix:" + b"/tmp/malak/p"
        self.write(
            "transfer",
            b"\x89\x01\x01\x01" + bytes([0x60 + len(address)]) + address
            + b"\x58\x20" + b"B" * 32 + b"\x64lead\x60\x40\x58\x20" + b"K" * 32,
        )
        self.tag()
        result = self.release(success=False)
        self.assertIn("'transfer': handover file", result.stderr)

    def test_detects_private_state(self):
        self.write(".malak/board.toml", "state")
        self.tag()
        result = self.release(success=False)
        self.assertIn("'.malak/board.toml': .malak state", result.stderr)

    def test_rejects_the_private_spec_filename(self):
        self.write("README", f"See [the specification]({PRIVATE_SPEC}).\n")
        self.tag()
        result = self.release(success=False)
        self.assertIn("'README': private specification reference", result.stderr)
        self.assertFalse(self.destination.exists())

    def test_rejects_private_section_references_in_shipped_files(self):
        references = {
            "architecture.md": "The store contract follows D" + ".6.",
            "acceptance.md": "See §" + "5.6 for limits.",
            "cli.rs": "// As described in SPEC " + "8.4.",
            "types.rs": "//! This follows section " + "Z.",
            "overview.md": "The reviewed sections " + "A–F cover the core.",
            "limits.md": "See section\n" + "8.4 for defaults.",
        }
        for name, contents in references.items():
            self.write(name, contents)
        self.tag()
        result = self.release(success=False)
        for name in references:
            self.assertIn(f"{name!r}: private specification reference", result.stderr)

    def test_rejects_numbered_owner_decisions(self):
        self.write("README", "This follows owner decision #" + "75.\n")
        self.tag()
        result = self.release(success=False)
        self.assertIn("'README': private specification reference", result.stderr)

    def test_allows_versions_and_measurements(self):
        self.write("README", "Python 3.11, Malak v0.1.0 and a median of 8.4 µs.\n")
        self.tag()
        self.release()

    def test_allows_a_truncated_participant_id_in_a_decision_row(self):
        self.write("mailbox.txt", "│ Decision  01234567... #1\n")
        self.tag()
        self.release()

    def test_lists_each_file_with_a_disallowed_email(self):
        for filename in ("first.txt", "second.txt"):
            self.write(filename, "someone@" + "example.net")
        self.tag()
        result = self.release(success=False)
        for filename in ("first.txt", "second.txt"):
            self.assertIn(f"{filename!r}: email address other than the author", result.stderr)

    def test_allows_the_author_email(self):
        self.write("README", f"Maintainer <{AUTHOR_EMAIL}>\n")
        self.tag()
        self.release()
        self.assertEqual(
            self.git(self.destination, "show", f"{TAG}:README"),
            f"Maintainer <{AUTHOR_EMAIL}>\n".encode(),
        )

    def test_excludes_manifest_paths_from_the_export_and_scan(self):
        self.write("docs/field-notes/nested/note.md", "/Users" + "/malak-example/private")
        self.write("docs/FIELD-NOTES.md", "internal index")
        self.write("docs/field-notes-public.md", "public file")
        self.tag()
        self.release()
        paths = self.git(self.destination, "ls-tree", "-r", "--name-only", TAG).splitlines()
        self.assertNotIn(b"docs/FIELD-NOTES.md", paths)
        self.assertFalse(any(path.startswith(b"docs/field-notes/") for path in paths))
        self.assertIn(b"docs/field-notes-public.md", paths)

    def test_excludes_internal_planning_docs_from_the_export_and_scan(self):
        internal_docs = ("docs/M2-DESIGN.md", "docs/OPEN-QUESTIONS.md")
        for path in internal_docs:
            self.write(path, f"See {PRIVATE_SPEC}.")
        self.tag()
        self.release()
        paths = self.git(self.destination, "ls-tree", "-r", "--name-only", TAG).splitlines()
        for path in internal_docs:
            self.assertNotIn(path.encode(), paths)

    def test_preserves_executables_and_symbolic_links(self):
        (self.build / "recipe-link").symlink_to("scripts/check")
        self.tag()
        self.release()
        self.assertTrue(os.access(self.destination / "scripts/check", os.X_OK))
        self.assertEqual((self.destination / "recipe-link").readlink(), Path("scripts/check"))

    def test_uses_the_build_author_and_exact_coauthor_trailers(self):
        self.tag()
        self.release()
        expected = (
            f"{AUTHOR}\n{AUTHOR_EMAIL}\nMalak {TAG}\n\n"
            "Co-authored-by: Claude Opus 5.5 <noreply@" + "anthropic.com>\n"
            "Co-authored-by: Codex Sol 6.2 <noreply@" + "openai.com>\n"
        )
        actual = self.git(self.destination, "show", "-s", "--format=%an%n%ae%n%B", TAG)
        self.assertEqual(actual, (expected + "\n").encode())

    def test_creates_no_remote(self):
        self.tag()
        self.release()
        self.assertEqual(self.git(self.destination, "remote"), b"")

    def test_second_run_keeps_the_same_head_and_tag_without_running_checks(self):
        self.env["CHECK_MARKER"] = str(self.build.parent / "check-marker")
        self.write_check('test ! -e "$CHECK_MARKER" && touch "$CHECK_MARKER"')
        self.tag()
        self.release()
        refs = self.git(self.destination, "rev-parse", "HEAD", TAG)
        result = self.release()
        self.assertIn("already in the release repository", result.stdout)
        self.assertEqual(self.git(self.destination, "rev-parse", "HEAD", TAG), refs)
        self.assertEqual(self.git(self.destination, "rev-list", "--count", "HEAD"), b"1\n")

    def test_next_release_replaces_the_old_tree_in_one_commit(self):
        self.write("old.txt", "remove for the next release")
        self.tag()
        self.release()
        (self.build / "old.txt").unlink()
        self.write("new.txt", "new contents")
        self.tag("v1.2.4")
        self.release("v1.2.4")
        self.assertFalse((self.destination / "old.txt").exists())
        self.assertEqual((self.destination / "new.txt").read_text(), "new contents")
        parent, first_release = self.git(self.destination, "rev-parse", "HEAD^", TAG).splitlines()
        self.assertEqual(parent, first_release)
        self.assertEqual(self.git(self.destination, "status", "--porcelain"), b"")

    def test_refuses_to_replace_untracked_destination_files(self):
        self.destination.mkdir()
        self.git(self.destination, "init", "-q")
        (self.destination / "README").write_text("local work")
        self.tag()
        result = self.release(success=False)
        self.assertIn("has local changes", result.stderr)
        self.assertEqual((self.destination / "README").read_text(), "local work")

    def test_incremental_export_uses_main_as_parent_from_another_branch(self):
        main = self.clone_public()
        self.git(self.destination, "switch", "-qc", "other")
        self.git(self.destination, "commit", "-qm", "Other branch commit", "--allow-empty")
        self.tag()

        self.incremental("--fixes", "1")

        self.assertEqual(self.git(self.destination, "rev-parse", "HEAD^"), main)

    def test_incremental_export_checks_out_one_new_commit(self):
        self.clone_public()
        self.tag()

        self.incremental("--fixes", "1")

        self.assertEqual(self.git(self.destination, "branch", "--show-current").strip().decode(), BRANCH)
        self.assertEqual(self.git(self.destination, "rev-list", "--count", "main..HEAD"), b"1\n")

    def test_incremental_export_does_not_create_a_tag(self):
        self.clone_public()
        self.tag()

        self.incremental("--fixes", "1")

        self.assertEqual(self.git(self.destination, "tag", "--list"), b"")

    def test_incremental_export_writes_one_fix_line_per_issue(self):
        self.clone_public()
        self.tag()

        self.incremental("--fixes", "1", "--fixes", "6")

        message = self.git(self.destination, "show", "-s", "--format=%B", "HEAD")
        self.assertEqual(message, f"Malak {TAG}\n\nFixes #1\nFixes #6\n\n".encode())

    def test_incremental_export_replaces_the_tree_without_excluded_files(self):
        self.clone_public()
        self.write("docs/field-notes/internal.md", f"See {PRIVATE_SPEC}.")
        self.tag()

        self.incremental("--fixes", "1")

        paths = self.git(self.destination, "ls-tree", "-r", "--name-only", "HEAD").splitlines()
        self.assertEqual(paths, [b".release-exclude", b"README", b"scripts/check", b"scripts/release"])

    def test_incremental_export_runs_checks_in_the_clean_filtered_tree(self):
        self.clone_public()
        self.write("docs/field-notes/internal.md", "excluded file\n")
        self.env["CHECK_MARKER"] = str(self.build.parent / "check-marker")
        self.write_check(
            'set -eu\n'
            'test ! -e docs/field-notes/internal.md\n'
            'test ! -e obsolete.txt\n'
            'test -z "$(git status --porcelain)"\n'
            'touch "$CHECK_MARKER"',
        )
        self.tag()

        self.incremental("--fixes", "1")

        self.assertTrue(Path(self.env["CHECK_MARKER"]).is_file())

    def test_incremental_export_leaves_refs_unchanged_when_checks_fail(self):
        self.clone_public()
        refs = self.git(self.destination, "show-ref")
        self.write_check("exit 1")
        self.tag()

        result = self.incremental("--fixes", "1", success=False)

        self.assertIn("scripts/check failed at the clean release export", result.stderr)
        self.assertEqual(self.git(self.destination, "show-ref"), refs)

    def test_incremental_export_refuses_private_material(self):
        self.clone_public()
        refs = self.git(self.destination, "show-ref")
        self.write("README", f"See {PRIVATE_SPEC}.")
        self.tag()

        result = self.incremental("--fixes", "1", success=False)

        self.assertIn("'README': private specification reference", result.stderr)
        self.assertEqual(self.git(self.destination, "show-ref"), refs)

    def test_incremental_export_refuses_a_dirty_target(self):
        self.clone_public()
        local = self.destination / "obsolete.txt"
        local.write_text("local edits\n")
        self.tag()

        result = self.incremental("--fixes", "1", success=False)

        self.assertIn("has local changes", result.stderr)
        self.assertEqual(local.read_text(), "local edits\n")

    def test_incremental_export_refuses_main_that_differs_from_origin(self):
        upstream = self.clone_public().strip().decode()
        self.git(self.destination, "commit", "-qm", "Local main commit", "--allow-empty")
        local = self.git(self.destination, "rev-parse", "main").strip().decode()
        self.tag()

        result = self.incremental("--fixes", "1", success=False)

        self.assertIn(f"main ({local}) must match origin/main ({upstream})", result.stderr)

    def test_incremental_export_requires_an_issue_number(self):
        self.clone_public()
        self.tag()

        result = self.incremental(success=False)

        self.assertIn("at least one --fixes issue number", result.stderr)

    def test_incremental_export_refuses_a_nonpositive_issue_number(self):
        self.clone_public()
        self.tag()

        result = self.incremental("--fixes", "0", success=False)

        self.assertIn("issue number must be a positive integer", result.stderr)

    def test_sync_exports_private_main_from_another_checkout(self):
        self.clone_public()
        self.write("README", "private main contents\n")
        self.commit("Private main")
        self.git(self.build, "switch", "-qc", "other")
        self.write("README", "other branch contents\n")
        self.commit("Other branch")

        self.sync("--fixes", "1")

        self.assertEqual((self.destination / "README").read_text(), "private main contents\n")

    def test_sync_creates_one_commit_on_public_main(self):
        main = self.clone_public()
        self.commit("Private main")

        self.sync("--fixes", "1")

        self.assertEqual(self.git(self.destination, "rev-parse", "sync/test^"), main)

    def test_sync_uses_the_sync_title_and_fix_lines(self):
        self.clone_public()
        self.commit("Private main")

        self.sync("--fixes", "1", "--fixes", "6")

        self.assertEqual(self.git(self.destination, "show", "-s", "--format=%B", "HEAD"),
                         b"Sync main\n\nFixes #1\nFixes #6\n\n")

    def test_sync_needs_no_build_tag_and_does_not_create_a_release_tag(self):
        self.clone_public()
        self.commit("Private main")

        self.sync("--fixes", "1")

        self.assertEqual(self.git(self.destination, "tag", "--list"), b"")

    def test_sync_refuses_a_build_tag_argument(self):
        result = self.run_release(TAG, "--sync", success=False)

        self.assertIn("--sync does not take a build tag", result.stderr)


if __name__ == "__main__":
    unittest.main()
