#!/usr/bin/env python3
"""Trusted Cargo dependency fixture for disposable controller tests."""
import os
import pathlib
import sys

program = pathlib.Path(__file__).name
root = pathlib.Path.cwd()
args = sys.argv[1:]

if program == "rustup":
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    if args == ["--version"]:
        assert os.environ["RUSTUP_TOOLCHAIN"] == "rustrace-discovery-bootstrap"
        print("rustup 1.28.1 (dependency fixture)")
    elif args == ["toolchain", "list"]:
        print("fixture (default)")
    elif args == ["show", "active-toolchain"]:
        print("fixture (environment override)")
    elif len(args) == 4 and args[:3] == ["which", "--toolchain", "fixture"]:
        target = pathlib.Path(__file__).resolve().parent / "v1" / args[3]
        assert target.is_file(), target
        print(target)
    elif len(args) >= 3 and args[:2] == ["run", "fixture"]:
        os.execv(args[2], args[2:])
    else:
        raise AssertionError(f"unsupported rustup invocation: {args}")
elif args in (["-vV"], ["-V"], ["--version"]):
    print(f"{program} 1.98.1 (dependency fixture)")
    if program == "rustc":
        print("host: aarch64-apple-darwin\nrelease: 1.98.1")
else:
    assert program == "cargo", (program, args)
    assert os.read(0, 1) == b""
    assert not (root / ".rustrace").exists(), "dependency tool received live state"
    assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
    assert os.environ["RUSTUP_TOOLCHAIN"] == "fixture"
    assert "CARGO_NET_OFFLINE" not in os.environ
    assert pathlib.Path(os.environ["CARGO_TARGET_DIR"]).resolve() == (root / "target").resolve()
    for name in [
        "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS", "CARGO_REGISTRIES_CRATES_IO_TOKEN",
    ]:
        assert not os.environ.get(name), (name, os.environ.get(name))

    live_root = pathlib.Path(__file__).resolve().parents[3]
    mode_path = live_root / "target" / "dependency-mode"
    mode = mode_path.read_text().strip() if mode_path.exists() else "normal"
    manifest = root / "Cargo.toml"
    lockfile = root / "Cargo.lock"
    if args == ["add", "serde@1.0.229"]:
        manifest.write_text(manifest.read_text() + 'serde = "1.0.229"\n')
        lockfile.write_text("lock-after-add\n")
    elif args == ["remove", "serde"]:
        manifest.write_text(manifest.read_text().replace('serde = "1.0.229"\n', ""))
        lockfile.write_text("lock-after-remove\n")
    elif args == ["update"]:
        lockfile.write_text("lock-after-update\n")
    else:
        raise AssertionError(f"unsupported cargo invocation: {args}")

    if mode == "failure":
        sys.exit(7)
    if mode == "unauthorized":
        (root / "src/main.rs").write_text("fn unauthorized() {}\n")
    print("dependency fixture output")
