"""Export a checked build tag or private main to a local public repository."""

import argparse
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile
import tempfile

from process_cleanup import cleanup_on_interrupt, command_process
from release_identity import identity_environment, release_identity


class ReleaseError(Exception):
    pass


VERSION = re.compile(r"v[0-9]+\.[0-9]+\.[0-9]+")
EMAIL = re.compile(rb"[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9-]+(?:\.[a-z0-9-]+)*\.[a-z]{2,}", re.I)
SIGNATURES = {
    "private specification reference": re.compile(
        rb"""
        (?i:\bspec\.md\b)
        |(?<![\w.])[A-Z]\.[0-9]+(?:\.[0-9]+)*\b
        |(?:\xc2\xa7)+[ \t]*[0-9]+(?:\.[0-9]+)*\b
        |(?i:\b(?:spec(?:ification)?(?:\s+sections?)?|section))\s+`?[0-9]+(?:\.[0-9]+)*\b
        |(?i:\bsections?)\s+`?[A-Z](?![\w/])
        |(?i:\bsections)\s+`?[0-9]+(?:\.[0-9]+)+\b
        |(?i:\bdecisions?)\s+\#?[ \t]*[0-9]+\b(?!\.[.0-9])
        """,
        re.X,
    ),
    "private key": re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----"),
    "secret": re.compile(
        rb"\b(?:(?:AKIA|ASIA)[A-Z0-9]{16}|gh[pousr]_[A-Za-z0-9_]{20,}"
        rb"|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}"
        rb"|sk_(?:live|test)_[A-Za-z0-9]{12,}|sk-[A-Za-z0-9_-]{20,}"
        rb"|AIza[A-Za-z0-9_-]{35})\b"
        rb"""|\b(?:[a-z0-9_]*[_-])?(?:token|secret|password|api[_-]?key|secret[_-]?key|access[_-]?key)["']?"""
        rb"""\s*[:=]\s*["']?[a-z0-9+/=_-]{8,}(?=["'\r\n# \t]|$)""",
        re.I,
    ),
    "absolute home path": re.compile(rb"(?:/(?:Users|home|root)/|[A-Z]:[\\/]Users[\\/])", re.I),
    "handover file": re.compile(
        rb"(?:\A\x89\x01\x01[\x01\x02][\x60-\x7b](?:.{0,3}?)(?:unix:|vsock:)"
        rb"|\A\x84\x01[\x60-\x7b](?:.{0,3}?)unix:)"
        rb".*\x58\x20.{32}\Z",
        re.S,
    ),
}
COAUTHORS = (
    ("Claude Opus 5.5", "anthropic.com"),
    ("Codex Sol 6.2", "openai.com"),
)


def git(repo, *args, env, **kwargs):
    return subprocess.run(
        ["git", "-C", repo, "-c", "core.hooksPath=/dev/null", *args],
        env=env,
        check=True,
        **kwargs,
    )


def git_output(repo, *args, env, **kwargs):
    return git(repo, *args, env=env, stdout=subprocess.PIPE, **kwargs).stdout


def resolve(repo, ref, env):
    try:
        return git_output(repo, "rev-parse", "--verify", "--quiet", ref, env=env).strip()
    except subprocess.CalledProcessError as error:
        if error.returncode == 1:
            return None
        raise


def run_check(checkout, description, env, log=None):
    try:
        command = [checkout / "scripts/check"]
        capture = {} if log is None else {"stdout": subprocess.PIPE, "stderr": subprocess.STDOUT}
        with command_process(command, cwd=checkout, env=env, **capture) as check:
            if log is not None:
                for line in check.stdout:
                    log.write(line)
                    sys.stdout.buffer.write(line)
                    sys.stdout.buffer.flush()
            if check.wait():
                raise subprocess.CalledProcessError(check.returncode, command)
            if log is not None:
                log.seek(0)
    except (OSError, subprocess.CalledProcessError) as error:
        raise ReleaseError(f"scripts/check failed at {description}") from error


@contextmanager
def checked_worktree(repo, checkout, commit, env):
    checkout = checkout.resolve()
    try:
        git(repo, "worktree", "add", "--quiet", "--detach", checkout, commit, env=env)
        yield checkout
    finally:
        # An interrupted add can leave a registration before checkout completes.
        worktrees = git_output(repo, "worktree", "list", "--porcelain", "-z", env=env)
        if b"worktree " + os.fsencode(checkout) in worktrees.split(b"\0"):
            # Git can leave its own initializing lock on the registered worktree.
            git(repo, "worktree", "remove", "--force", "--force", checkout, env=env)


def run_tag_check(build, commit, scratch, env):
    with checked_worktree(build, scratch / "check", commit, env) as checkout:
        run_check(checkout, "the build tag", env)


def exclusions(build, commit, env):
    manifest = git_output(build, "show", commit + b":.release-exclude", env=env)
    paths = set()
    for line in os.fsdecode(manifest).splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            path = PurePosixPath(line)
            if path.is_absolute() or ".." in path.parts or not path.parts:
                raise ReleaseError(f"invalid path in .release-exclude: {line!r}")
            paths.add(path)
    return paths


def is_excluded(path, excluded):
    return any(part in excluded for part in (path, *path.parents))


def archived_files(build, commit, scratch, env, excluded):
    with tempfile.TemporaryFile(dir=scratch) as archive:
        git(build, "archive", "--format=tar", commit, env=env, stdout=archive)
        archive.seek(0)
        with tarfile.open(fileobj=archive, mode="r:") as tree:
            for member in tree:
                path = PurePosixPath(member.name)
                if member.isdir() or is_excluded(path, excluded):
                    continue
                if member.issym():
                    yield path, b"120000", os.fsencode(member.linkname)
                elif member.isfile():
                    with tree.extractfile(member) as contents:
                        mode = b"100755" if member.mode & 0o111 else b"100644"
                        yield path, mode, contents.read()
                else:
                    raise ReleaseError(f"unsupported archive entry: {str(path)!r}")


def scan(files, author_email):
    findings = []
    for path, mode, contents in files:
        encoded_path = os.fsencode(path)
        items = (encoded_path, contents)
        names = (path, PurePosixPath(os.fsdecode(contents))) if mode == b"120000" else (path,)
        reasons = {
            name for name, pattern in SIGNATURES.items()
            if any(pattern.search(item) for item in items)
        }
        for name in names:
            basename = name.name.lower()
            if ".malak" in (part.lower() for part in name.parts):
                reasons.add(".malak state")
            if basename == "handover" or re.search(r"\.handover(?:\.|$)", basename):
                reasons.add("handover file")
            if basename in {"owner.credential", "id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"}:
                reasons.add("private key")
        if any(email != author_email for item in items for email in EMAIL.findall(item)):
            reasons.add("email address other than the author")
        if reasons:
            findings.append(f"  {str(path)!r}: {', '.join(sorted(reasons))}")
    if findings:
        raise ReleaseError("release export contains private material:\n" + "\n".join(findings))


def git_path(repo, option, env):
    path = git_output(repo, "rev-parse", "--path-format=absolute", option, env=env)
    return Path(os.fsdecode(path.removesuffix(b"\n"))).resolve()


def existing_release_repo(build, release, env):
    if build == release or build in release.parents or release in build.parents:
        raise ReleaseError("choose a separate release repository")
    if not release.exists():
        return False
    dot_git = release / ".git"
    if dot_git.is_dir() and not dot_git.is_symlink():
        if git_path(build, "--git-common-dir", env) == git_path(release, "--git-common-dir", env):
            raise ReleaseError("the build repository cannot also be the release repository")
        if git_path(release, "--show-toplevel", env) == release:
            return True
    if not release.is_dir() or any(release.iterdir()):
        raise ReleaseError("release path must be a Git repository or an empty directory")
    return False


def release_tree(repo, files, index, env):
    index_env = {**env, "GIT_INDEX_FILE": str(index)}
    index_entries = []
    for path, mode, data in files:
        blob = git_output(repo, "hash-object", "-w", "--stdin", input=data, env=env).strip()
        index_entries.append(mode + b" " + blob + b"\t" + os.fsencode(path) + b"\0")
    git(repo, "update-index", "-z", "--index-info", input=b"".join(index_entries), env=index_env)
    return git_output(repo, "write-tree", env=index_env).strip()


def export_commit(repo, tree, parent, message, identity, env):
    parents = ("-p", parent) if parent else ()
    return git_output(repo, "commit-tree", "--no-gpg-sign", tree, *parents,
                      input=message.encode(), env=identity_environment(env, identity)).strip()


def require_clean(repo, env):
    if git_output(repo, "--no-optional-locks", "status", "--porcelain", "--untracked-files=all",
                  "--ignored", env=env):
        raise ReleaseError("release repository has local changes")


def publish(repo, tag, files, scratch, identity, env):
    # Serialize publishers before taking the parent and updating its checkout.
    with (repo / ".git" / "malak-release.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        ref = f"refs/tags/{tag}"
        if resolve(repo, ref, env):
            return False
        require_clean(repo, env)
        parent = resolve(repo, "HEAD", env)
        old_tree = resolve(repo, "HEAD^{tree}", env)
        if old_tree is None:
            old_tree = git_output(repo, "mktree", input=b"", env=env).strip()
        tree = release_tree(repo, files, scratch / "index", env)
        trailers = [f"Co-authored-by: {person} <noreply@{domain}>" for person, domain in COAUTHORS]
        message = "\n".join((f"Malak {tag}", "", *trailers, ""))
        commit = export_commit(repo, tree, parent, message, identity, env)
        previous = parent or b"0" * len(commit)
        updates = b"update HEAD %s %s\ncreate %s %s\n" % (commit, previous, ref.encode(), commit)
        git(repo, "read-tree", "-m", "-u", old_tree, tree, env=env)
        try:
            git(repo, "update-ref", "--stdin", input=updates, env=env)
        except subprocess.CalledProcessError:
            git(repo, "read-tree", "-m", "-u", tree, old_tree, env=env)
            raise
    return True


def incremental_base(repo, branch, env):
    require_clean(repo, env)
    if resolve(repo, f"refs/heads/{branch}", env):
        raise ReleaseError(f"release branch {branch!r} already exists")
    main = resolve(repo, "refs/heads/main^{commit}", env)
    origin_main = resolve(repo, "refs/remotes/origin/main^{commit}", env)
    if main is None or main != origin_main:
        local = os.fsdecode(main or b"missing")
        upstream = os.fsdecode(origin_main or b"missing")
        raise ReleaseError(
            f"main ({local}) must match origin/main ({upstream}). "
            "Fetch origin and fast-forward local main before retrying."
        )
    return main


def check_branch(repo, branch, env):
    try:
        validated = git_output(repo, "check-ref-format", "--branch", branch,
                               env=env, stderr=subprocess.DEVNULL)
    except subprocess.CalledProcessError:
        validated = None
    if validated != os.fsencode(branch) + b"\n":
        raise ReleaseError(f"invalid release branch name: {branch!r}")


def check_export(checkout, message, files, scratch, identity, env, log):
    tree = release_tree(checkout, files, scratch / "check-index", env)
    commit = export_commit(checkout, tree, None, message, identity, env)
    git(checkout, "switch", "--quiet", "--detach", commit, env=env)
    run_check(checkout, "the clean release export", env, log)


def lane_files(build, parent, public, base, files, excluded, checkout, scratch, identity, env):
    parent_files = archived_files(build, parent, scratch, env, exclusions(build, parent, env))
    parent_tree = release_tree(checkout, parent_files, scratch / "parent-index", env)
    parent_commit = export_commit(checkout, parent_tree, None, "Lane parent\n", identity, env)
    lane_tree = release_tree(checkout, files, scratch / "lane-index", env)
    lane_commit = export_commit(checkout, lane_tree, parent_commit, "Lane change\n", identity, env)

    git(checkout, "fetch", "--quiet", "--no-tags", "--no-recurse-submodules", public, base, env=env)
    base_tree = resolve(checkout, base + b"^{tree}", env)
    public_commit = export_commit(checkout, base_tree, parent_commit, "Public main\n", identity, env)
    try:
        merged = git_output(checkout, "merge-tree", "--write-tree", public_commit, lane_commit, env=env)
    except subprocess.CalledProcessError as error:
        if error.returncode != 1:
            raise
        raise ReleaseError(
            "the lane conflicts with public main\n"
            "  Next    Sync any prerequisite lane and refresh the public clone, "
            "or export the pending changes together."
        ) from error
    tree = merged.splitlines()[0]
    if tree == base_tree:
        raise ReleaseError("the lane has no changes to export on public main")
    return list(archived_files(checkout, tree, scratch, env, excluded))


def publish_incremental(repo, branch, base, message, files, scratch, identity, env):
    with (repo / ".git" / "malak-release.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if incremental_base(repo, branch, env) != base:
            raise ReleaseError("main changed while checking the release export; retry the export")
        tree = release_tree(repo, files, scratch / "index", env)
        commit = export_commit(repo, tree, base, message, identity, env)
        git(repo, "switch", "--quiet", "--create", branch, commit, env=env)


def export_release(tag, release, branch, fixes, allow_email, *, source_commit=None, lane_parent=None,
                   settings_file=None, check_log=None, public_base=None):
    # Inherited Git repository and index overrides must never redirect a command
    # from the build or release repository named by this script.
    local_vars = subprocess.check_output(["git", "rev-parse", "--local-env-vars"], text=True)
    overrides = set(local_vars.splitlines())
    env = {key: value for key, value in os.environ.items() if key not in overrides}
    env["GIT_NO_REPLACE_OBJECTS"] = "1"
    build = git_path(Path(__file__).resolve().parents[1], "--show-toplevel", env)
    source = source_commit or (f"refs/tags/{tag}" if tag is not None else "refs/heads/main")
    commit = resolve(build, f"{source}^{{commit}}", env)
    if commit is None:
        if tag is None:
            raise ReleaseError("private main does not exist or does not point to a commit")
        raise ReleaseError(f"build tag {tag} does not exist or does not point to a commit")
    release = release.expanduser().resolve()
    existing = existing_release_repo(build, release, env)
    if branch is not None:
        if not existing:
            raise ReleaseError("--onto must name an existing Git clone")
        check_branch(release, branch, env)
        base = incremental_base(release, branch, env)
        if public_base is not None and base != os.fsencode(public_base):
            raise ReleaseError("public main changed while preparing the pull request; retry the export")
    elif existing and resolve(release, f"refs/tags/{tag}", env):
        print(f"{tag} is already in the release repository.")
        return
    settings_file = settings_file or Path(
        env.get("MALAK_INTERNAL_SETTINGS") or build / "internal/settings.toml").expanduser()
    identity = release_identity(release if existing else None, settings_file, env, allow_email)
    with cleanup_on_interrupt(), tempfile.TemporaryDirectory(prefix="malak-release-") as temp:
        scratch = Path(temp)
        if branch is None:
            run_tag_check(build, commit, scratch, env)
        excluded = exclusions(build, commit, env)
        files = list(archived_files(build, commit, scratch, env, excluded))
        if branch is not None:
            title = f"Malak {tag}" if tag is not None else "Sync main"
            message = "\n".join((title, "", *(f"Fixes #{issue}" for issue in fixes), ""))
            checkout = scratch / "check"
            git(scratch, "init", "--quiet", checkout, env=env)
            if lane_parent is not None:
                files = lane_files(build, os.fsencode(lane_parent), release, base, files,
                                   excluded, checkout, scratch, identity, env)
            scan(files, os.fsencode(identity[1]))
            check_export(checkout, message, files, scratch, identity, env, check_log)
            publish_incremental(release, branch, base, message, files, scratch, identity, env)
            print(f"Created {title} on {branch} in {release}.")
            return
        scan(files, os.fsencode(identity[1]))
        if not existing:
            git(build, "init", "--quiet", release, env=env)
        if publish(release, tag, files, scratch, identity, env):
            print(f"Created Malak {tag} in {release}.")
        else:
            print(f"{tag} is already in the release repository.")


def issue_number(text):
    number = text.lstrip("0")
    if not re.fullmatch(r"[0-9]+", text) or not number:
        raise argparse.ArgumentTypeError("issue number must be a positive integer")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("tag", nargs="?", help="build tag in vX.Y.Z form")
    parser.add_argument("--sync", action="store_true", help="export private main instead of a tag")
    destination = parser.add_mutually_exclusive_group()
    destination.add_argument("--release-repo", type=Path,
                             default=Path.home() / "Projects/malak-release",
                             help="repository for the local tagged export")
    destination.add_argument("--onto", type=Path, help="public clone for an incremental export")
    parser.add_argument("--branch", help="new branch in the public clone")
    parser.add_argument("--fixes", action="append", type=issue_number, metavar="ISSUE",
                        help="positive issue number to close, repeatable")
    parser.add_argument("--allow-email", action="store_true", help="allow an email outside GitHub's no-reply domain")
    args = parser.parse_args()
    if args.sync:
        if args.tag is not None:
            parser.error("--sync does not take a build tag")
        if args.onto is None:
            parser.error("--sync requires --onto")
    elif args.tag is None or not VERSION.fullmatch(args.tag):
        parser.error("tag must be in vX.Y.Z form")
    if args.onto is not None:
        if args.branch is None or not args.fixes:
            parser.error("--onto requires --branch and at least one --fixes issue number")
    elif args.branch is not None or args.fixes:
        parser.error("--branch and --fixes require --onto")
    try:
        export_release(args.tag, args.onto or args.release_repo, args.branch, args.fixes, args.allow_email)
    except (ReleaseError, OSError, subprocess.CalledProcessError, tarfile.TarError, ValueError) as error:
        sys.exit(f"release: {error}")


if __name__ == "__main__":
    main()
