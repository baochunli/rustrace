#!/usr/bin/env python3
"""Install, notice, update and restart the real CLI using only local fixtures."""
import functools
import http.server
import io
import shutil
import tarfile
import threading
import fcntl
import json
import os
import pathlib
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

from pty_screen import rendered_screen
from test_home import isolate


def run(arguments, env, cwd):
    result = subprocess.run(arguments, env=env, cwd=cwd, capture_output=True,
                            text=True, timeout=30)
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


def work(binary, package, workspace, env, badge, request_log):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 140, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    wrapper = ("import subprocess,termios,sys; before=termios.tcgetattr(0); "
               "result=subprocess.run(sys.argv[1:]); "
               "assert termios.tcgetattr(0)==before, 'terminal mode leaked'; "
               "print('TERMINAL_RESTORED',flush=True); sys.exit(result.returncode)")
    process = subprocess.Popen([sys.executable, '-c', wrapper, str(binary), 'work',
                                str(package), '--workspace', str(workspace)],
                               env=env, cwd=package.parent, stdin=slave,
                               stdout=slave, stderr=slave, preexec_fn=child)
    transcript = bytearray()
    deadline = time.monotonic() + 20
    stage = 0
    requests = None
    try:
        while process.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], .03)[0]:
                transcript.extend(os.read(master, 65536))
            assert len(transcript) < 2 * 1024 * 1024
            if b'\x1b[?1049h' not in transcript:
                continue
            screen = rendered_screen(transcript, rows=36, columns=140)
            if stage == 0 and ' files' in screen and 'menu' in screen:
                assert ('● menu' in screen) == badge, screen
                requests = request_log.read_bytes()
                os.write(master, b'\x1b[18~')  # F7
                stage = 1
            elif stage == 1 and 'Automatic checks: On' in screen:
                row = next(line for line in screen.splitlines() if 'Update Rustrace' in line)
                assert bool(re.search(r'Update Rustrace\s+NEW', row)) == badge, screen
                assert 'Update dependencies' in screen, screen
                assert request_log.read_bytes() == requests, 'menu made a request'
                os.write(master, b'\x1b')
                stage = 2
            elif stage == 2 and 'menu' in screen and 'Automatic checks: On' not in screen:
                assert ('● menu' in screen) == badge, screen
                os.write(master, b'\x11')  # Ctrl-Q
                stage = 3
        assert process.poll() is not None, ('work stalled', bytes(transcript[-4000:]))
        assert process.returncode == 0, bytes(transcript[-4000:])
        assert stage == 3, bytes(transcript[-4000:])
        while select.select([master], [], [], .03)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            transcript.extend(chunk)
        assert b'TERMINAL_RESTORED' in transcript
        assert b'\x1b[?1049l' in transcript
        assert request_log.read_bytes() == requests, 'request during work/quit'
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
        # Close before waiting: macOS may hold exit until PTY descriptors close.
        os.close(master)
        os.close(slave)
        if process.poll() is None:
            process.wait(timeout=5)


def executable(path, body):
    path.write_text('#!' + sys.executable + '\n' + body)
    path.chmod(0o700)


def prepare_fixture(root, original, replacement):
    tools = root / 'tools'
    tools.mkdir()
    # Finite PATH: no real Rust launchers or ambient shell/startup settings.
    for name in ('curl', 'awk', 'sed', 'grep', 'mktemp', 'mkdir', 'mv', 'rm',
                 'cat', 'dirname', 'uname', 'python3'):
        path = shutil.which(name)
        assert path, f'missing utility: {name}'
        (tools / name).symlink_to(path)
    env = {'PATH': str(tools), 'SHELL': '/bin/zsh', 'TERM': 'xterm-256color',
           'RUSTUP_AUTO_INSTALL': '0', 'CARGO_NET_OFFLINE': 'true',
           'CARGO_TARGET_DIR': str(root / 'target')}
    for key, folder in [('HOME', 'home'), ('CARGO_HOME', 'cargo'),
                        ('RUSTUP_HOME', 'rustup'), ('XDG_CONFIG_HOME', 'config'),
                        ('XDG_STATE_HOME', 'state')]:
        (root / folder).mkdir()
        env[key] = str(root / folder)
    state = pathlib.Path(env['XDG_STATE_HOME']) / 'rustrace/update-state.json'
    state.parent.mkdir()
    # Use the shared helper's enabled-state schema in this fixture's own homes.
    state.write_bytes((pathlib.Path(os.environ['XDG_STATE_HOME']) /
                       'rustrace/update-state.json').read_bytes())
    old = run([str(original), '--version'], env, root).strip().split()[1]
    new = run([str(replacement), '--version'], env, root).strip().split()[1]
    repository = root / 'release-source'
    repository.mkdir()
    git = shutil.which('git')
    assert git
    run([git, 'init', '-q', str(repository)], env, root)
    commits = {}
    for version in (old, new):
        (repository / 'Cargo.toml').write_text(f'[package]\nname="rustrace"\nversion="{version}"\n')
        run([git, 'add', 'Cargo.toml'], env, repository)
        run([git, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
             '-c', 'commit.gpgsign=false', 'commit', '-qm', version], env, repository)
        run([git, 'tag', 'v' + version], env, repository)
        commits[version] = run([git, 'rev-parse', 'HEAD'], env, repository).strip()
    env['RUSTRACE_SOURCE_REPOSITORY'] = repository.as_uri()
    cargo_log = root / 'cargo.jsonl'
    request_log = root / 'requests.jsonl'
    request_log.touch()
    settings = dict(binaries={old: str(original), new: str(replacement)},
                    repository=repository.as_uri(), git=git, cargo_log=str(cargo_log))
    (tools / 'fixture.json').write_text(json.dumps(settings))
    executable(tools / 'cargo', r"""import json,os,pathlib,shutil,subprocess,sys
root=pathlib.Path(__file__).parent
fixture=json.loads((root/'fixture.json').read_text())
args=sys.argv[1:]
assert os.environ['RUSTUP_AUTO_INSTALL']=='0'
if args in (['-V'], ['--version'], ['--version', '--verbose']):
    print('cargo 1.98.1 (fixture)'); sys.exit(0)
assert args[:4]==['+1.98.1','install','--git',fixture['repository']], args
assert args[4]=='--tag' and args[6:8]==['rustrace','--locked'], args
assert args[8:] in ([],['--force']), args
version=args[5][1:]
assert version in fixture['binaries'], args
# Resolve the selected local Git tag; no source URL can leave this fixture.
source=fixture['repository'].removeprefix('file://')
result=subprocess.run([fixture['git'],'show',args[5]+':Cargo.toml'],cwd=source,
                      capture_output=True,text=True,check=True)
assert 'version="'+version+'"' in result.stdout
with open(fixture['cargo_log'],'a') as log: log.write(json.dumps(args)+'\n')
install=pathlib.Path(os.environ.get('CARGO_INSTALL_ROOT',os.environ['CARGO_HOME']))/'bin/rustrace'
install.parent.mkdir(parents=True,exist_ok=True)
temporary=install.with_suffix('.new')
shutil.copyfile(fixture['binaries'][version],temporary)
temporary.chmod(0o700); temporary.replace(install)
print('fixture Cargo installed '+version,flush=True)
""")
    executable(tools / 'rustup', r"""import os,pathlib,sys
root=pathlib.Path(__file__).parent
args=sys.argv[1:]
assert os.environ['RUSTUP_AUTO_INSTALL']=='0'
if args==['--version']: print('rustup 1.28.2 (fixture)')
elif args==['toolchain','list']: print('1.98.1')
elif args==['show','active-toolchain']: print('1.98.1 (environment override)')
elif args[:2]==['which','--toolchain']:
    path=root/args[3]
    if not path.exists(): sys.exit(1)
    print(path)
elif args[:2]==['run','1.98.1']:
    tool=args[2]
    if not os.path.isabs(tool): tool=str(root/tool)
    os.execv(tool,[tool,*args[3:]])
else: raise AssertionError(args)
""")
    executable(tools / 'rustc', r"print('rustc 1.98.1 (fixture)\nrelease: 1.98.1\nhost: fixture-host')")
    package = root / 'assignment.rta'
    manifest = b'''format_version = 2
course_id = "course"
assignment_id = "install-update-e2e"
assignment_version = "v1"
title = "Install and update"
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
    with tarfile.open(package, 'w', format=tarfile.USTAR_FORMAT) as archive:
        for name, contents in [('assignment.toml', manifest),
                               ('starter/Cargo.toml', b'[package]\nname="fixture"\nversion="0.1.0"\n[workspace]\n'),
                               ('starter/main.rs', b'fn main() {}\n'),
                               ('test-cases/alpha.in', b'input\n'),
                               ('test-cases/alpha.expected', b'output\n')]:
            info = tarfile.TarInfo(name)
            info.size = len(contents)
            archive.addfile(info, io.BytesIO(contents))
    data = package.read_bytes()
    while data.endswith(bytes(512)):
        data = data[:-512]
    package.write_bytes(data + bytes(1024))

    class QuietHandler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            if self.path != '/latest.json':
                self.send_error(404)
                return
            super().do_GET()

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0),
                 functools.partial(QuietHandler, directory=str(root)))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f'http://127.0.0.1:{server.server_port}/latest.json'
    env['RUSTRACE_MANIFEST_URL'] = url
    real_curl = (tools / 'curl').resolve()
    (tools / 'curl').unlink()
    executable(tools / 'curl', f"""import os,sys
args=sys.argv[1:]
canonical='https://github.com/baochunli/rustrace/releases/latest/download/latest.json'
assert args[0]=='--disable', args
if args[-1]==canonical:
    with open({str(request_log)!r},'a') as log: log.write(canonical+'\\n')
    args[-1]={url!r}
else:
    assert '-o' in args and args[args.index('-o')-1]=={url!r}, args
os.execv({str(real_curl)!r},[{str(real_curl)!r},*args])
""")

    def publish(version, installer=False):
        source = repository.as_uri() if installer else 'https://github.com/baochunli/rustrace'
        manifest = dict(schema_version=1, version=version, tag='v' + version,
                        commit=commits[version], event_format=1, package_format=1,
                        assignment_format=2, source=dict(repository=source, tag='v' + version),
                        targets={})
        temporary = root / 'latest.new'
        temporary.write_text(json.dumps(manifest))
        temporary.replace(root / 'latest.json')

    def stop():
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        assert not thread.is_alive()

    return env, publish, stop, package, cargo_log, request_log


def scenario(original, replacement):
    started = time.monotonic()
    isolate(checks_enabled=True)
    with tempfile.TemporaryDirectory(prefix='rustrace-install-update-e2e-') as temporary:
        root = pathlib.Path(temporary).resolve()
        fixture = prepare_fixture(root, original, replacement)
        env, publish, stop, package, cargo_log, request_log = fixture
        try:
            old_version = run([str(original), '--version'], env, root).strip().split()[1]
            new_version = run([str(replacement), '--version'], env, root).strip().split()[1]
            assert tuple(map(int, new_version.split('.'))) > tuple(map(int, old_version.split('.')))
            publish(old_version, installer=True)
            output = run(['/bin/sh', str(pathlib.Path(__file__).resolve().parents[2] /
                                        'scripts/install.sh')], env, root)
            assert f'Building Rustrace {old_version} from source with Rust 1.98.1' in output
            assert 'Open a new terminal, then run: rustrace --version' in output
            binary = pathlib.Path(env['CARGO_HOME']) / 'bin/rustrace'
            receipt_path = pathlib.Path(env['XDG_STATE_HOME']) / 'rustrace/install.json'
            receipt = json.loads(receipt_path.read_text())
            assert receipt['version'] == old_version and receipt['path'] == str(binary)
            assert receipt['method'] == 'cargo-git'
            assert run([str(binary), '--version'], env, root) == f'rustrace {old_version}\n'
            # Apply the PATH block exactly as a newly opened terminal would.
            env['PATH'] = run(['/bin/sh', '-c', '. "$HOME/.zshrc"; printf "%s" "$PATH"'], env, root)
            assert str(binary.parent) in env['PATH'].split(os.pathsep)
            publish(old_version)
            work(binary, package, root / 'assignment.work', env, False, request_log)
            state_path = pathlib.Path(env['XDG_STATE_HOME']) / 'rustrace/update-state.json'
            state = json.loads(state_path.read_text())
            assert state['checks_enabled'] and state['latest']['version'] == old_version
            assert len(request_log.read_text().splitlines()) == 1
            print('install -> first work: installer receipt, PATH, daily check, no badge')

            publish(new_version)
            # Simulate the next eligible day without sleeping or changing the OS clock.
            state['last_attempt'] = state['next_eligible'] = None
            state_path.write_text(json.dumps(state))
            work(binary, package, root / 'assignment.work', env, True, request_log)
            assert len(request_log.read_text().splitlines()) == 2
            assert json.loads(state_path.read_text())['latest']['version'] == new_version
            print('new manifest -> second work: Update Rustrace NEW and ● menu')

            output = run([str(binary), 'update'], env, root / 'assignment.work')
            assert f'Building {new_version} from source (this takes a few minutes)...' in output
            assert f'Installed {new_version}. Restart Rustrace to use it.' in output
            assert 'fixture Cargo installed' in output
            assert len(request_log.read_text().splitlines()) == 3
            receipt = json.loads(receipt_path.read_text())
            assert receipt['version'] == new_version and receipt['tag'] == 'v' + new_version
            invocations = [json.loads(line) for line in cargo_log.read_text().splitlines()]
            expected = ['+1.98.1', 'install', '--git', receipt['repository'], '--tag']
            assert invocations == [expected + ['v' + old_version, 'rustrace', '--locked'],
                                   expected + ['v' + new_version, 'rustrace', '--locked', '--force']]
            print('quit -> update: exact Cargo argv, success/restart wording, new receipt')

            assert run([str(binary), '--version'], env, root) == f'rustrace {new_version}\n'
            work(binary, package, root / 'updated.work', env, False, request_log)
            assert len(request_log.read_text().splitlines()) == 3, 'daily throttle lost after update'
            session = json.loads((root / 'updated.work/.rustrace/session.json').read_text())
            assert session['client_version'] == new_version, session
            assert session['test_case_suite_hash'], 'assignment was not a v2 fixture'
            assert json.loads(state_path.read_text())['checks_enabled']
            elapsed = time.monotonic() - started
            assert elapsed < 120, f'e2e exceeded two minutes: {elapsed:.2f}s'
            print(f'third work: version {new_version}, new session identity, no badge ({elapsed:.2f}s)')
        finally:
            stop()


if __name__ == '__main__':
    scenario(pathlib.Path(sys.argv[1]).resolve(), pathlib.Path(sys.argv[2]).resolve())
