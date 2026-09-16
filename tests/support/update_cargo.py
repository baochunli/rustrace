#!/usr/bin/python3
"""Direct Cargo fixture: record literal argv and atomically replace on success."""
import json
import os
import pathlib
import sys
import shutil
import time
root = pathlib.Path(__file__).parent
(root / 'cargo.json').write_text(json.dumps({'argv': sys.argv[1:], 'root': os.environ.get('CARGO_INSTALL_ROOT'), 'auto_install': os.environ.get('RUSTUP_AUTO_INSTALL')}))
mode = (root / 'cargo-mode').read_text()
if mode == 'wait-failure':
    deadline = time.monotonic() + 15
    while not (root / 'release-cargo').exists():
        assert time.monotonic() < deadline
        time.sleep(.01)
if mode in ['failure', 'wait-failure']:
    print('fixture cargo build failure', file=sys.stderr)
    sys.exit(17)
version = sys.argv[sys.argv.index('--tag') + 1][1:]
target = os.environ['UPDATE_TARGET']
lines = {'event': '1', 'package': '1', 'assignment': '2'}
if mode == 'version':
    version = '98.0.0'
if mode == 'target':
    target = 'wrong-target'
if mode in lines:
    lines[mode] = 'invalid'
output = f'rustrace {version}\nbuild commit: {"a" * 40}\nevent format: {lines["event"]}\npackage format: {lines["package"]}\nassignment format: {lines["assignment"]}\ntarget: {target}\n'
binary = pathlib.Path(os.environ['CARGO_INSTALL_ROOT']) / 'bin/rustrace'
temporary = binary.with_suffix('.new')
temporary.write_text('#!/usr/bin/python3\nimport sys\nassert sys.argv[1:] == ["--version", "--verbose"]\nprint(' + repr(output) + ', end="")\n')
if os.environ.get('UPDATE_REPLACEMENT'):
    shutil.copyfile(os.environ['UPDATE_REPLACEMENT'], temporary)
temporary.chmod(0o700)
temporary.replace(binary)
print('fixture cargo stdout')
print('fixture cargo stderr', file=sys.stderr)
