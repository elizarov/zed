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

def references(branch="main", tag_revision="revision-2"):
    return [
        {"kind": "branch", "name": branch, "revision": "revision-2", "upstream": {"name": "origin/main", "ahead": 2, "behind": 1}},
        {"kind": "branch", "name": "topic", "revision": "revision-1", "upstream": {"name": "origin/topic"}},
        {"kind": "branch", "name": "gone", "revision": "revision-1", "upstream": {"name": "origin/gone", "gone": True}},
        {"kind": "remoteBranch", "name": "origin/main", "revision": "revision-1"},
        {"kind": "tag", "name": "v1", "revision": tag_revision},
        {"kind": "tag", "name": "main", "revision": "revision-1"},
    ]

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
        result = {"protocolVersion": "9.0" if mode == "version" else "0.1", "capabilities": {"readOnly": True, "staging": True, "history": mode.startswith("history") or mode == "remote"}}
        if mode in {"history-refs", "remote"} or mode.startswith("bad-refs"):
            result["capabilities"].update(branches=True, tags=True, tracking=True)
        if mode == "bad-tracking":
            result["capabilities"]["tracking"] = True
        if mode == "reserved-feature":
            result["capabilities"]["blame"] = True
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
        result = {"snapshot": str(counter), "revision": "revision-2" if mode.startswith("history") else "opaque-revision", "branch": "main", "changes": [
            {"path": "../escape" if mode == "bad-path" else "hello.txt", "status": "modified", "stagedStatus": "modified"},
            {"path": "new.txt", "status": "untracked"},
            {"path": "deleted.txt", "status": "deleted"},
        ]}
        if mode == "history-refs" or mode.startswith("bad-refs"):
            result["references"] = references()
            if mode == "bad-refs-duplicate": result["references"].append(result["references"][0])
            if mode == "bad-refs-name": result["references"][0]["name"] = "bad\nname"
            if mode == "bad-refs-counts": del result["references"][0]["upstream"]["ahead"]
            if mode == "bad-refs-gone": result["references"][0]["upstream"]["gone"] = True
        if mode == "unadvertised-refs": result["references"] = references()
        if mode == "remote":
            state_path = Path(sys.argv[2])
            state = json.loads(state_path.read_text())
            while state.get("pauseStatus"):
                time.sleep(0.01)
                state = json.loads(state_path.read_text())
            if state.get("failStatus"):
                send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32000, "message": "mock startup failed"}})
                continue
            result = {"snapshot": state["snapshot"], "revision": "revision-2", "branch": "remote-branch", "changes": state["changes"], "references": references("remote-branch", state.get("tagRevision", "revision-2"))}
        send({"jsonrpc": "2.0", "method": "repository/changed", "params": {"repository": "mock"}})
    elif method == "repository/comparison":
        comparison_counter += 1
        if mode == "expired" and comparison_counter == 1:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32001, "message": "expired snapshot"}})
            continue
        result = [{"path": path, "base": None if path == "new.txt" else "base", "index": None if path == "new.txt" else "index"} for path in message["params"]["paths"]]
        if mode == "remote":
            result = [{"path": path, "base": "base-" + message["params"]["snapshot"], "index": "index-" + message["params"]["snapshot"]} for path in message["params"]["paths"]]
    elif method in {"repository/history", "repository/commitDetails"}:
        commits = [{"id": "revision-2", "parents": ["revision-1"], "authorName": "Example", "authorEmail": "example@example.test", "timestamp": 1700000000, "message": "Change hello\n\nDetails"},
                   {"id": "revision-1", "parents": [], "authorName": "Example", "authorEmail": "example@example.test", "timestamp": 1600000000, "message": "Initial"}]
        if method == "repository/commitDetails":
            result = next(commit for commit in commits if commit["id"] == message["params"]["revision"])
        else:
            if message["params"]["revision"] == "revision-1": commits = commits[1:]
            limit = message["params"]["limit"]
            result = {"commits": commits[:limit], "hasMore": limit < len(commits)}
            if mode == "history-bad-id":
                result["commits"] = [commits[0], commits[0]]
            if mode == "history-too-many":
                result["commits"] = commits
    elif method == "repository/commitChanges":
        result = [{"path": "../escape" if mode == "history-bad-path" else "hello.txt", "base": "before", "target": "after"},
                  {"path": "new.txt", "base": None, "target": "after"},
                  {"path": "gone.txt", "base": "before", "target": None}]
        if mode == "remote":
            result.extend([
                {"path": "large-added.c", "base": None, "target": "oversized"},
                {"path": "large-deleted.c", "base": "oversized", "target": None},
                {"path": "large-modified.c", "base": "before", "target": "oversized"},
                {"path": "after-large.txt", "base": None, "target": "after"},
            ])
    elif method == "repository/readContent":
        if mode == "remote" and json.loads(Path(sys.argv[2]).read_text()).get("failContent"):
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32000, "message": "mock content failed"}})
            continue
        if message["params"]["content"] == "oversized":
            send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32002, "message": "Content exceeds the file size limit"}})
            continue
        content = message["params"]["content"].encode() + (b"\n" if mode in {"text", "remote"} else b"\n\x00\xff")
        result = {"encoding": "base64", "data": base64.b64encode(content).decode()}
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "unsupported method"}})
        continue
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
