"""Local board selection and the public prefix of participant handovers."""

import json
import os
from pathlib import Path
import re
import shlex
import stat
import subprocess
import tomllib


LEAD_WATCHER_CONSEQUENCE = (
    "reports, questions and syncs from your workers, sub-agents and other lead agents will go unseen."
)


class BoardSelectionError(ValueError):
    def __init__(self, message, code=2):
        super().__init__(message)
        self.code = code


def _board_query(*args):
    result = subprocess.run(
        [os.environ.get("MALAK_RECIPE_EXE", "malak"), *args],
        stdin=subprocess.DEVNULL, capture_output=True, text=True,
    )
    if result.returncode:
        raise BoardSelectionError(result.stderr.strip(), result.returncode)
    return result.stdout.removesuffix("\n")


def board_config(name=None):
    if name is not None:
        return Path(_board_query("recipe", "__board-config", name))
    base = Path(os.environ.get("XDG_CONFIG_HOME", ""))
    if not base.is_absolute():
        base = Path.home() / ".config"
    return base / "malak" / "board.toml"


def selected_config(name=None, path=None):
    global_config = os.environ.get("MALAK_RECIPE_CONFIG")
    if global_config:
        if name is not None or path is not None:
            raise BoardSelectionError("choose a board only once with --board or --config")
        return Path(global_config)
    return board_config(name) if name is not None else path


def board_name(config):
    return _board_query("--config", config, "recipe", "__board-name")


def board_selector(config, name):
    try:
        if board_config(name) == config.absolute():
            return f"--board {shlex.quote(name)}"
    except BoardSelectionError:
        pass
    return f"--config {shlex.quote(str(config.absolute()))}"


def lead_watcher_command(config, name):
    return f"malak {board_selector(config, name)} mailbox --plain --for lead"


def board_entries():
    return [(name, Path(path)) for name, path in json.loads(_board_query("recipe", "__board-list"))]


def board_settings(config):
    with config.open("rb") as source:
        return tomllib.load(source)["board"]


def config_for_socket(socket_folder):
    for _, config in board_entries():
        try:
            if Path(board_settings(config)["socket_folder"]) == socket_folder:
                return config
        except (KeyError, OSError, TypeError, ValueError):
            continue
    return None


def local_board_label(socket_folder):
    home = Path.home()
    if socket_folder.is_relative_to(home):
        return f"~/{socket_folder.relative_to(home)}"
    return str(socket_folder)


def read_cbor_head(file, major):
    initial = file.read(1)
    if not initial or initial[0] >> 5 != major:
        raise ValueError("unexpected handover field")
    size = initial[0] & 31
    if size < 24:
        return size
    widths = {24: 1, 25: 2, 26: 4, 27: 8}
    width = widths.get(size)
    if width is None:
        raise ValueError("invalid handover field")
    encoded = file.read(width)
    if len(encoded) != width:
        raise ValueError("truncated handover")
    return int.from_bytes(encoded, "big")


def read_bytes(file, major, limit):
    size = read_cbor_head(file, major)
    if size > limit:
        raise ValueError("oversize handover field")
    value = file.read(size)
    if len(value) != size:
        raise ValueError("truncated handover")
    return value


def board_identity(handover):
    socket_folder, participant, _ = handover_identity(handover)
    return socket_folder, participant


def handover_identity(handover):
    # Read the v0.1 participant handover prefix only. Unbuffered reads leave
    # the mechanism data and private key untouched.
    fd = os.open(handover, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb", buffering=0) as file:
        if not stat.S_ISREG(os.fstat(file.fileno()).st_mode):
            raise ValueError("handover must be a file")
        shape = tuple(read_cbor_head(file, major) for major in (4, 0, 0, 0))
        if shape not in {(9, 1, 1, 1), (9, 1, 1, 2)}:
            raise ValueError("unsupported handover shape")
        address = read_bytes(file, 3, 4096).decode()
        pin = read_bytes(file, 2, 32)
        if len(pin) != 32:
            raise ValueError("invalid board pin")
        participant = read_bytes(file, 3, 32).decode()

    socket = Path(address.removeprefix("unix:"))
    lead = shape[-1] == 2
    if (
        not address.startswith("unix:")
        or not socket.is_absolute()
        or socket.name != "s"
        or socket.parent.name != ("l" if lead else "p")
        or "\0" in address
        or (participant != "lead" if lead else not re.fullmatch("[a-z0-9]{8,32}", participant))
    ):
        raise ValueError("invalid local board identity")
    return socket.parent.parent, participant, pin.hex()
