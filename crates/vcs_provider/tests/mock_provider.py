#!/usr/bin/env python3
"""Protocol fixture with no dependency on an installed VCS."""
import base64
import json
import os
from pathlib import Path
import sys
import time

mode = sys.argv[1] if len(sys.argv) > 1 else "normal"
root = None
counter = 0
comparison_counter = 0

def send(value):
    body = json.dumps(value).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()

while True:
    header = sys.stdin.buffer.readline()
    if not header:
        break
    length = int(header.split(b":")[1])
    assert sys.stdin.buffer.readline() == b"\r\n"
    message = json.loads(sys.stdin.buffer.read(length))
    method = message["method"]
    if mode == "eof":
        break
    if mode == "timeout":
        time.sleep(10)
    if mode == "bad-id":
        send({"jsonrpc": "2.0", "id": 999, "result": {}})
        continue
    if method == "initialize":
        result = {"protocolVersion": "9.0" if mode == "version" else "0.1", "capabilities": {"readOnly": True, "staging": True}}
    elif method == "repository/discover":
        root = message["params"]["workspaceRoot"]
        if mode == "remote":
            assert os.getcwd() == root
            assert os.environ["VCS_PROVIDER_TEST_HOST"] == "server"
        result = {"id": "mock", "root": root, "label": "Mock VCS"}
    elif method == "repository/status":
        counter += 1
        if mode == "status-error" and counter > 1:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32000, "message": "offline"}})
            continue
        result = {"snapshot": str(counter), "revision": "opaque-revision", "branch": "main", "changes": [
            {"path": "../escape" if mode == "bad-path" else "hello.txt", "status": "modified", "stagedStatus": "modified"},
            {"path": "new.txt", "status": "untracked"},
            {"path": "deleted.txt", "status": "deleted"},
        ]}
        if mode == "remote":
            state = json.loads(Path(sys.argv[2]).read_text())
            result = {"snapshot": state["snapshot"], "branch": "remote-branch", "changes": state["changes"]}
        send({"jsonrpc": "2.0", "method": "repository/changed", "params": {"repository": "mock"}})
    elif method == "repository/comparison":
        comparison_counter += 1
        if mode == "expired" and comparison_counter == 1:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32001, "message": "expired snapshot"}})
            continue
        result = [{"path": path, "base": None if path == "new.txt" else "base", "index": None if path == "new.txt" else "index"} for path in message["params"]["paths"]]
        if mode == "remote":
            result = [{"path": path, "base": "base-" + message["params"]["snapshot"], "index": "index-" + message["params"]["snapshot"]} for path in message["params"]["paths"]]
    elif method == "repository/readContent":
        content = message["params"]["content"].encode() + (b"\n" if mode in {"text", "remote"} else b"\n\x00\xff")
        result = {"encoding": "base64", "data": base64.b64encode(content).decode()}
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "unsupported method"}})
        continue
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
