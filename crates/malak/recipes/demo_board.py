"""Own the files and locks for disposable demo boards."""

from contextlib import ExitStack, contextmanager
import errno
import fcntl
import json
import os
from pathlib import Path
import secrets
import shutil
import stat
import tempfile


MARKER = ".malak-demo"


def board_root():
    home = Path.home()
    if not home.is_absolute():
        raise OSError("the home directory must be absolute")
    return home / ".malak"


class DemoBoard:
    def __init__(self, folder):
        self.folder = folder
        self.name = folder.name
        self.config = folder / "board.toml"
        self.state = folder / "state"
        self.sockets = folder / "s"

    def marker(self, keep):
        mode = "keep" if keep else "temporary"
        return f"malak demo v1\n{self.name}\n{mode}\n".encode()

    def settings(self):
        state = json.dumps(str(self.state), ensure_ascii=False)
        sockets = json.dumps(str(self.sockets), ensure_ascii=False)
        return f"[board]\nstate = {state}\nsocket_folder = {sockets}\n".encode()

    @contextmanager
    def mark(self, keep):
        with ExitStack() as stack:
            state_created = False
            marker = None
            marked = False
            prepared = False
            try:
                self.state.mkdir(mode=0o700)
                state_created = True
                marker = stack.enter_context((self.state / MARKER).open("xb"))
                fcntl.flock(marker, fcntl.LOCK_EX)
                marker.write(self.marker(keep))
                marker.flush()
                marked = True
                pending = self.folder / ".board.toml"
                pending.write_bytes(self.settings())
                pending.replace(self.config)
                prepared = True
                yield marker
            finally:
                if marked:
                    if (not keep or not prepared) and not self.remove_stopped(marker):
                        raise OSError(f"demo board {self.name} is still running")
                else:
                    if marker is not None:
                        (self.state / MARKER).unlink()
                    if state_created:
                        self.state.rmdir()
                    self.folder.rmdir()

    @contextmanager
    def claim(self, *, temporary_only):
        """Hold a matching marker while checking or deleting a stopped board."""
        with ExitStack() as stack:
            try:
                folder = stack.enter_context(directory(self.folder))
                state = stack.enter_context(directory("state", folder))
                marker = stack.enter_context(read_file(MARKER, state))
            except OSError as error:
                if error.errno not in {errno.ENOENT, errno.ENOTDIR, errno.ELOOP, errno.EACCES}:
                    raise
                yield None
                return
            temporary = self.marker(False)
            recognized = (temporary,) if temporary_only else (temporary, self.marker(True))
            if marker.read(len(temporary) + 1) not in recognized:
                yield None
                return
            if not try_lock(marker):
                raise BlockingIOError(f"demo board {self.name} is running or in use")
            try:
                # An interrupted setup may have published the marker only.
                with read_file("board.toml", folder) as config:
                    settings = self.settings()
                    valid = config.read(len(settings) + 1) == settings
            except FileNotFoundError:
                valid = True
            except PermissionError:
                valid = False
            except OSError as error:
                if error.errno != errno.ELOOP:
                    raise
                valid = False
            yield marker if valid else None

    def remove_stopped(self, marker):
        # Board starts append under the registry directory's flock. Keep the
        # socket directory's flock through deletion, as a running board does.
        with directory(self.folder.parent) as root:
            fcntl.flock(root, fcntl.LOCK_EX)
            marker_path = self.state / MARKER
            try:
                current = marker_path.lstat()
            except FileNotFoundError:
                return True
            if not os.path.samestat(current, os.fstat(marker.fileno())):
                raise OSError(f"demo board {self.name} changed during cleanup")
            self.sockets.mkdir(mode=0o700, exist_ok=True)
            with directory(self.sockets) as sockets:
                if not try_lock(sockets):
                    return False
                remove_registry_entry(self.config)
                # Leave the marker until the board's files are gone, so a
                # crash during deletion can be retried by the next demo.
                remove_contents(self.folder, except_path=self.state)
                remove_contents(self.state, except_path=marker_path)
                marker_path.unlink()
                self.state.rmdir()
                self.folder.rmdir()
                return True


@contextmanager
def directory(path, parent=None):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
    try:
        if not owned(fd, stat.S_ISDIR):
            raise PermissionError(errno.EACCES, "directory must be writable only by its owner", path)
        yield fd
    finally:
        os.close(fd)


def owned(fd, kind):
    info = os.fstat(fd)
    return kind(info.st_mode) and info.st_uid == os.geteuid() and not info.st_mode & 0o022


@contextmanager
def read_file(path, parent):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
    with os.fdopen(fd, "rb") as source:
        if not owned(source.fileno(), stat.S_ISREG):
            raise PermissionError(errno.EACCES, "expected an owned regular file", path)
        yield source


def try_lock(file):
    try:
        fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except BlockingIOError:
        return False


def remove_registry_entry(config):
    registry = config.parent.parent / "boards.jsonl"
    try:
        fd = os.open(registry, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return
    with os.fdopen(fd, "rb") as source:
        if not owned(source.fileno(), stat.S_ISREG):
            raise PermissionError(f"{registry} must be an owned registry file")
        lines = source.readlines()
    retained = []
    for line in lines:
        try:
            if json.loads(line) == str(config):
                # record() puts a blank separator before each complete entry.
                if retained and retained[-1] == b"\n":
                    retained.pop()
                continue
        except ValueError:
            pass
        retained.append(line)
    if len(retained) == len(lines):
        return
    with tempfile.NamedTemporaryFile(dir=config.parent, prefix=".boards.", delete=False) as output:
        pending = Path(output.name)
        try:
            output.writelines(retained)
            output.close()
            pending.replace(registry)
        finally:
            pending.unlink(missing_ok=True)


def remove_contents(folder, *, except_path):
    for entry in folder.iterdir():
        if entry == except_path:
            continue
        if stat.S_ISDIR(entry.lstat().st_mode):
            shutil.rmtree(entry)
        else:
            entry.unlink()


def create(root):
    root.mkdir(mode=0o700, exist_ok=True)
    with directory(root):
        while True:
            folder = root / f"demo-{secrets.token_hex(6)}"
            try:
                folder.mkdir(mode=0o700)
            except FileExistsError:
                continue
            return DemoBoard(folder)


def reap_stopped(root):
    if root.exists():
        with directory(root):
            for folder in root.glob("demo-*"):
                board = DemoBoard(folder)
                try:
                    with board.claim(temporary_only=True) as marker:
                        if marker is not None:
                            board.remove_stopped(marker)
                except BlockingIOError:
                    continue
