"""Real production rustup logging regression using only synthetic credentials."""
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

binary = sys.argv[1]
rustup = shutil.which("rustup")
assert rustup, "installed rustup is required by the contributor matrix"


def package(path, pin):
    manifest = f'''format_version = 1
course_id = "course"
assignment_id = "logging"
assignment_version = "v1"
title = "Logging"
toolchain = "{pin}"
edition = "2024"
allowed_paths = ["*.rs", "Cargo.toml"]
[commands]
check = ["cargo", "check"]
test = ["cargo", "test"]
run = ["cargo", "run"]
clippy = ["cargo", "clippy"]
format = ["cargo", "fmt"]
'''.encode()
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in [("assignment.toml", manifest), ("starter/Cargo.toml", b'[package]\nname = "fixture"\nversion = "0.1.0"\n[workspace]\n'), ("starter/main.rs", b"A")]:
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(data), 0o600
            archive.addfile(info, io.BytesIO(data))
    data = stream.getvalue()
    while data.endswith(bytes(512)):
        data = data[:-512]
    path.write_bytes(data + bytes(1024))


with tempfile.TemporaryDirectory(prefix="rustrace-logging-") as temporary, socket.socket() as trap:
    root = pathlib.Path(temporary).resolve()
    trap.bind(("127.0.0.1", 0))
    trap.listen()
    # Inherit only installation locations, never real logging/server/proxy or
    # credential settings. Do not print raw environment or captured probe data.
    env = {key: os.environ[key] for key in ["HOME", "RUSTUP_HOME", "CARGO_HOME"] if key in os.environ}
    env.update(PATH=str(pathlib.Path(rustup).parent) + os.pathsep + os.defpath,
               RUSTUP_AUTO_INSTALL="0", NO_PROXY="127.0.0.1,localhost",
               no_proxy="127.0.0.1,localhost",
               XDG_CONFIG_HOME=str(root / "xdg-config"),
               XDG_STATE_HOME=str(root / "xdg-state"))
    # This driver isolates toolchain networking; app checks have their own suite.
    update_state = root / "xdg-state/rustrace/update-state.json"
    update_state.parent.mkdir(parents=True)
    update_state.write_text(json.dumps({"schema_version": 1, "checks_enabled": False,
                                       "last_attempt": None, "last_success": None,
                                       "next_eligible": None, "latest": None}))
    failures = []
    prior = {}
    selected = None
    archive = root / "logging.rta"
    package(archive, "1.98.1")
    for mode in [None, "trace", "debug"]:
        sentinel = "synthetic-rustrace-credential-" + (mode or "control")
        env["RUSTUP_DIST_SERVER"] = f"http://synthetic-user:{sentinel}@127.0.0.1:{trap.getsockname()[1]}"
        env["RUSTUP_UPDATE_ROOT"] = env["RUSTUP_DIST_SERVER"]
        if mode:
            env["RUSTUP_LOG"] = mode
        result = subprocess.run([binary, "work", str(archive)] + (["--resume"] if prior else []),
                                cwd=root, env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=30)
        state = root / "logging.work/.rustrace"
        observations = sorted(state.glob("toolchain-*.json"))
        assert observations, "production must publish tool evidence before non-PTY terminal startup"
        fresh = observations[-1]
        assert fresh not in prior, "resume must publish a new observation"
        assert all(path.read_bytes() == data for path, data in prior.items()), "prior metadata changed"
        raw = fresh.read_bytes()
        report = json.loads(raw)["report"]
        if sentinel.encode() in raw + result.stdout + result.stderr:
            failures.append(f"{mode or 'control'}: synthetic credential persisted or displayed by production")
        assert not select.select([trap], [], [], 0)[0], "distribution/update connection attempted"
        if selected is None:
            selected = report["selected_toolchain"]
        assert selected and report["selected_toolchain"] == selected, "actual pin selection changed"
        for component in ["rustc", "cargo"]:
            probe = next(p for p in report["probes"] if p["component"] == component and p["purpose"] == "version")
            assert probe["status"] == "available" and probe["stdout"].startswith(component + " "), "actual tool version unavailable"
            assert probe["argv"][1:3] == ["run", selected] and pathlib.Path(probe["argv"][3]).is_absolute(), "exact executable evidence lost"
        prior.update({fresh: raw, state / "session.json": (state / "session.json").read_bytes()})

    # Normal actionable required-tool diagnostics must survive logging removal.
    missing = root / "missing.rta"
    package(missing, "rustrace-m1-missing-" + str(os.getpid()))
    env["RUSTUP_LOG"] = "trace"
    result = subprocess.run([binary, "work", str(missing)], cwd=root, env=env,
                            stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
    fresh = next((root / "missing.work/.rustrace").glob("toolchain-*.json"))
    raw = fresh.read_bytes()
    report = json.loads(raw)["report"]
    assert result.returncode != 0 and any(p["required"] and p["status"] != "available" and p["remediation"] for p in report["probes"]), "missing tools must remain actionable"
    assert b"required Rust tools unavailable" in result.stdout + result.stderr
    if sentinel.encode() in raw + result.stdout + result.stderr:
        failures.append("missing: synthetic credential persisted or displayed by production")
    assert not select.select([trap], [], [], 0)[0], "missing tool attempted network"
    assert all(path.read_bytes() == data for path, data in prior.items()), "original observations changed"
    assert not failures, "\n".join(failures)
    print("Production logging control/trace/debug and missing-tool paths: no synthetic credential in new metadata or CLI, exact versions retained, prior metadata unchanged, zero network connections")
