"""The owner's organisation roster; no board or participant format changes."""

from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import stat
import tempfile

import board_data as board
from recipe_board import board_entries, board_settings


def folder(config):
    return Path(board_settings(Path(config))["state"]) / "org"


def private_open(path, flags):
    fd = os.open(path, flags | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    metadata = os.fstat(fd)
    if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
        os.close(fd)
        raise board.RecipeError(f"organisation state must be an owner-only file: {path}")
    return os.fdopen(fd, "r+" if flags & os.O_RDWR else "r")


def load(config):
    try:
        with private_open(folder(config) / "roster.json", os.O_RDONLY) as source:
            return json.load(source)
    except FileNotFoundError:
        return None


def save(config, roster, filename="roster.json"):
    root = folder(config)
    with tempfile.NamedTemporaryFile(mode="w", dir=root, delete=False) as target:
        path = Path(target.name)
        try:
            json.dump(roster, target, indent=2)
            target.write("\n")
            target.flush()
            os.fsync(target.fileno())
            os.replace(path, root / filename)
            directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            path.unlink(missing_ok=True)


def ensure_folder(config):
    root = folder(config)
    root.mkdir(mode=0o700, exist_ok=True)
    metadata = root.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o077:
        raise board.RecipeError(f"organisation state must be an owner-only directory: {root}")
    return root


@contextmanager
def locked(config):
    root = ensure_folder(config)
    with private_open(root / "roster.lock", os.O_RDWR | os.O_CREAT) as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield load(config)


def find(config):
    """Return (top config, roster) for this board, or None. Removed boards remain known."""
    config = Path(config).absolute()
    candidates = [config, *(path.absolute() for _, path in board_entries() if path.absolute() != config)]
    for candidate in candidates:
        roster = load(candidate)
        if roster and (Path(roster["top"]) == config or any(
            Path(lead["config"]) == config for lead in roster["leads"].values()
        )):
            return candidate, roster
    return None


def readers(roster):
    result = {f"lead:{name}": {**lead, "name": name} for name, lead in roster["leads"].items() if lead["active"]}
    chief = roster.get("chief_of_staff")
    if chief:
        result[f"chief_of_staff:{chief['name']}"] = {"config": roster["top"], **chief}
    candidate = roster.get("candidate")
    if candidate:
        result[f"candidate:{candidate['name']}"] = {"config": roster["top"], **candidate}
    return result


class Publication:
    def __init__(self, config, floor):
        self.config = config
        try:
            with private_open(folder(config) / "rail-published.json", os.O_RDONLY) as source:
                self.links = json.load(source)
            if not isinstance(self.links, list) or not all(valid_publication(link) for link in self.links):
                raise ValueError("invalid published links")
        except FileNotFoundError:
            if floor > 1:
                raise board.RecipeError("Rail publication paused: history is missing after a rail trim; restore this board's publication history")
            self.links = []
            self.save()
        except (OSError, ValueError, board.RecipeError) as error:
            raise board.RecipeError("Rail publication paused: history is unreadable; restore this board's publication history") from error

    def contains(self, event, link):
        return [event, *link] in self.links

    def remember(self, event, link):
        key = [event, *link] if isinstance(link, list) else None
        if valid_publication(key) and key not in self.links:
            self.links.append(key)

    def save(self):
        save(self.config, self.links, "rail-published.json")


def valid_publication(link):
    return (isinstance(link, list) and len(link) == 5 and link[0] in ("started", "finished")
            and all(isinstance(value, str) for value in link[1:4])
            and type(link[4]) is int and 0 < link[4] < 2**64)


@contextmanager
def publication(config, floor):
    root = ensure_folder(config)
    with private_open(root / "rail-published.lock", os.O_RDWR | os.O_CREAT) as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield Publication(config, floor)
