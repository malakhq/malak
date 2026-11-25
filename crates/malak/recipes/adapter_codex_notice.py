"""Send an opt-in notice to already loaded Codex app-server threads."""

import base64
import hashlib
import json
import os
import selectors
import struct
import subprocess
import time


NOTICE = (
    "Malak is installed for visibility. It lets you coordinate agents on a shared board. "
    "Use the malak skill or run `malak recipe --help` when asked to start, join, or "
    "take over a board."
)


class ControlSocket:
    """WebSocket frames transported by the Codex Unix-socket proxy."""

    def __init__(self, process):
        self.process = process
        self.pending = bytearray()
        self.deadline = time.monotonic() + 5
        key = base64.b64encode(os.urandom(16))
        process.stdin.write(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
            b"Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
            b"Sec-WebSocket-Key: " + key + b"\r\n\r\n"
        )
        process.stdin.flush()
        with selectors.DefaultSelector() as ready:
            ready.register(process.stdout, selectors.EVENT_READ)
            while b"\r\n\r\n" not in self.pending:
                if len(self.pending) >= 8192:
                    raise ValueError("oversize app-server handshake")
                self.read_more(ready)
        headers, self.pending = self.pending.split(b"\r\n\r\n", 1)
        status, *headers = headers.split(b"\r\n")
        expected = base64.b64encode(hashlib.sha1(
            key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        ).digest())
        accept = next(
            (value.strip() for name, _, value in (line.partition(b":") for line in headers)
             if name.lower() == b"sec-websocket-accept"), None
        )
        if status.split()[1:2] != [b"101"] or accept != expected:
            raise ConnectionError("app-server rejected the WebSocket upgrade")

    def read_more(self, ready):
        if not ready.select(max(0, self.deadline - time.monotonic())):
            raise TimeoutError("timed out waiting for the app-server")
        data = os.read(self.process.stdout.fileno(), 65536)
        if not data:
            raise ConnectionError("app-server connection closed")
        self.pending.extend(data)

    def take(self, ready, size):
        while len(self.pending) < size:
            self.read_more(ready)
        data = self.pending[:size]
        del self.pending[:size]
        return data

    def send(self, payload, opcode=1):
        mask = os.urandom(4)
        size = len(payload)
        if size < 126:
            length = bytes([0x80 | size])
        elif size < 65536:
            length = bytes([0x80 | 126]) + struct.pack("!H", size)
        else:
            length = bytes([0x80 | 127]) + struct.pack("!Q", size)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.process.stdin.write(bytes([0x80 | opcode]) + length + mask + masked)
        self.process.stdin.flush()

    def receive(self, ready):
        message = bytearray()
        while time.monotonic() < self.deadline:
            frame, size = self.take(ready, 2)
            opcode = frame & 0x0F
            if frame & 0x70 or size & 0x80:
                raise ValueError("invalid app-server WebSocket frame")
            if size in (126, 127):
                size = int.from_bytes(self.take(ready, 2 if size == 126 else 8), "big")
            if size + len(message) > 8 * 1024 * 1024:
                raise ValueError("oversize app-server reply")
            payload = self.take(ready, size)
            if opcode == 8:
                raise ConnectionError("app-server closed the WebSocket")
            if opcode == 9:
                self.send(payload, opcode=10)
            elif opcode in (0, 1):
                message.extend(payload)
                if frame & 0x80:
                    return json.loads(message)
        raise TimeoutError("timed out waiting for the app-server")


class AppServer:
    def __init__(self, process):
        self.socket = ControlSocket(process)
        self.request_id = 0

    def send(self, message):
        self.socket.send(json.dumps(message).encode())

    def call(self, method, params):
        self.request_id += 1
        self.send({"id": self.request_id, "method": method, "params": params})
        self.socket.deadline = time.monotonic() + 5
        with selectors.DefaultSelector() as ready:
            ready.register(self.socket.process.stdout, selectors.EVENT_READ)
            while time.monotonic() < self.socket.deadline:
                message = self.socket.receive(ready)
                if message.get("id") == self.request_id:
                    if "error" in message:
                        raise ValueError(message["error"].get("message", method))
                    return message["result"]
        raise TimeoutError(f"timed out waiting for {method}")

    def threads(self):
        cursor = None
        while True:
            page = self.call("thread/loaded/list", {"cursor": cursor})
            yield from page["data"]
            cursor = page.get("nextCursor")
            if cursor is None:
                return

    def inject(self, thread, notice):
        self.call("thread/inject_items", {
            "threadId": thread,
            "items": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": notice}],
            }],
        })


def no_app_server(error):
    print(f"No reachable Codex app-server could list open threads ({error}).")
    return 0


def reload_hooks():
    """Ask the Codex app-server to reread config for every loaded thread.

    Codex rebuilds a thread's hooks from the user config on this request, so changed
    Malak entries apply without a restart. An empty batch writes nothing. Hook trust
    stays with the owner: a reloaded entry still waits for approval in /hooks.
    """
    try:
        process = subprocess.Popen(
            ["codex", "app-server", "proxy"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
    except OSError as error:
        return no_app_server(error)
    with process:
        try:
            try:
                app = AppServer(process)
                app.call("initialize", {
                    "clientInfo": {"name": "malak", "version": "0.1.2"},
                    "capabilities": {"experimentalApi": True},
                })
                app.send({"method": "initialized"})
                app.call("config/batchWrite", {"edits": [], "reloadUserConfig": True})
            except (OSError, ValueError, KeyError) as error:
                print(f"Could not ask open Codex threads to reload their hooks ({error}).")
                return False
        finally:
            if process.poll() is None:
                process.kill()
            try:
                process.stdin.close()
            except BrokenPipeError:
                pass
    print("Asked open Codex threads to reload their hooks.")
    return True


def notify_open(delivered, save, notice=NOTICE):
    try:
        process = subprocess.Popen(
            ["codex", "app-server", "proxy"], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
        )
    except OSError as error:
        return no_app_server(error)
    sent = 0
    with process:
        try:
            try:
                app = AppServer(process)
                app.call("initialize", {
                    "clientInfo": {"name": "malak", "version": "0.1.2"},
                    "capabilities": {"experimentalApi": True},
                })
                app.send({"method": "initialized"})
                threads = list(app.threads())
            except (OSError, ValueError, KeyError) as error:
                return no_app_server(error)
            for thread in threads:
                if thread in delivered:
                    continue
                try:
                    app.inject(thread, notice)
                except (OSError, ValueError, KeyError) as error:
                    print(f"Could not notify Codex thread {thread} ({error}).")
                    continue
                delivered.append(thread)
                save()
                sent += 1
        finally:
            if process.poll() is None:
                process.kill()
            try:
                process.stdin.close()
            except BrokenPipeError:
                # A failed flush can leave bytes buffered after the proxy exits.
                pass
    if sent:
        print(f"Notified {sent} open Codex thread(s).")
    return sent


# What `recipe agents` says about notifying open sessions.
HELP = "also notify open local Codex app-server threads"
QUESTION = " and notify open local Codex threads"
WOULD_NOTIFY = "Would notify open local Codex threads once."
NONE_NOTIFIED = "No new Codex threads were notified."
