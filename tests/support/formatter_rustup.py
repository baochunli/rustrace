#!/usr/bin/env python3
"""Trusted resolver/formatter used only in disposable T4.5 tests."""
import os
import pathlib
import sys
import time

program = pathlib.Path(__file__).name
root = pathlib.Path.cwd()
mode = (root / "Cargo.lock").read_text().strip() if (root / "Cargo.lock").exists() else "noop"
args = sys.argv[1:]

if program == "rustup":
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    if args == ["--version"]:
        assert os.environ["RUSTUP_TOOLCHAIN"] == "rustrace-discovery-bootstrap"
        print("rustup 1.28.1 (formatter fixture)")
    elif args == ["toolchain", "list"]:
        print("fixture (default)")
    elif args == ["show", "active-toolchain"]:
        print("fixture (environment override)")
    elif len(args) == 4 and args[:3] == ["which", "--toolchain", "fixture"]:
        if mode == "missing_formatter" and args[3] in ["cargo-fmt", "rustfmt"]:
            sys.exit(1)
        target = pathlib.Path(__file__).resolve().parent / "v1" / args[3]
        assert target.is_file(), target
        print(target)
    elif len(args) >= 3 and args[:2] == ["run", "fixture"]:
        os.execv(args[2], args[2:])
    else:
        raise AssertionError(f"unsupported rustup invocation: {args}")
elif args in (["-vV"], ["-V"], ["--version"], ["fmt", "--version"]):
    name = "rustfmt" if program == "cargo-fmt" else program
    print(f"{name} 1.98.1 (formatter fixture)")
    if program == "rustc":
        print("host: aarch64-apple-darwin\nrelease: 1.98.1")
else:
    assert program == "cargo-fmt"
    assert args == ["fmt"]
    assert os.read(0, 1) == b""
    assert not (root / ".rustrace").exists(), "formatter received the live session directory"
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    assert os.environ["RUSTUP_TOOLCHAIN"] == "fixture"
    assert "CARGO_NET_OFFLINE" not in os.environ
    assert os.environ["RUSTFMT"].endswith("/rustfmt")
    if mode in ["changed", "multiple", "nonzero", "lock_change", "create", "delete", "rename", "cancel_partial", "external_during", "metadata_during"]:
        (root / "main.rs").write_text("fn main() {}\n")
    if mode == "multiple":
        (root / "other.rs").write_text("pub fn answer() -> u8 { 42 }\n")
    elif mode == "lock_change":
        (root / "Cargo.lock").write_text("formatter must not own this")
    elif mode == "create":
        (root / "created.rs").write_text("pub fn created() {}\n")
    elif mode == "delete":
        (root / "other.rs").unlink()
    elif mode == "rename":
        (root / "other.rs").rename(root / "renamed.rs")
    elif mode == "invalid_utf8":
        (root / "main.rs").write_bytes(b"\xff")
    elif mode == "oversize":
        (root / "main.rs").write_bytes(b"x" * (1024 * 1024 + 1))
    elif mode == "transaction_oversize":
        (root / "main.rs").write_text("x" * (300 * 1024))
    elif mode in ["cancel_partial", "external_during", "metadata_during"]:
        live_target = pathlib.Path(__file__).resolve().parents[2]
        (live_target / "format-started").write_text(mode)
        if mode in ["external_during", "metadata_during"]:
            deadline = time.monotonic() + 15
            while not (live_target / "format-continue").exists() and time.monotonic() < deadline:
                time.sleep(.01)
            assert (live_target / "format-continue").exists()
        else:
            time.sleep(20)
    print("formatter fixture output")
    sys.exit(7 if mode == "nonzero" else 0)
