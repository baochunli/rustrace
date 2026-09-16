"""HOME absent must not introduce relative Cargo/rustup fallback paths."""
import os
import sys
import pathlib
sys.path.insert(0, str(pathlib.Path(sys.argv[1])))
import test_home
assert 'HOME' not in os.environ
assert 'CARGO_HOME' not in os.environ
assert 'RUSTUP_HOME' not in os.environ
test_home.isolate()
assert 'CARGO_HOME' not in os.environ
assert 'RUSTUP_HOME' not in os.environ
assert pathlib.Path(os.environ['HOME']).is_absolute()
