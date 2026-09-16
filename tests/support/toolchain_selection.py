"""Exercise real rustup selection with linked fake tools in a disposable rustup home.

No installation, download, component modification or global override is performed.
"""
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

from test_home import isolate
isolate()

rustup = shutil.which("rustup")
assert rustup, "this Rust contributor test requires the installed rustup used by the matrix"
test_binary = sys.argv[1]

with tempfile.TemporaryDirectory(prefix="rustrace-real-rustup-") as temporary:
    root = pathlib.Path(temporary).resolve()
    workspace = root / "workspace"
    workspace.mkdir()
    env = {**os.environ, "RUSTUP_HOME": str(root / "rustup-home"),
           "RUSTUP_AUTO_INSTALL": "0"}
    env.pop("RUSTUP_TOOLCHAIN", None)

    def run(args, cwd=workspace):
        result = subprocess.run([rustup, *args], env=env, cwd=cwd,
                                capture_output=True, timeout=10)
        assert result.returncode == 0, (args, result.stdout, result.stderr)
        return result

    for name, version in [("course-a", "1.98.1"), ("course-b", "1.99.0")]:
        toolchain = root / name
        (toolchain / "bin").mkdir(parents=True)
        (toolchain / "lib").mkdir()
        for tool, output in [
            ("rustc", f"rustc {version} ({name})\nrelease: {version}\nhost: test-host"),
            ("cargo", f"cargo {version} ({name})"),
        ]:
            path = toolchain / "bin" / tool
            path.write_text("#!" + sys.executable + "\nprint(" + repr(output) + ")\n")
            path.chmod(0o700)
        run(["toolchain", "link", name, str(toolchain)])
    # Both names are already linked/installed; default cannot install here.
    run(["default", "course-a"])

    def discover(expected, pin=None, override=None):
        child_env = {**env, "RUSTRACE_DISCOVERY_TEST_ROOT": str(workspace),
                     "RUSTRACE_DISCOVERY_TEST_OUTPUT": str(root / "report.json")}
        if pin:
            child_env["RUSTRACE_DISCOVERY_TEST_PIN"] = pin
        if override:
            child_env["RUSTUP_TOOLCHAIN"] = override
        result = subprocess.run([test_binary, "--exact", "isolated_discovery_child"],
                                env=child_env, cwd=workspace, capture_output=True, timeout=25)
        assert result.returncode == 0, (result.stdout, result.stderr)
        report = json.loads((root / "report.json").read_bytes())
        assert report["selected_toolchain"] == expected, report
        assert not [p for p in report["probes"] if p["required"] and p["status"] != "available"], report
        for component in ["rustc", "cargo"]:
            probe = next(p for p in report["probes"] if p["component"] == component and p["purpose"] == "version")
            assert f"({expected})" in probe["stdout"], probe
            assert probe["argv"][1:3] == ["run", expected], probe
            assert pathlib.Path(probe["argv"][3]).resolve() == root / expected / "bin" / component, probe
        return report

    discover("course-a")
    run(["override", "set", "course-b"])
    discover("course-b")
    discover("course-a", override="course-a")
    discover("course-b", pin="course-b", override="course-a")
    discover("course-a", pin="course-a", override="course-b")
    # Even an installed directory selection must not hide an unavailable env pin.
    child_env = {**env, "RUSTUP_TOOLCHAIN": "missing-local-toolchain",
                 "RUSTRACE_DISCOVERY_TEST_ROOT": str(workspace),
                 "RUSTRACE_DISCOVERY_TEST_OUTPUT": str(root / "missing.json")}
    result = subprocess.run([test_binary, "--exact", "isolated_discovery_child"],
                            env=child_env, cwd=workspace, capture_output=True, timeout=25)
    assert result.returncode == 0, result.stderr
    missing = json.loads((root / "missing.json").read_bytes())
    assert any(p["required"] and p["status"] != "available" for p in missing["probes"]), missing
    assert not any("run" in p["argv"][1:2] for p in missing["probes"]), missing
    assert not (root / "rustup-home/toolchains/missing-local-toolchain").exists()
    print("Real rustup: default, directory override, environment override, explicit assignment priority, exact executable/version and missing non-installing selection passed")
