"""Checks finish before production recording; offline/hanging work remains usable."""
import hashlib
import fcntl
import io
import json
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import sys
import tarfile
import tempfile
import termios
import time

from pty_screen import rendered_screen

from test_home import isolate
isolate(checks_enabled=True)

binary = sys.argv[1]
manifest = b'''format_version = 1
course_id = "course"
assignment_id = "update-check"
assignment_version = "v1"
title = "Update preflight"
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


def executable(path, body):
    path.write_text("#!" + sys.executable + "\n" + body)
    path.chmod(0o700)


def run_work(package, workspace, tools, env, expect_request):
    active = tools / "session-active"
    active.unlink(missing_ok=True)
    snapshots = {str(p.relative_to(workspace)): hashlib.sha256(p.read_bytes()).hexdigest()
                 for p in workspace.joinpath(".rustrace").rglob("*") if p.is_file()}
    (tools / "before-session.json").write_text(json.dumps({"workspace": str(workspace), "files": snapshots}))
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 120, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = ("import subprocess,termios,sys; before=termios.tcgetattr(0); "
               "result=subprocess.run(sys.argv[1:]); "
               "assert termios.tcgetattr(0)==before, 'terminal mode leaked'; "
               "print('TERMINAL_RESTORED',flush=True); sys.exit(result.returncode)")
    proc = subprocess.Popen([sys.executable, "-c", wrapper, binary, "work", str(package), "--workspace", str(workspace)],
                            stdin=slave, stdout=slave, stderr=slave, preexec_fn=child,
                            env=env)
    transcript = bytearray()
    started = None
    request_bytes = None
    quit_sent = False
    deadline = time.monotonic() + 15
    try:
        while proc.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.03)[0]:
                transcript.extend(os.read(master, 65536))
            screen = rendered_screen(transcript, rows=32, columns=120)
            if started is None and " files" in screen:
                started = time.time()
                requests = tools / "requests.jsonl"
                request_bytes = requests.read_bytes() if requests.exists() else b""
                if expect_request:
                    assert request_bytes, "no pre-session request"
                    last = json.loads(request_bytes.splitlines()[-1])
                    assert last["time"] <= started
                    try:
                        os.kill(last["pid"], 0)
                    except ProcessLookupError:
                        pass
                    else:
                        raise AssertionError("curl not reaped before editor")
                active.touch()
            if started is not None and not quit_sent and time.time() - started > 0.25:
                requests = tools / "requests.jsonl"
                assert (requests.read_bytes() if requests.exists() else b"") == request_bytes
                os.write(master, b"\x11")
                quit_sent = True
        assert proc.poll() is not None, ("work startup stalled", bytes(transcript[-4000:]))
        assert proc.returncode == 0, bytes(transcript[-4000:])
        assert quit_sent
        while select.select([master], [], [], 0.02)[0]:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            if not chunk:
                break
            transcript.extend(chunk)
        assert b"TERMINAL_RESTORED" in transcript, "terminal cleanup was not verified"
        requests = tools / "requests.jsonl"
        assert (requests.read_bytes() if requests.exists() else b"") == request_bytes
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
        os.close(master)
        os.close(slave)
        if proc.poll() is None:
            proc.wait(timeout=5)


with tempfile.TemporaryDirectory(prefix="rustrace-update-work-") as temporary:
    root = pathlib.Path(temporary)
    tools = root / "bin"
    tools.mkdir()
    executable(tools / "curl", pathlib.Path(__file__).with_name("update_curl.py").read_text())
    (tools / "latest.json").write_text(json.dumps({
        "schema_version": 1, "version": "99.0.0", "tag": "v99.0.0", "commit": "a" * 40,
        "event_format": 1, "package_format": 1, "assignment_format": 2,
        "source": {"repository": "https://github.com/baochunli/rustrace", "tag": "v99.0.0"},
        "targets": {},
    }))
    executable(tools / "rustup", '''import os,pathlib,sys
root=pathlib.Path(__file__).parent
args=sys.argv[1:]
assert os.environ["RUSTUP_AUTO_INSTALL"] == "0"
if args == ["--version"]: print("rustup 1.28.2 (fixture)")
elif args == ["toolchain", "list"]: print("1.98.1")
elif args == ["show", "active-toolchain"]: print("1.98.1 (environment override)")
elif args[:2] == ["which", "--toolchain"]:
    path=root/args[3]
    if not path.exists(): sys.exit(1)
    print(path)
elif args[:2] == ["run", "1.98.1"]: os.execv(args[2],args[2:])
else: raise AssertionError(args)
''')
    executable(tools / "rustc", 'print("rustc 1.98.1 (fixture)\\nrelease: 1.98.1\\nhost: fixture-host")')
    executable(tools / "cargo", 'print("cargo 1.98.1 (fixture)")')
    package = root / "assignment.rta"
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, content in [("assignment.toml", manifest),
                              ("starter/Cargo.toml", b'[package]\nname="fixture"\nversion="0.1.0"\n[workspace]\n'),
                              ("starter/main.rs", b"A")]:
            info = tarfile.TarInfo(name)
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
    contents = data.getvalue()
    while contents.endswith(bytes(512)):
        contents = contents[:-512]
    package.write_bytes(contents + bytes(1024))
    env = {**os.environ, "PATH": str(tools), "XDG_CONFIG_HOME": str(root / "config"),
           "XDG_STATE_HOME": str(root / "state"), "TERM": "xterm-256color"}
    env.pop("RUSTUP_TOOLCHAIN", None)
    state = root / "state/rustrace/update-state.json"
    for mode in ["success", "offline", "hang", "missing"]:
        (tools / "mode").write_text(mode)
        state.unlink(missing_ok=True)
        (tools / "requests.jsonl").unlink(missing_ok=True)
        if mode == "missing":
            (tools / "curl").unlink()
        workspace = root / (mode + ".work")
        run_work(package, workspace, tools, env, mode != "missing")
        assert state.exists(), "check attempt was not persisted"
        cached = json.loads(state.read_text())
        assert cached["last_attempt"] and cached["next_eligible"]
        if mode == "success":
            assert cached["last_success"] and cached["latest"]["version"] == "99.0.0", "fake endpoint rejected a late request"
        else:
            assert cached["last_success"] is None and cached["latest"] is None
        # Across launches, failures and successes alike must suppress a retry.
        before_requests = (tools / "requests.jsonl").read_bytes() if mode != "missing" else b""
        run_work(package, workspace, tools, env, False)
        assert ((tools / "requests.jsonl").read_bytes() if mode != "missing" else b"") == before_requests
        # Forced eligibility tests the resume boundary independently of throttle.
        if mode == "success":
            cached["next_eligible"] = None
            cached["last_attempt"] = None
            cached["last_success"] = None
            cached["latest"] = None
            state.write_text(json.dumps(cached))
            run_work(package, workspace, tools, env, True)
            resumed = json.loads(state.read_text())
            assert resumed["last_success"] and resumed["latest"]["version"] == "99.0.0", "fake endpoint rejected a post-resume request"
        # No update identifiers or endpoint may be recorded in provenance.
        for file in workspace.joinpath(".rustrace").rglob("*"):
            if file.is_file():
                content = file.read_bytes()
                assert b"latest.json" not in content and b"v99.0.0" not in content
        print(mode + ": normal PTY start/resume, cache throttle, no late requests, no provenance")
    # Invalid archives and inspection must never fetch, even when eligible.
    executable(tools / "curl", pathlib.Path(__file__).with_name("update_curl.py").read_text())
    (tools / "mode").write_text("success")
    state.unlink()
    before_requests = (tools / "requests.jsonl").read_bytes() if (tools / "requests.jsonl").exists() else b""
    invalid = root / "invalid.rta"
    invalid.write_bytes(b"invalid")
    result = subprocess.run([binary,"work",str(invalid)],env=env,capture_output=True,timeout=5)
    assert result.returncode != 0
    result = subprocess.run([binary,"work",str(package),"--workspace",str(root/"success.work"),"--inspect"],env=env,capture_output=True,timeout=5)
    assert result.returncode == 0, result.stdout
    assert ((tools/"requests.jsonl").read_bytes() if (tools/"requests.jsonl").exists() else b"") == before_requests
    print("invalid archives and --inspect remain offline")
