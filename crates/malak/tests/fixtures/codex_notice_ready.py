"""Wait for the fixture proxy to start before timing the WebSocket exchange."""

import os
import selectors
import subprocess


popen = subprocess.Popen


def wait_for_proxy(argv, *args, **kwargs):
    if argv != ["codex", "app-server", "proxy"]:
        return popen(argv, *args, **kwargs)

    timeout = int(os.environ["MALAK_TEST_CODEX_PROXY_READY"])
    kwargs["stderr"] = subprocess.PIPE
    process = popen(argv, *args, **kwargs)
    try:
        with selectors.DefaultSelector() as ready:
            ready.register(process.stderr, selectors.EVENT_READ)
            # The fixture signals on stderr so stdout remains a pure WebSocket stream.
            if ready.select(timeout) and os.read(process.stderr.fileno(), 1) == b"\0":
                return process
        raise AssertionError("Codex fixture proxy did not signal readiness")
    except BaseException:
        with process:
            process.kill()
        raise


subprocess.Popen = wait_for_proxy
