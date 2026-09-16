"""Installed-tool fixture, copied under each tool name. No real tool execution."""
# RESUMED LIGHT PREP / UNEXECUTED: no fixture has run and no Red is claimed.
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import threading
import time


def record(path, value):
    with path.open("a") as stream:
        stream.write(json.dumps(value, ensure_ascii=True) + "\n")


def record_command_server_state(root):
    launches = [json.loads(line)
                for line in (root / "server-launches.jsonl").read_text().splitlines()]
    first_server_pid = launches[0]["pid"]
    try:
        os.kill(first_server_pid, 0)
        first_server_alive = True
    except ProcessLookupError:
        first_server_alive = False
    marker = json.loads(
        (root.parent / "assignment.work/.rustrace/command-activity.json").read_text()
    )
    record(root / "command-server-state.json", {
        "command_marker": marker,
        "first_server_pid": first_server_pid,
        "first_server_alive": first_server_alive,
    })


def server(root, mode):
    send_lock = threading.Lock()

    def send(value):
        with send_lock:
            record(root / "server-frames.jsonl", value)
            body = json.dumps(value).encode()
            sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
            sys.stdout.buffer.flush()

    launch_log = root / "server-launches.jsonl"
    launch_count = len(launch_log.read_text().splitlines()) if launch_log.exists() else 0
    record(launch_log, {"pid": os.getpid(), "launch": launch_count + 1, "mode": mode})
    if mode == "mutate_resolution":
        workspace = root.parent / "assignment.work"
        (root / "main-at-server-launch.txt").write_bytes((workspace / "main.rs").read_bytes())
    opens = 0
    root_uri = None
    request_step = 0
    while True:
        header = bytearray()
        while not header.endswith(b"\r\n\r\n"):
            byte = sys.stdin.buffer.read(1)
            if not byte:
                assert not header, "partial client header"
                return
            header.extend(byte)
            assert len(header) <= 8192, "oversized client header"
        lengths = [int(line.split(b":", 1)[1]) for line in header.split(b"\r\n")
                   if line.lower().startswith(b"content-length:")]
        assert len(lengths) == 1 and 0 <= lengths[0] <= 8 * 1024 * 1024
        body = sys.stdin.buffer.read(lengths[0])
        assert len(body) == lengths[0], "partial client body"
        message = json.loads(body)
        record(root / "frames.jsonl", message)
        method = message.get("method")
        if method == "initialize":
            root_uri = message["params"]["rootUri"]
            if mode == "init_blocked":
                time.sleep(60)
            if mode == "partial":
                sys.stdout.buffer.write(b"Content-Length: 10\r\n\r\n{")
                sys.stdout.buffer.flush()
                return
            if mode == "oversized":
                sys.stdout.buffer.write(b"Content-Length: 999999999\r\n\r\n")
                sys.stdout.buffer.flush()
                return
            encoding = "utf-8" if mode == "unsupported" else "utf-16"
            send({"jsonrpc": "2.0", "id": message["id"], "result": {
                "capabilities": {"positionEncoding": encoding,
                                 "textDocumentSync": {"openClose": True, "change": 2}},
                "serverInfo": {"name": "rustrace-lifecycle-fixture"}}})
            if mode == "malformed":
                sys.stdout.buffer.write(b"Content-Length: 5\r\n\r\n{bad!")
                sys.stdout.buffer.flush()
            elif mode == "flood":
                for index in range(100):
                    send({"jsonrpc": "2.0", "method": "$/progress",
                          "params": {"token": "bounded", "value": index}})
        elif method == "initialized" and mode == "requests":
            send({"jsonrpc": "2.0", "id": "apply", "method": "workspace/applyEdit",
                  "params": {"edit": {"changes": {}}}})
            request_step = 1
        elif method == "initialized" and mode == "blocked_stdin":
            (root / "blocked-stdin-ready").write_text("initialized")
            time.sleep(60)
        elif method == "textDocument/didOpen":
            opens += 1
            if opens == 1 and mode == "diagnostics_rust_only":
                send({
                    "jsonrpc": "2.0",
                    "method": "textDocument/publishDiagnostics",
                    "params": {
                        "uri": root_uri + "/Cargo.toml",
                        "version": 1,
                        "diagnostics": [{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 1},
                            },
                            "severity": 1,
                            "source": "fixture",
                            "message": "TOML live problem",
                        }],
                    },
                })
                (root / "non-rust-diagnostic-sent").write_text("sent")
            if opens == 2 and mode == "command_barrier":
                (root / "command-barrier-ready").write_text("opens received")
                while not (root / "release-command-barrier").is_file():
                    time.sleep(.01)
            if opens == 2 and mode == "crash_loop":
                os._exit(23)
            if opens == 2 and mode in ("mutate", "invalid"):
                workspace = root.parent / "assignment.work"
                if mode == "mutate":
                    (workspace / "main.rs").write_text("SERVER MUTATION\n")
                else:
                    (workspace / "evil.txt").write_text("unmanaged server mutation\n")
                (root / "mutation-complete").write_text(mode)
        elif method == "textDocument/didChange" and mode == "crash" and launch_count == 0:
            os._exit(23)
        elif method == "textDocument/didChange" and mode == "command_barrier":
            marker = root.parent / "assignment.work/.rustrace/command-activity.json"
            state = json.loads(marker.read_text()) if marker.is_file() else {"active": False}
            (root / "change-command-state.json").write_text(json.dumps(state))
        elif method == "textDocument/didChange" and mode == "diagnostics":
            document = message["params"]["textDocument"]
            text = message["params"]["contentChanges"][0]["text"]
            diagnostics = ([{
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 1},
                },
                "severity": 1,
                "source": "fixture",
                "message": "live problem",
            }] if text.startswith("X") else [])
            send({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": document["uri"],
                    "version": document["version"],
                    "diagnostics": diagnostics,
                },
            })
        elif method == "textDocument/didChange" and mode == "diagnostics_rust_only":
            document = message["params"]["textDocument"]
            is_rust = document["uri"].endswith(".rs")
            send({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": document["uri"],
                    "version": document["version"],
                    "diagnostics": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 1},
                        },
                        "severity": 1,
                        "source": "fixture",
                        "message": ("Rust live problem" if is_rust
                                    else "TOML live problem"),
                    }],
                },
            })
        elif method == "textDocument/didChange" and mode == "diagnostics_malformed":
            document = message["params"]["textDocument"]
            send({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": document["uri"],
                    "diagnostics": [{"message": "malformed live hint"}],
                },
            })
        elif (method == "textDocument/completion"
              and mode in ("completion", "completion_mouse", "completion_mouse_control",
                           "completion_paste_rejection",
                           "completion_navigation", "completion_console_retirement",
                           "completion_test_cases_retirement", "completion_automatic",
                           "completion_automatic_control", "completion_automatic_keyboard_cancel",
                           "completion_automatic_mouse_cancel", "completion_automatic_console_cancel",
                           "completion_silence_crash",
                           "completion_silence_timeout", "completion_silence_empty")):
            assert message["params"]["context"] == {"triggerKind": 1}
            if mode == "completion_silence_crash" and launch_count == 0:
                os._exit(23)
            if mode == "completion_silence_timeout":
                continue
            if mode in ("completion_silence_crash", "completion_silence_empty"):
                send({"jsonrpc": "2.0", "id": message["id"], "result": {
                    "isIncomplete": False, "items": []
                }})
                continue
            if mode in ("completion_automatic", "completion_automatic_control",
                        "completion_automatic_keyboard_cancel", "completion_automatic_mouse_cancel",
                        "completion_automatic_console_cancel"):
                automatic_requests = sum(
                    frame.get("method") == "textDocument/completion"
                    for frame in (json.loads(line)
                                  for line in (root / "frames.jsonl").read_text().splitlines())
                )
                expected_character = 20 if automatic_requests == 1 else 21
                assert message["params"]["position"] == {
                    "line": 0, "character": expected_character
                }
                if automatic_requests == 1:
                    (root / "automatic-completion-held").write_text("held")
                    request_id = message["id"]

                    def release_held_completion(request_id=request_id):
                        until = time.monotonic() + 5
                        while not (root / "release-automatic-completion").is_file():
                            assert time.monotonic() < until
                            time.sleep(.005)
                        send({"jsonrpc": "2.0", "id": request_id, "result": {
                            "isIncomplete": False,
                            "items": [{"label": "auto-stale", "kind": 6,
                                       "insertText": "STALE", "insertTextFormat": 1}]
                        }})

                    threading.Thread(target=release_held_completion, daemon=True).start()
                    continue
                items = [
                    {"label": "auto-fresh", "kind": 6,
                     "insertText": "AUTO", "insertTextFormat": 1}
                ]
                send({"jsonrpc": "2.0", "id": message["id"], "result": {
                    "isIncomplete": False,
                    "items": items
                }})
                continue
            assert message["params"]["position"] == {"line": 0, "character": 0}
            if mode in ("completion_console_retirement",
                        "completion_test_cases_retirement"):
                (root / "completion-request-pending").write_text("ready")
                until = time.monotonic() + 2.5
                while not (root / "release-completion-response").exists():
                    assert time.monotonic() < until
                    time.sleep(.005)
            if mode in ("completion_navigation", "completion_console_retirement",
                        "completion_test_cases_retirement"):
                items = [
                    {"label": f"item-{index}", "kind": 6,
                     "insertText": chr(ord("A") + index), "insertTextFormat": 1}
                    for index in range(4)
                ]
            else:
                items = [
                    {"label": "malicious\u001b[2J\nlabel", "kind": 6,
                     "insertText": "\u03bb", "insertTextFormat": 1},
                    {"label": "unsupported snippet", "kind": 3,
                     "insertText": "${1:no}", "insertTextFormat": 2},
                ]
            send({"jsonrpc": "2.0", "id": message["id"], "result": {
                "isIncomplete": False,
                "items": items
            }})
            if mode in ("completion_console_retirement",
                        "completion_test_cases_retirement"):
                (root / "completion-response-sent").write_text("sent")
        elif method == "shutdown" and mode == "quit_command":
            marker = root.parent / "assignment.work/.rustrace/command-activity.json"
            (root / "shutdown-command-state.json").write_text(marker.read_text())
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "shutdown" and mode == "blocked":
            time.sleep(60)
        elif method == "shutdown" and mode == "descendant":
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
            subprocess.Popen(["/bin/sleep", "60"])
        elif method == "rust-analyzer/reloadWorkspace" and mode == "mutate_reload":
            workspace = root.parent / "assignment.work"
            (workspace / "main.rs").write_text("SERVER MUTATION\n")
            (root / "mutation-complete").write_text("reload")
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "shutdown" and mode == "mutate_shutdown":
            workspace = root.parent / "assignment.work"
            (workspace / "main.rs").write_text("SERVER MUTATION\n")
            (root / "mutation-complete").write_text("shutdown")
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method in ("shutdown", "rust-analyzer/reloadWorkspace"):
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "exit":
            if mode == "mutate_exit":
                workspace = root.parent / "assignment.work"
                (workspace / "main.rs").write_text("SERVER MUTATION\n")
                (root / "mutation-complete").write_text("exit")
            return
        elif (method == "rustrace/commandBoundary"
              and mode == "command_quiescence" and launch_count == 0):
            send({"jsonrpc": "2.0", "id": message["id"], "error": {
                "code": -32601, "message": "fixture boundary acknowledgement"}})
            until = time.monotonic() + 15
            while not (root / "command-running").is_file():
                assert time.monotonic() < until
                time.sleep(.005)
            marker_path = root.parent / "assignment.work/.rustrace/command-activity.json"
            marker = json.loads(marker_path.read_text())
            assert marker["active"] is True
            workspace = root.parent / "assignment.work"
            before = (workspace / "main.rs").read_bytes()
            mutated = b"SERVER DURING COMMAND\n"
            (workspace / "main.rs").write_bytes(mutated)
            record(root / "mutation.jsonl", {
                "command_marker": marker,
                "before_sha256": hashlib.sha256(before).hexdigest(),
                "after_sha256": hashlib.sha256(mutated).hexdigest(),
                "server_pid": os.getpid(),
            })
            (root / "server-mutated").write_text("mutation completed after command start")
        elif method is None and mode == "requests" and message.get("id") == "apply":
            assert message.get("error", {}).get("code") == -32601
            send({"jsonrpc": "2.0", "id": "execute", "method": "workspace/executeCommand",
                  "params": {"command": "forbidden"}})
            request_step = 2
        elif method is None and mode == "requests" and message.get("id") == "execute":
            assert message.get("error", {}).get("code") == -32601
            send({"jsonrpc": "2.0", "id": "config", "method": "workspace/configuration",
                  "params": {"items": [{"section": "rust-analyzer"}]}})
            request_step = 3
        elif method is None and mode == "requests" and message.get("id") == "config":
            assert isinstance(message.get("result"), list) and len(message["result"]) == 1
            (root / "requests-complete").write_text(str(request_step))
        elif "id" in message and method is not None:
            send({"jsonrpc": "2.0", "id": message["id"], "error": {
                "code": -32601, "message": "unsupported fixture request"}})


root = pathlib.Path(__file__).resolve().parent
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
mode = (root / "mode.txt").read_text().strip()
controlled = ["RUSTUP_AUTO_INSTALL", "RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "RUSTDOC",
              "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_ENCODED_RUSTFLAGS",
              "CARGO_ENCODED_RUSTDOCFLAGS", "CARGO_NET_OFFLINE", "CARGO_TARGET_DIR",
              "CARGO_BUILD_BUILD_DIR", "CARGO_TERM_COLOR", "CARGO_TERM_PROGRESS_WHEN"]
forbidden = ["RUST_ANALYZER", "RUSTFLAGS", "RUSTDOCFLAGS", "RA_LOG", "RUSTUP_LOG",
             "CARGO_BUILD_RUSTC_WRAPPER", "RUSTRACE_SECRET_SENTINEL"]
record(root / "calls.jsonl", {
    "tool": name,
    "args": args,
    "controlled_env": {key: os.environ.get(key) for key in controlled if key in os.environ},
    "forbidden_present": [key for key in forbidden if key in os.environ],
})
assert os.environ.get("RUSTUP_AUTO_INSTALL") == "0", "auto-install not disabled"
if name == "rustup":
    if args == ["--version"]:
        if mode == "resolve_blocked":
            calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
            if sum(call["tool"] == "rustup" and call["args"] == ["--version"]
                   for call in calls) >= 2:
                time.sleep(60)
        print("rustup 1.28.2 (fixture)")
    elif args == ["toolchain", "list"]:
        print("pinned")
    elif args == ["show", "active-toolchain"]:
        assert os.environ.get("RUSTUP_TOOLCHAIN") == "pinned"
        print("pinned (environment override)")
    elif len(args) == 4 and args[:3] == ["which", "--toolchain", "pinned"]:
        tool = root / args[3]
        if not tool.is_file():
            sys.exit(1)
        print(tool)
    elif len(args) >= 3 and args[:2] == ["run", "pinned"]:
        tool = pathlib.Path(args[2])
        assert tool.is_absolute() and tool.parent == root and tool.is_file()
        os.execv(str(tool), args[2:])
    else:
        raise AssertionError("unexpected rustup command: " + repr(args))
elif name == "rustc" and args == ["-vV"]:
    print("rustc 1.98.1 (fixture)\nrelease: 1.98.1\nhost: fixture-host")
elif name == "cargo":
    if args == ["-V"]:
        marker = root.parent / "assignment.work/.rustrace/command-activity.json"
        command_probe = ((root / "server-launches.jsonl").is_file()
                         and marker.is_file()
                         and json.loads(marker.read_text())["active"])
        if mode in ("command_failed", "command_cancelled") and command_probe:
            record_command_server_state(root)
            (root / "command-resolution-probe").write_text(str(os.getpid()))
            if mode == "command_failed":
                sys.exit(7)
            time.sleep(60)
        print("cargo 1.98.1 (fixture)")
    elif mode in ("quit_command", "command_barrier") and args == [
            "check", "--message-format=json", "--locked"]:
        assert sys.stdin.buffer.read(1) == b""
        record_command_server_state(root)
        (root / "command-running").write_text(str(os.getpid()))
        time.sleep(60)
    elif mode == "command_quiescence" and args == [
            "check", "--message-format=json", "--locked"]:
        record_command_server_state(root)
        (root / "command-running").write_text(str(os.getpid()))
        until = time.monotonic() + .75
        while not (root / "server-mutated").is_file() and time.monotonic() < until:
            time.sleep(.005)
        observed = (root.parent / "assignment.work/main.rs").read_bytes()
        (root / "command-observed.bin").write_bytes(observed)
        print("command observed sha256=" + hashlib.sha256(observed).hexdigest())
    else:
        raise AssertionError("unexpected cargo invocation: " + repr(args))
elif name == "rustdoc" and args in (["--version"], ["-V"]):
    print("rustdoc 1.98.1 (fixture)")
elif name == "rust-analyzer" and args == ["--version"]:
    if mode == "mutate_resolution":
        calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
        if sum(call["tool"] == "rust-analyzer" and call["args"] == ["--version"]
               for call in calls) == 2:
            workspace = root.parent / "assignment.work"
            (workspace / "main.rs").write_text("SERVER MUTATION\n")
            (root / "mutation-complete").write_text("resolution")
    print("rust-analyzer 1.98.1 (fixture)")
elif name == "rust-analyzer" and not args:
    server(root, mode)
else:
    raise AssertionError("unexpected tool invocation: " + repr((name, args)))
