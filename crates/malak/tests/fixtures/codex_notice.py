#!/usr/bin/env python3
import base64
import hashlib
import json
from pathlib import Path
import sys

assert sys.argv[1:] == ["app-server", "proxy"]
source, sink = sys.stdin.buffer, sys.stdout.buffer
headers = {}
assert source.readline() == b"GET / HTTP/1.1\r\n"
while (line := source.readline().strip()):
    key, value = line.split(b":", 1)
    headers[key] = value.strip()
accept = base64.b64encode(hashlib.sha1(
    headers[b"Sec-WebSocket-Key"] + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
).digest())
sink.write(b"HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
sink.flush()
while (head := source.read(2)):
    size = head[1] & 0x7f
    if size == 126:
        size = int.from_bytes(source.read(2), "big")
    assert head[0] == 0x81 and head[1] & 0x80
    mask = source.read(4)
    msg = json.loads(bytes(byte ^ mask[i % 4] for i, byte in enumerate(source.read(size))))
    match msg["method"]:
        case "initialize":
            assert msg["params"]["capabilities"]["experimentalApi"]
            result = {}
        case "initialized":
            continue
        case "thread/loaded/list":
            result = {"data": ["opened-thread"], "nextCursor": None}
        case "thread/inject_items":
            with (Path.home() / "injected.jsonl").open("a") as log:
                log.write(json.dumps(msg["params"]) + "\n")
            result = {}
        case _:
            sys.exit(1)
    payload = json.dumps({"id": msg["id"], "result": result}).encode()
    assert len(payload) < 126
    sink.write(bytes([0x81, len(payload)]) + payload)
    sink.flush()
