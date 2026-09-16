"""Give each standalone binary driver disposable application homes."""
import os
import pathlib
import tempfile

# Retained until interpreter exit; subprocesses finish before fixture cleanup.
_home = None


def isolate(checks_enabled=False):
    global _home
    import json

    if "HOME" in os.environ:
        original_home = pathlib.Path(os.environ["HOME"])
        os.environ.setdefault("RUSTUP_HOME", str(original_home / ".rustup"))
        os.environ.setdefault("CARGO_HOME", str(original_home / ".cargo"))
    _home = tempfile.TemporaryDirectory(prefix="rustrace-driver-home-")
    root = pathlib.Path(_home.name)
    for key, directory in [("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
                           ("XDG_STATE_HOME", "state")]:
        path = root / directory
        path.mkdir()
        os.environ[key] = str(path)
    state = root / "state/rustrace/update-state.json"
    state.parent.mkdir()
    state.write_text(json.dumps({"schema_version": 1, "checks_enabled": checks_enabled,
                                "last_attempt": None, "last_success": None,
                                "next_eligible": None, "latest": None}))
