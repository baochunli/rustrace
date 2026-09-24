#!/usr/bin/env python3
"""Trusted current-tool resolver/launcher used only in disposable runner tests."""
import json
import os
import pathlib
import subprocess
import sys
import time


def language_server(root):
    target = root / "target"
    target.mkdir(exist_ok=True)
    with (target / "lsp-launches.jsonl").open("a") as stream:
        stream.write(json.dumps({"pid": os.getpid()}) + "\n")

    def send(value):
        body = json.dumps(value, separators=(",", ":")).encode()
        os.write(1, f"Content-Length: {len(body)}\r\n\r\n".encode() + body)

    while True:
        header = bytearray()
        while not header.endswith(b"\r\n\r\n"):
            byte = os.read(0, 1)
            if not byte:
                return
            header.extend(byte)
            assert len(header) <= 8192
        lengths = [int(line.split(b":", 1)[1]) for line in header.split(b"\r\n")
                   if line.lower().startswith(b"content-length:")]
        assert len(lengths) == 1 and lengths[0] <= 8 * 1024 * 1024
        body = bytearray()
        while len(body) < lengths[0]:
            block = os.read(0, lengths[0] - len(body))
            assert block
            body.extend(block)
        message = json.loads(body)
        method = message.get("method")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": message["id"], "result": {
                "capabilities": {"positionEncoding": "utf-16",
                                 "textDocumentSync": {"openClose": True, "change": 2}},
                "serverInfo": {"name": "console-session-fixture"}}})
        elif method == "shutdown":
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
        elif method == "exit":
            return

program = pathlib.Path(__file__).name
root = pathlib.Path.cwd()
config_path = root / "target" / "runner-fixture.json"
config = json.loads(config_path.read_bytes()) if config_path.exists() else {}
args = sys.argv[1:]
if program == "rustup":
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    if args == ["--version"]:
        assert os.environ["RUSTUP_TOOLCHAIN"] == "rustrace-discovery-bootstrap"
        if config.get("mode") == "preparing_drop":
            time.sleep(20)
        if config.get("mode") == "probe_invalid":
            os.write(1, b"\xff\0")
            sys.exit(0)
        print("rustup " + config.get("manager", "1.28.1") + " (fixture)")
    elif args == ["toolchain", "list"]:
        print("fixture (default)")
    elif args == ["show", "active-toolchain"]:
        assert os.environ["RUSTUP_TOOLCHAIN"] == "fixture"
        print("fixture (environment override)")
    elif len(args) == 4 and args[:3] == ["which", "--toolchain", "fixture"]:
        if args[3] in config.get("missing", []):
            sys.exit(1)
        tool = pathlib.Path(__file__).resolve().parent / config.get("generation", "v1") / args[3]
        assert tool.is_file(), tool
        print(tool)
    elif len(args) >= 3 and args[:2] == ["run", "fixture"]:
        os.execv(args[2], args[2:])
    else:
        raise AssertionError("unsupported fixture rustup invocation")
elif args in (["-vV"], ["-V"], ["--version"], ["clippy", "-V"], ["fmt", "--version"]):
    name = {"cargo-clippy": "clippy", "cargo-fmt": "rustfmt"}.get(program, program)
    suffix = "x" * 5000 if config.get("mode") == "version_oversize" and program == "rustdoc" else ""
    print(name + " 1.98.1 (fixture-" + pathlib.Path(__file__).parent.name + ")" + suffix)
    if program == "rustc":
        print("host: aarch64-apple-darwin\nrelease: 1.98.1")
elif program == "rust-analyzer" and not args:
    language_server(root)
else:
    assert program in ["cargo", "cargo-clippy", "cargo-fmt"]
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    assert os.environ["RUSTUP_TOOLCHAIN"] == "fixture"
    assert "CARGO_NET_OFFLINE" not in os.environ
    assert os.environ["RUSTC_WRAPPER"] == ""
    assert os.environ["RUSTC_WORKSPACE_WRAPPER"] == ""
    assert os.environ["CARGO_ENCODED_RUSTFLAGS"] == ""
    assert "UNRELATED_SECRET" not in os.environ
    assert "RUSTFLAGS" not in os.environ
    assert "RUSTUP_LOG" not in os.environ
    assert args[0] in ["check", "test", "run", "clippy", "fmt", "doc"]
    console = str(config.get("mode", "")).startswith("console")
    if console:
        assert args in [[name, "--locked"]
                        for name in ["check", "test", "clippy", "doc"]] + [
                            ["run", "--locked"], ["run", "--release", "--locked"]]
    else:
        assert os.read(0, 1) == b""
        assert args == ["fmt"] or args[1:3] == ["--message-format=json", "--locked"]
    target = root / "target"
    target.mkdir(exist_ok=True)
    (target / "invocation.json").write_text(json.dumps({"program": str(pathlib.Path(__file__).resolve()),
        "argv": args, "keys": sorted(os.environ), "cwd": str(root)}))
    mode = config.get("mode")
    if mode is None and program == "cargo-fmt" and (root / "Cargo.lock").exists():
        mode = (root / "Cargo.lock").read_text().strip()
    mode = mode or "bytes"
    if console and mode == "console_exit_early":
        os.write(1, b"console-exited-before-stdin")
        sys.exit(config.get("exit", 0))
    elif console and mode == "console_hang":
        # A student program stuck in a loop: it outlives a killed session.
        (target / "hung-pid").write_text(str(os.getpid()))
        while True:
            time.sleep(60)
    elif console and mode == "console_io":
        value = b""
        while True:
            block = os.read(0, 8192)
            if not block:
                break
            value += block
        if config.get("mutate_source"):
            (root / "main.rs").write_bytes(b"rejected console source")
        if config.get("delay_after_input_millis"):
            (target / "runner-input-read").write_text("ready")
            time.sleep(config["delay_after_input_millis"] / 1000)
        os.write(1, b"stdout:" + value if config.get("echo", True) else b"console-complete")
        os.write(2, b"console-stderr")
        sys.exit(config.get("exit", 0))
    elif program == "cargo-fmt":
        if mode == "format_rejected":
            (root / "main.rs").write_text("fn main() {}\n")
            (root / "Cargo.lock").write_bytes(b"formatter must not own this")
        else:
            for source in sorted(root.rglob("*.rs")):
                contents = source.read_bytes()
                for before, after in [(b"formatter fixture", b"Formatter fixture"),
                                      (b"fixture seed", b"Fixture seed")]:
                    if before in contents:
                        source.write_bytes(contents.replace(before, after, 1))
                        break
                else:
                    continue
                break
    elif mode == "diagnostic_tints":
        target = {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": str(root / "main.rs"), "edition": "2024"
        }
        def diagnostic(level, code, message, start, end, line):
            span = {
                "file_name": "main.rs", "byte_start": start, "byte_end": end,
                "line_start": line, "line_end": line,
                "column_start": 1, "column_end": end - start + 1,
                "is_primary": True, "text": [], "label": None,
                "suggested_replacement": None, "suggestion_applicability": None,
                "expansion": None,
            }
            return {
                "reason": "compiler-message", "package_id": "student 0.1.0",
                "manifest_path": str(root / "Cargo.toml"), "target": target,
                "message": {"rendered": f"{level}: {message}\n", "message": message,
                    "code": {"code": code, "explanation": None}, "level": level,
                    "spans": [span], "children": []},
            }
        for message in [
            diagnostic("warning", "W0001", "warning tint", 0, 7, 1),
            diagnostic("error", "E0308", "error tint", 8, 13, 2),
        ]:
            print(json.dumps(message, separators=(",", ":")))
        print(json.dumps({"reason":"build-finished", "success":False}, separators=(",", ":")))
        sys.exit(config.get("exit", 101))
    elif mode in ["diagnostic_filter", "warning_success"]:
        target = {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": str(root / "main.rs"), "edition": "2024"
        }
        span = {
            "file_name": "main.rs", "byte_start": 0, "byte_end": 1,
            "line_start": 1, "line_end": 1,
            "column_start": 1, "column_end": 2,
            "is_primary": True, "text": [], "label": None,
            "suggested_replacement": None, "suggestion_applicability": None,
            "expansion": None,
        }

        def diagnostic(level, message, spans=None, children=None):
            return {
                "reason": "compiler-message", "package_id": "student 0.1.0",
                "manifest_path": str(root / "Cargo.toml"), "target": target,
                "message": {
                    "rendered": f"{level}: {message}\n", "message": message,
                    "code": None, "level": level, "spans": spans or [],
                    "children": children or [],
                },
            }

        child_note = {
            "message": "spanned error child note stays in evidence",
            "code": None, "level": "note", "spans": [], "children": [],
            "rendered": None,
        }
        if mode == "warning_success":
            messages = [diagnostic("warning", "successful warning", [span])]
        else:
            messages = [
                diagnostic("error", "real spanned error", [span], [child_note]),
                diagnostic("warning", "real spanned warning", [span]),
                diagnostic("note", "real spanned note", [span]),
                diagnostic("failure-note", "could not compile `student`"),
                diagnostic("note", "For more information about this error, try `rustc --explain E0308`."),
                diagnostic("help", "aborting due to 1 previous error"),
                diagnostic("note", "Some errors have detailed explanations: E0308."),
            ]
        for message in messages:
            print(json.dumps(message, separators=(",", ":")))
        success = mode == "warning_success"
        print(json.dumps({"reason": "build-finished", "success": success}, separators=(",", ":")))
        sys.exit(config.get("exit", 0 if success else 101))
    elif mode == "diagnostic":
        target = {
            "kind": ["bin"], "crate_types": ["bin"], "name": "student",
            "src_path": str(root / "main.rs"), "edition": "2024"
        }
        def diagnostic(message, spans):
            return {
                "reason": "compiler-message", "package_id": "student 0.1.0",
                "manifest_path": str(root / "Cargo.toml"), "target": target,
                "message": {"rendered": "error: " + message + "\n", "message": message,
                    "code": {"code": "E0308", "explanation": None}, "level": "error",
                    "spans": spans, "children": []}
            }
        def span(path, start, end, line_start=1, line_end=1, column_start=1, column_end=2):
            return {"file_name": path, "byte_start": start, "byte_end": end,
                "line_start": line_start, "line_end": line_end,
                "column_start": column_start, "column_end": column_end,
                "is_primary": True, "text": [], "label": None,
                "suggested_replacement": None, "suggestion_applicability": None,
                "expansion": None}
        messages = [
            diagnostic("valid Unicode byte span", [span("main.rs", 3, 7, column_start=3, column_end=4)]),
            diagnostic("managed but not editable", [span("Cargo.lock", 0, 1)]),
            diagnostic("outside workspace", [span("../foreign.rs", 0, 1)]),
            diagnostic("invalid coordinates", [span("main.rs", 999, 1000)]),
            diagnostic("missing target span", []),
        ]
        for message in messages:
            print(json.dumps(message, ensure_ascii=False, separators=(",", ":")))
        print(json.dumps({"reason":"build-finished", "success":False}, separators=(",", ":")))
        sys.exit(config.get("exit", 101))
    elif mode == "source":
        (root / config.get("source_path", "main.rs")).write_bytes(b"rejected command source")
        if config.get("change_lock"):
            (root / "Cargo.lock").write_bytes(b"rejected command lock")
    elif mode == "lock":
        (root / "Cargo.lock").write_bytes(b"rejected lock change")
    elif mode == "unsafe":
        (root / "main.rs").unlink()
        (root / "main.rs").symlink_to(target / "invocation.json")
    elif mode == "out_of_policy":
        (root / "foreign.dat").write_bytes(b"preserve this")
    elif mode == "oversize":
        (root / "main.rs").write_bytes(b"x" * (1024 * 1024 + 1))
    elif mode == "boundary_failure":
        (root / ".rustrace" / "unexpected-directory").mkdir()
    elif mode == "metadata_failure":
        (root / ".rustrace" / "manifest.toml").write_bytes(b"tampered metadata")
    elif mode == "capture_collision":
        (root / ".rustrace" / "command-4-capture.json").write_bytes(b"existing unowned artifact")
    elif mode == "short_sleep":
        time.sleep(0.25)
    elif mode == "sleep":
        time.sleep(20)
    elif mode == "huge":
        while True:
            os.write(1, bytes(range(256)) * 32)
            os.write(2, bytes(range(256)) * 32)
    elif mode == "missing":
        sys.exit(config.get("exit", 0))
    elif mode == "read_failed":
        escaped = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(3.2)"],
            start_new_session=True,
        )
        (target / "escaped-pid").write_text(str(escaped.pid))
        sys.exit(config.get("exit", 0))
    os.write(1, b"stdout\xff\0\x1b]52;c;fixture\x07\n")
    os.write(2, b"stderr\xfe\n")
    sys.exit(config.get("exit", 0))
