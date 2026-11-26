"""Let command-line tools unwind their temporary resources on interruption."""

from contextlib import contextmanager
import os
from pathlib import Path
import pwd
import secrets
import shutil
import signal
import subprocess
import sys


class Interrupted(SystemExit):
    def __init__(self, signum):
        self.signum = signum
        super().__init__(128 + signum)


@contextmanager
def cleanup_on_interrupt():
    def interrupt(signum, _frame):
        raise Interrupted(signum)

    previous = {sig: signal.signal(sig, interrupt) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        yield
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


@contextmanager
def command_process(command, **kwargs):
    """Stop and reap a command and signal its children before removing their files."""
    with subprocess.Popen(command, start_new_session=True, **kwargs) as process:
        try:
            yield process
        except BaseException as error:
            signum = error.signum if isinstance(error, Interrupted) else signal.SIGTERM
            try:
                os.killpg(process.pid, signum)
                process.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
            raise


def account_home():
    return Path(pwd.getpwuid(os.geteuid()).pw_dir).resolve()


@contextmanager
def private_check_run(env=None):
    """Give one check run a private temp folder and a run id for its test homes.

    The folder is mode 700 under the account home, because the SQLite store
    refuses a folder inside a world-writable parent such as /tmp. The
    environment it yields sets TMPDIR to the folder and MALAK_CHECK_RUN to the
    run id, which tags the run's short test homes as ~/.m/<run id>. Both are
    removed afterwards.
    """
    home = account_home()
    cache = home / ("Library/Caches" if sys.platform == "darwin" else ".cache")
    run = secrets.token_hex(3)
    temporary = cache / f"malak-check-{run}"
    temporary.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary.mkdir(mode=0o700)
    try:
        yield {**(os.environ if env is None else env), "TMPDIR": str(temporary), "MALAK_CHECK_RUN": run}
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
        shutil.rmtree(home / ".m" / run, ignore_errors=True)
        try:
            (home / ".m").rmdir()
        except OSError:
            # Other runs or fixtures are still using the shared short directory.
            pass
