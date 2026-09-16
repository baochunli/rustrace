"""No-install bootstrap regressions: source-modeled legacy and real modern rustup."""
import io
import json
import os
import pathlib
import select
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile

from test_home import isolate
isolate()

mode, test_binary, binary = sys.argv[1:]
real_rustup = shutil.which("rustup")
assert real_rustup

with tempfile.TemporaryDirectory(prefix="rustrace-bootstrap-") as temporary:
    root = pathlib.Path(temporary).resolve()
    workspace = root / "workspace"
    workspace.mkdir()
    home = root / "rustup-home"
    home.mkdir()
    settings = b'version = "12"\ndefault_toolchain = "stable"\n'
    (home / "settings.toml").write_bytes(settings)
    env = {**os.environ, "RUSTUP_HOME": str(home), "RUSTUP_AUTO_INSTALL": "1"}
    env.pop("RUSTUP_TOOLCHAIN", None)
    env.pop("RUSTRACE_DISCOVERY_TEST_PIN", None)

    def discover(pin=None, override=None):
        child_env = {**env, "RUSTRACE_DISCOVERY_TEST_ROOT": str(workspace),
                     "RUSTRACE_DISCOVERY_TEST_OUTPUT": str(root / "report.json")}
        if pin:
            child_env["RUSTRACE_DISCOVERY_TEST_PIN"] = pin
        if override:
            child_env["RUSTUP_TOOLCHAIN"] = override
        result = subprocess.run([test_binary, "--exact", "isolated_discovery_child"],
                                env=child_env, cwd=workspace, capture_output=True, timeout=25)
        assert result.returncode == 0, (result.stdout, result.stderr)
        return json.loads((root / "report.json").read_bytes())

    if mode == "legacy":
        launcher = root / "rustup"
        # Model the source-traced legacy resolver: official selections attempt
        # installation even with AUTO_INSTALL=0; custom selections never do.
        # Record a local marker instead of performing network or installation.
        launcher.write_text("#!" + sys.executable + "\n" + '''
import json, os, pathlib, re, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
selection = os.environ.get("RUSTUP_TOOLCHAIN")
if not selection:
    file = pathlib.Path.cwd() / "rust-toolchain"
    selection = file.read_text().strip() if file.exists() else "stable"
with (root / "calls").open("a") as calls:
    calls.write(json.dumps([args, selection, os.environ.get("RUSTUP_AUTO_INSTALL")]) + "\\n")
if args == ["--version"]:
    if re.match(r"^(stable|beta|nightly|[0-9]+\\.[0-9]+)", selection):
        (root / "install-attempt").write_text(selection)
    print("rustup " + (root / "manager-version").read_text().strip() + " (fixture)")
else:
    print("legacy reached normal discovery", file=sys.stderr)
    sys.exit(9)
''')
        launcher.chmod(0o700)
        env["PATH"] = str(root)
        failures = []
        for version in ["1.27.1", "1.28.0", "1.28.1-beta.1", "1.28", "9.invalid.0"]:
            (root / "manager-version").write_text(version)
            for source in ["assignment", "environment", "directory", "default"]:
                (root / "calls").unlink(missing_ok=True)
                (root / "install-attempt").unlink(missing_ok=True)
                file = workspace / "rust-toolchain"
                file.unlink(missing_ok=True)
                if source == "directory":
                    file.write_text("beta\n")
                report = discover("1.98.1" if source == "assignment" else None,
                                  "nightly" if source == "environment" else None)
                calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
                if (root / "install-attempt").exists():
                    failures.append(f"{version}/{source}: first probe exposed an official installation selection")
                if len(calls) != 1 or calls[0][0] != ["--version"] or calls[0][2] != "0":
                    failures.append(f"{version}/{source}: continued past unsupported manager: {calls}")
                probe = report["probes"][0]
                if probe["status"] == "available" or "1.28.1" not in probe["remediation"]:
                    failures.append(f"{version}/{source}: no unsupported-manager/manual-update blocker")

        # The real work entry must persist the failure and preserve inspection.
        (root / "manager-version").write_text("1.27.1")
        manifest = b'''format_version = 1
course_id = "course"
assignment_id = "bootstrap"
assignment_version = "v1"
title = "Bootstrap"
toolchain = "1.98.1"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''
        archive = root / "bootstrap.rta"
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for name, data in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"A")]:
                info = tarfile.TarInfo(name)
                info.size, info.mode = len(data), 0o600
                tar.addfile(info, io.BytesIO(data))
        data = stream.getvalue()
        while data.endswith(bytes(512)):
            data = data[:-512]
        archive.write_bytes(data + bytes(1024))
        result = subprocess.run([binary, "work", str(archive)], cwd=root, env=env,
                                capture_output=True, timeout=25)
        if result.returncode == 0 or b"1.28.1" not in result.stdout + result.stderr:
            failures.append(f"production did not report manual manager update: {result.stdout!r} {result.stderr!r}")
        observations = list(root.glob("**/toolchain-*.json"))
        if not observations or json.loads(observations[0].read_bytes())["report"]["probes"][0]["status"] == "available":
            failures.append("production did not persist unsupported bootstrap evidence")
        inspected = subprocess.run([binary, "work", str(archive), "--inspect"], cwd=root,
                                   env=env, capture_output=True, timeout=25)
        if inspected.returncode != 0:
            failures.append(f"preserved assignment inspection failed: {inspected.stderr!r}")
        assert not failures, "\n".join(failures)
        print("Legacy/bootstrap: assignment, inherited environment, directory and default are shielded; unsupported versions stop before discovery; real work persists blocker and inspection works")
    else:
        assert mode == "modern"
        # A local listener traps every distribution/update request without
        # contacting public servers, serving bytes or installing anything.
        with socket.socket() as trap:
            trap.bind(("127.0.0.1", 0))
            trap.listen()
            url = f"http://127.0.0.1:{trap.getsockname()[1]}"
            env.update(RUSTUP_DIST_SERVER=url, RUSTUP_UPDATE_ROOT=url,
                       RUSTUP_DIST_ROOT=url, PATH=str(pathlib.Path(real_rustup).parent),
                       NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]:
                env.pop(key, None)
            for source in ["assignment", "environment", "directory", "default"]:
                file = workspace / "rust-toolchain"
                file.unlink(missing_ok=True)
                if source == "directory":
                    file.write_text("beta\n")
                report = discover("1.98.1" if source == "assignment" else None,
                                  "nightly" if source == "environment" else None)
                assert report["probes"][0]["status"] == "available", report
                assert any(p["required"] and p["status"] != "available" for p in report["probes"]), report
                assert not any(p["argv"][1:2] == ["run"] for p in report["probes"]), report
                assert not select.select([trap], [], [], 0)[0], f"{source}: network request attempted"
                assert not (home / "toolchains").exists() or not list((home / "toolchains").iterdir())
                assert (home / "settings.toml").read_bytes() == settings
        print("Installed modern rustup: all missing official selection sources block without distribution/update requests, installed toolchains or settings changes")
