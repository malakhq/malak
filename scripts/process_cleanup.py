"""Let command-line tools unwind their temporary resources on interruption."""

from contextlib import contextmanager
import os
import signal
import subprocess


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
