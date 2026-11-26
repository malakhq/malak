"""Merge and remove Malak's entries in native agent settings."""

import base64
import json
import os
from pathlib import Path
import stat
import tempfile
import tomllib


def read_file(path, *, private=False):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    with os.fdopen(fd, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid():
            raise ValueError(f"{path} must be a regular file owned by this user")
        if private and stat.S_IMODE(metadata.st_mode) != 0o600:
            raise ValueError(f"{path} must have mode 0600")
        return source.read()


def directory_exists(path):
    try:
        metadata = path.lstat()
    except FileNotFoundError:
        return False
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid():
        raise ValueError(f"{path} must be a directory owned by this user")
    if metadata.st_mode & 0o022:
        raise ValueError(f"{path} must be writable only by this user")
    return True


def replace_file(path, previous, updated):
    if previous == updated:
        return
    mode = 0o600 if previous is None else stat.S_IMODE(path.lstat().st_mode)
    temporary = None
    try:
        if updated is not None:
            with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as staged:
                temporary = Path(staged.name)
                staged.write(updated)
                os.fchmod(staged.fileno(), mode)
        if read_file(path) != previous:
            raise ValueError(f"{path} changed during the update; retry the command")
        if temporary is None:
            path.unlink()
        else:
            temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def encode(data):
    return base64.b64encode(data).decode() if data is not None else None


def decode(data):
    return base64.b64decode(data, validate=True) if data is not None else None


def parse_settings(path, original):
    doc = json.loads(original) if original and original.strip() else {}
    if not isinstance(doc, dict) or not isinstance(doc.get("hooks", {}), dict):
        raise ValueError(f"{path} must contain a JSON object with an optional hooks object")
    return doc


def render_settings(doc, original):
    # Keep the indentation and line ending of pretty-printed files.
    lines = (original or b"").splitlines()
    indent = next((line[:len(line) - len(line.lstrip())].decode()
                   for line in lines if line.lstrip().startswith(b'"')), "  ")
    data = (json.dumps(doc, indent=indent or None, ensure_ascii=False, allow_nan=False) + "\n").encode()
    return data.replace(b"\n", b"\r\n") if b"\r\n" in (original or b"") else data


def event_groups(path, hooks, event):
    groups = hooks.setdefault(event, [])
    if not isinstance(groups, list):
        raise ValueError(f"{path}: hooks.{event} must be an array")
    return groups


def plan_settings(path, registrations):
    return settings_additions(path, read_file(path), registrations)


def settings_additions(path, before, registrations):
    doc = parse_settings(path, before)
    hooks = doc.setdefault("hooks", {})
    added = {}
    for event, group in registrations.items():
        groups = event_groups(path, hooks, event)
        if group not in groups:
            groups.append(group)
            added[event] = group
    if not added:
        return None
    return {
        "path": str(path),
        "before": encode(before),
        "after": encode(render_settings(doc, before)),
        "added": added,
    }


def remove_owned_group(path, groups, group, original_groups):
    exact = [i for i, value in enumerate(groups) if value == group]
    if len(exact) == 1:
        del groups[exact[0]]
        return True
    handler = group["hooks"][0]
    # If the owner added handlers to our group, remove only our handler.
    # Identical copies are ambiguous, so leave the file available for review.
    candidates = [
        (value, handlers)
        for value in groups if isinstance(value, dict)
        and value.get("matcher") == group.get("matcher")
        and isinstance(handlers := value.get("hooks"), list)
        and handler in handlers
    ]
    original_count = sum(
        handler in value.get("hooks", [])
        for value in original_groups if isinstance(value, dict)
        and isinstance(value.get("hooks"), list)
        and value.get("matcher") == group.get("matcher")
    )
    if len(candidates) <= original_count:
        return False
    if len(candidates) != 1 or candidates[0][1].count(handler) != 1:
        raise ValueError(f"{path} contains multiple matching Malak hooks; leave one or remove them and retry")
    value, handlers = candidates[0]
    handlers.remove(handler)
    if not handlers and value.keys() == group.keys():
        groups.remove(value)
    return True


def plan_settings_removal(change):
    path = Path(change["path"])
    original = read_file(path)
    before, after = decode(change["before"]), decode(change["after"])
    if original is None or original == before:
        return None
    if original == after:
        return path, original, before, list(change["added"])
    doc = parse_settings(path, original)
    hooks = doc.get("hooks", {})
    baseline = parse_settings(path, before).get("hooks", {})
    removed = []
    for event, group in change["added"].items():
        if event not in hooks:
            continue
        groups = event_groups(path, hooks, event)
        if remove_owned_group(path, groups, group, baseline.get(event, [])):
            removed.append(event)
            if not groups and event not in baseline:
                del hooks[event]
    if not removed:
        return None
    if not hooks and "hooks" not in parse_settings(path, before):
        del doc["hooks"]
    updated = render_settings(doc, original) if doc or before is not None else None
    return path, original, updated, removed


# The MCP server Malak registers, next to the hooks. A tool's adapter names the
# file and picks the codec for its format. A codec never creates the user's file
# and removes only the entry it wrote.
MCP_NAME = "malak"


class JsonServers:
    """MCP servers kept as an object of entries under one key of a JSON file."""

    def __init__(self, key):
        self.key = key

    def entry(self, executable):
        return {"type": "stdio", "command": executable, "args": ["mcp"], "env": {}}

    def plan(self, path, executable):
        original = read_file(path)
        if original is None:
            return None
        doc = parse_object(path, original)
        servers = doc.setdefault(self.key, {})
        if not isinstance(servers, dict):
            raise ValueError(f"{path}: {self.key} must be an object")
        if MCP_NAME in servers:
            return {"path": str(path), "kept": True}
        created = self.key not in parse_object(path, original)
        entry = self.entry(executable)
        servers[MCP_NAME] = entry
        return {
            "path": str(path), "before": encode(original), "after": encode(render_settings(doc, original)),
            "entry": entry, "created": created,
        }

    def removal(self, change):
        path = Path(change["path"])
        original = read_file(path)
        if original is None:
            return None
        doc = parse_object(path, original)
        servers = doc.get(self.key)
        if not isinstance(servers, dict) or servers.get(MCP_NAME) != change["entry"]:
            return None
        del servers[MCP_NAME]
        if not servers and change["created"]:
            del doc[self.key]
        return path, original, render_settings(doc, original)


class TomlServers:
    """MCP servers kept as `[table.NAME]` sections of a TOML file, in a marked block."""

    BEGIN = "# BEGIN malak mcp (added by malak recipe agents)"
    END = "# END malak mcp"

    def __init__(self, table):
        self.table = table

    def block(self, executable):
        return "\n".join([
            self.BEGIN, f"[{self.table}.{MCP_NAME}]", f"command = {json.dumps(executable)}",
            'args = ["mcp"]', self.END,
        ]) + "\n"

    def plan(self, path, executable):
        original = read_file(path)
        if original is None:
            return None
        text = original.decode()
        if MCP_NAME in tomllib.loads(text).get(self.table, {}):
            return {"path": str(path), "kept": True}
        separator = "" if not text else ("\n" if text.endswith("\n") else "\n\n")
        added = separator + self.block(executable)
        return {
            "path": str(path), "before": encode(original), "after": encode((text + added).encode()),
            "added": added,
        }

    def removal(self, change):
        path = Path(change["path"])
        original = read_file(path)
        if original is None:
            return None
        text = original.decode()
        if text.count(change["added"]) != 1:
            return None
        return path, original, text.replace(change["added"], "", 1).encode()


def parse_object(path, original):
    doc = json.loads(original) if original and original.strip() else {}
    if not isinstance(doc, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return doc
