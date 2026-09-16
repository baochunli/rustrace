#!/usr/bin/env python3
"""Hermetic source-installer tests: local HTTP, fake tools, one real Git build."""
import functools
import http.server
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import threading
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / 'scripts/install.sh'
REAL_RUSTUP = shutil.which('rustup')
REAL_CARGO = shutil.which('cargo')
REAL_RUSTC = shutil.which('rustc')
RUSTUP_HOME = os.environ.get('RUSTUP_HOME', str(pathlib.Path.home() / '.rustup'))


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass


class InstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='rustrace-install-')
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name).resolve()
        self.bin = self.root / 'tools'
        self.bin.mkdir()
        self.env = {k: v for k, v in os.environ.items() if k not in (
            'CARGO_INSTALL_ROOT', 'RUSTRACE_INSTALL_DIR', 'RUSTRACE_MANIFEST_URL',
            'RUSTRACE_SOURCE_REPOSITORY', 'TMPDIR', 'ENV', 'BASH_ENV')}
        for key, folder in [('HOME', 'home'), ('CARGO_HOME', 'cargo'),
                            ('XDG_STATE_HOME', 'state'), ('XDG_CONFIG_HOME', 'config')]:
            self.env[key] = str(self.root / folder)
            (self.root / folder).mkdir()
        self.env.update(PATH=str(self.bin), SHELL='/bin/zsh', RUSTUP_HOME=RUSTUP_HOME,
                        RUSTUP_AUTO_INSTALL='0', CARGO_NET_OFFLINE='true',
                        CARGO_TARGET_DIR=str(self.root / 'target'), RECORD=str(self.root / 'argv'), ENV_RECORD=str(self.root / 'child-env'),
                        FAKE_VERSION='1.2.3', FAKE_FORMATS='yes')
        # Explicit utility inventory keeps real rustup/cargo out of fake-tool tests.
        for tool in ('curl', 'awk', 'sed', 'grep', 'mktemp', 'mkdir', 'mv', 'rm',
                     'cat', 'dirname', 'uname', 'python3'):
            selected = os.environ.get('RUSTRACE_TEST_AWK', 'awk') if tool == 'awk' else tool
            executable = shutil.which(selected)
            self.assertIsNotNone(executable, f'required tool not installed: {selected}')
            (self.bin / tool).symlink_to(executable)
        self.tool('uname', 'echo Linux')
        self.tool('rustup', '''printf 'rustup:%s\\n' "${RUSTUP_AUTO_INSTALL:-unset}" >> "$ENV_RECORD"
printf 'rustup\\n' >> "$RECORD"
printf '<%s>\\n' "$@" >> "$RECORD"
case "$1" in
run) exit "${MISSING_TOOLCHAIN:-0}";;
toolchain) exit "${TOOLCHAIN_FAILURE:-0}";;
esac''')
        self.tool('cargo', '''printf 'cargo:%s\\n' "${RUSTUP_AUTO_INSTALL:-unset}" >> "$ENV_RECORD"
printf 'cargo\\n' >> "$RECORD"
printf '<%s>\\n' "$@" >> "$RECORD"
[ "${CARGO_FAILURE:-0}" = 0 ] || exit "$CARGO_FAILURE"
root=${CARGO_INSTALL_ROOT:-$CARGO_HOME}
if [ -z "${CARGO_INSTALL_ROOT:-}" ] && [ -n "${FIXTURE_CONFIG_ROOT:-}" ]; then root=$FIXTURE_CONFIG_ROOT; fi
while [ "$#" -gt 0 ]; do
 if [ "$1" = --root ]; then root=$2; shift; fi
 shift
done
mkdir -p "$root/bin"
cat > "$root/bin/rustrace" <<'BIN'
#!/bin/sh
[ "$*" = '--version --verbose' ] || exit 9
printf 'rustrace %s\\n' "$FAKE_VERSION"
if [ "$FAKE_FORMATS" = yes ]; then
 printf 'package format: 1\\nassignment format: 2\\n'
fi
exit "${BINARY_FAILURE:-0}"
BIN
chmod +x "$root/bin/rustrace"''')
        (self.bin / 'chmod').symlink_to(shutil.which('chmod'))
        self.repository_path = self.root / 'source.git'
        self.repository = self.repository_path.as_uri()
        self.env['RUSTRACE_SOURCE_REPOSITORY'] = self.repository
        self.manifest = dict(schema_version=1, version='1.2.3', tag='v1.2.3',
                             source=dict(repository=self.repository, tag='v1.2.3'), targets={})
        self.write_manifest()
        handler = functools.partial(QuietHandler, directory=str(self.root))
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)
        self.env['RUSTRACE_MANIFEST_URL'] = f'http://127.0.0.1:{self.server.server_port}/latest.json'

    def stop_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def tool(self, name, body):
        path = self.bin / name
        if path.exists() or path.is_symlink():
            path.unlink()
        path.write_text('#!/bin/sh\n' + body + '\n')
        path.chmod(0o755)

    def write_manifest(self):
        (self.root / 'latest.json').write_text(json.dumps(self.manifest, indent=2))

    def run_install(self, status=0, piped=False):
        result = subprocess.run(['/bin/sh'] if piped else ['/bin/sh', str(SCRIPT)],
                                input=SCRIPT.read_text() if piped else None,
                                env=self.env, cwd=self.root, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=55)
        self.assertEqual(result.returncode, status, result.stdout)
        return result.stdout

    def receipt(self):
        return json.loads((pathlib.Path(self.env['XDG_STATE_HOME']) / 'rustrace/install.json').read_text())

    def argv(self):
        path = self.root / 'argv'
        return path.read_text() if path.exists() else ''

    def test_selected_awk_is_on_the_hermetic_path(self):
        selected = os.environ.get('RUSTRACE_TEST_AWK', 'awk')
        expected = shutil.which(selected)
        self.assertIsNotNone(expected, f'selected awk not installed: {selected}')
        self.assertEqual((self.bin / 'awk').resolve(), pathlib.Path(expected).resolve())
        result = subprocess.run(['awk', 'BEGIN {print "selected-awk"}'], env=self.env,
                                text=True, capture_output=True, check=True)
        self.assertEqual(result.stdout, 'selected-awk\n')

    def test_fresh_and_reinstall(self):
        output = self.run_install(piped=True)
        self.assertIn('Building Rustrace 1.2.3 from source with Rust 1.98.1 (this takes a few minutes)', output)
        expected = dict(schema_version=1, method='cargo-git',
                        path=str(self.root / 'cargo/bin/rustrace'), tag='v1.2.3',
                        version='1.2.3', repository=self.repository)
        self.assertEqual(self.receipt(), expected)
        self.assertIn('<+1.98.1>\n<install>\n<--git>\n<' + self.repository + '>\n<--tag>\n<v1.2.3>\n<rustrace>\n<--locked>', self.argv())
        self.assertNotIn('replacing an existing', self.run_install())
        self.assertEqual(self.receipt(), expected)
        self.assertEqual(list((self.root / 'state/rustrace').iterdir()), [self.root / 'state/rustrace/install.json'])

    def test_existing_unowned_or_other_method(self):
        self.run_install()
        receipt = self.root / 'state/rustrace/install.json'
        for content in (None, '{"method":"direct"}', '{"method":"cargo-git","broken":'):
            if content is None:
                receipt.unlink()
            else:
                receipt.write_text(content)
            self.assertIn('replacing an existing Rustrace at ', self.run_install())

    def test_missing_rustup(self):
        (self.bin / 'rustup').unlink()
        output = self.run_install(1)
        self.assertIn("curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh", output)
        self.assertIn('docs/student-guide.md', output)
        self.assertEqual(self.argv(), '')

    def test_home_rustup_fallback(self):
        homebin = self.root / 'home/.cargo/bin'
        homebin.mkdir(parents=True)
        (self.bin / 'rustup').rename(homebin / 'rustup')
        self.run_install()
        self.assertIn('rustup', self.argv())

    def test_missing_toolchain_and_failure(self):
        self.env['MISSING_TOOLCHAIN'] = '1'
        self.run_install()
        command = '<toolchain>\n<install>\n<1.98.1>\n<--profile>\n<minimal>\n<--component>\n<clippy,rustfmt>'
        self.assertIn(command, self.argv())
        self.env['TOOLCHAIN_FAILURE'] = '7'
        self.assertIn('rustup toolchain install 1.98.1 --profile minimal --component clippy,rustfmt', self.run_install(1))

    def test_children_disable_rustup_auto_install(self):
        self.env['RUSTUP_AUTO_INSTALL'] = '1'
        self.env['MISSING_TOOLCHAIN'] = '1'
        self.run_install()
        self.assertEqual((self.root / 'child-env').read_text().splitlines(),
                         ['rustup:0', 'rustup:0', 'cargo:0'])

    def check_invalid_manifests(self):
        valid = json.dumps(self.manifest)
        invalid = [valid.replace('"schema_version": 1', '"schema_version": 2'),
                   valid.replace('"schema_version": 1', '"schema_version": true'),
                   valid.replace('v1.2.3', 'v1.2.3;touch injected'),
                   valid.replace(self.repository, 'https://evil.example/repo'),
                   valid.replace('"version": "1.2.3"', '"version": "9.9.9"'),
                   valid[:-1], valid + '{}',
                   valid.replace('"schema_version": 1', '"schema_version": 1, "schema_version": 1'),
                   valid.replace('"source": {', '"source": {"nested": {"repository": "wrong"},')]
        # Nested unrelated repository is valid and must not shadow source.repository.
        nested = invalid.pop()
        for content in invalid:
            with self.subTest(content=content):
                (self.root / 'latest.json').write_text(content)
                self.run_install(1)
                self.assertNotIn('cargo', self.argv())
                self.assertFalse((self.root / 'state/rustrace/install.json').exists())
        (self.root / 'latest.json').write_text(nested)
        self.run_install()

    def test_invalid_manifest(self):
        self.check_invalid_manifests()

    def test_awk_fallback(self):
        (self.bin / 'python3').unlink()
        self.check_invalid_manifests()
        self.assertEqual(self.receipt()['tag'], 'v1.2.3')

    def test_awk_rejects_forged_source_object(self):
        (self.bin / 'python3').unlink()
        del self.manifest['source']
        self.manifest['source/repository'] = self.repository
        self.manifest['source/tag'] = 'v1.2.3'
        self.write_manifest()
        self.run_install(1)
        self.assertNotIn('cargo', self.argv())

    def check_unstable_tags(self, fallback):
        if fallback:
            (self.bin / 'python3').unlink()
        for tag, version in [('v1.2.3-rc1', '1.2.3-rc1'),
                             ('v1.2', '1.2'), ('v01.2.3', '01.2.3')]:
            with self.subTest(tag=tag, fallback=fallback):
                self.manifest.update(tag=tag, version=version)
                self.manifest['source']['tag'] = tag
                self.env['FAKE_VERSION'] = version
                self.write_manifest()
                (self.root / 'argv').unlink(missing_ok=True)
                self.run_install(1)
                self.assertNotIn('cargo', self.argv())
                self.assertFalse((self.root / 'state/rustrace/install.json').exists())

    def test_unstable_tags_python(self):
        self.check_unstable_tags(False)

    def test_unstable_tags_awk(self):
        self.check_unstable_tags(True)

    def test_awk_rejects_escaped_source_key(self):
        (self.bin / 'python3').unlink()
        content = json.dumps(self.manifest).replace('"source":', r'"so\u0075rce":')
        self.assertIn(r'"so\u0075rce"', content)
        (self.root / 'latest.json').write_text(content)
        self.run_install(1)
        self.assertNotIn('cargo', self.argv())
        self.assertFalse((self.root / 'state/rustrace/install.json').exists())

    def test_fetch_failure(self):
        (self.root / 'latest.json').unlink()
        self.assertIn('manifest', self.run_install(1).lower())
        self.assertNotIn('cargo', self.argv())

    def test_manifest_download_size_limit(self):
        (self.root / 'latest.json').write_text(json.dumps(self.manifest) + ' ' * 65536)
        self.assertIn('manifest', self.run_install(1).lower())
        self.assertNotIn('cargo', self.argv())
        self.assertFalse((self.root / 'state/rustrace/install.json').exists())

    def test_cargo_failure(self):
        self.env['CARGO_FAILURE'] = '23'
        self.assertIn('build-essential', self.run_install(23))
        self.tool('uname', 'echo Darwin')
        self.assertIn('Xcode Command Line Tools', self.run_install(23))
        self.assertFalse((self.root / 'state/rustrace/install.json').exists())

    def test_binary_validation_preserves_receipt(self):
        self.run_install()
        before = self.receipt()
        for key, value in [('FAKE_VERSION', '9.9.9'), ('FAKE_FORMATS', 'no'), ('BINARY_FAILURE', '5')]:
            with self.subTest(key=key):
                old = self.env.get(key)
                self.env[key] = value
                self.run_install(1)
                self.assertEqual(self.receipt(), before)
                if old is None:
                    del self.env[key]
                else:
                    self.env[key] = old

    def test_path_blocks_once(self):
        for shell, profiles in [('zsh', ['.zshrc']), ('bash', ['.bashrc', '.bash_profile'])]:
            self.env['SHELL'] = '/bin/' + shell
            for name in profiles:
                (self.root / 'home' / name).write_text('# keep me\n')
            self.assertIn('Open a new terminal, then run: rustrace --version', self.run_install())
            self.run_install()
            for name in profiles:
                text = (self.root / 'home' / name).read_text()
                self.assertTrue(text.startswith('# keep me\n'))
                self.assertEqual(text.count('# >>> rustrace installer >>>'), 1)
                self.assertEqual(text.count('# <<< rustrace installer <<<'), 1)
                result = subprocess.run(['/bin/sh', '-c', '. "$HOME/' + name + '"; printf "%s" "$PATH"'], env=self.env, text=True, capture_output=True, check=True)
                self.assertIn(str(self.root / 'cargo/bin'), result.stdout.split(':'))
        (self.root / 'home/.bash_profile').unlink()
        self.run_install()
        self.assertFalse((self.root / 'home/.bash_profile').exists())

    def test_changed_install_root_adds_path_once(self):
        for shell, key, profiles in [
            ('zsh', 'RUSTRACE_INSTALL_DIR', ['.zshrc']),
            ('bash', 'CARGO_INSTALL_ROOT', ['.bashrc', '.bash_profile']),
        ]:
            with self.subTest(shell=shell):
                self.env.pop('RUSTRACE_INSTALL_DIR', None)
                self.env.pop('CARGO_INSTALL_ROOT', None)
                self.env['SHELL'] = '/bin/' + shell
                for profile in profiles:
                    (self.root / 'home' / profile).write_text('# keep me\n')
                first = self.root / (shell + '-first')
                second = self.root / (shell + "-second space ' $value `literal`")
                self.env[key] = str(first)
                self.run_install()
                self.env[key] = str(second)
                output = self.run_install()
                self.assertIn('Adding ' + str(second / 'bin') + ' to PATH', output)
                before = [(self.root / 'home' / profile).read_text() for profile in profiles]
                self.assertNotIn('Adding ', self.run_install())
                for profile, content in zip(profiles, before):
                    self.assertEqual((self.root / 'home' / profile).read_text(), content)
                    self.assertTrue(content.startswith('# keep me\n'))
                    result = subprocess.run(['/bin/sh', '-c', '. "$HOME/' + profile + '"; printf "%s" "$PATH"'],
                                            env=self.env, text=True, capture_output=True, check=True)
                    self.assertIn(str(first / 'bin'), result.stdout.split(':'))
                    self.assertIn(str(second / 'bin'), result.stdout.split(':'))
                    self.assertEqual(result.stdout.split(':').count(str(second / 'bin')), 1)
                del self.env[key]

    def test_path_already_present(self):
        self.env['PATH'] += ':' + str(self.root / 'cargo/bin')
        self.assertIn('Installed rustrace 1.2.3 at ', self.run_install())
        self.assertFalse((self.root / 'home/.zshrc').exists())

    def test_fish_and_unknown_shell(self):
        for shell in ('fish', 'unknown'):
            self.env['SHELL'] = '/bin/' + shell
            output = self.run_install()
            self.assertIn(str(self.root / 'cargo/bin'), output)
            self.assertIn('fish_add_path' if shell == 'fish' else 'export PATH=', output)
            self.assertEqual(list((self.root / 'home').iterdir()), [])

    def test_unsupported_os_and_missing_curl(self):
        self.tool('uname', 'echo FreeBSD')
        self.assertIn('cargo +1.98.1 install', self.run_install(1))
        self.tool('uname', 'echo Linux')
        (self.bin / 'curl').unlink()
        self.assertIn('curl', self.run_install(1))
        self.assertEqual(self.argv(), '')

    def test_install_roots_and_literal_paths(self):
        for key in ('CARGO_INSTALL_ROOT', 'RUSTRACE_INSTALL_DIR'):
            root = self.root / (key + " space ' $value `literal`")
            self.env[key] = str(root)
            self.run_install()
            self.assertEqual(self.receipt()['path'], str(root / 'bin/rustrace'))
            if key == 'RUSTRACE_INSTALL_DIR':
                self.assertIn('<--root>\n<' + str(root) + '>', self.argv())
            del self.env[key]
        # Fresh profile exercises literal quoting rather than prior marker reuse.
        (self.root / 'home/.zshrc').unlink()
        self.env['RUSTRACE_INSTALL_DIR'] = str(root)
        self.run_install()
        result = subprocess.run(['/bin/sh', '-c', '. "$HOME/.zshrc"; printf "%s" "$PATH"'], env=self.env, text=True, capture_output=True, check=True)
        self.assertIn(str(root / 'bin'), result.stdout.split(':'))
        self.assertFalse((self.root / 'literal').exists())

    def test_cargo_config_root_precedence(self):
        config_home = self.root / 'cargo'
        for name in ('config.toml', 'config'):
            with self.subTest(config=name):
                root = self.root / (name + ' configured space')
                config = config_home / name
                config.write_text('[build]\nroot = "ignored"\n[install] # installer root\n  root = "' + str(root) + '" # keep this\n[net]\noffline = true\n')
                self.env['FIXTURE_CONFIG_ROOT'] = str(root)
                self.run_install()
                self.assertEqual(self.receipt()['path'], str(root / 'bin/rustrace'))
                env_root = self.root / 'environment-root'
                self.env['CARGO_INSTALL_ROOT'] = str(env_root)
                self.run_install()
                self.assertEqual(self.receipt()['path'], str(env_root / 'bin/rustrace'))
                override = self.root / 'explicit-root'
                self.env['RUSTRACE_INSTALL_DIR'] = str(override)
                self.run_install()
                self.assertEqual(self.receipt()['path'], str(override / 'bin/rustrace'))
                del self.env['RUSTRACE_INSTALL_DIR']
                del self.env['CARGO_INSTALL_ROOT']
                config.unlink()
        # Cargo prefers legacy config when both filenames are present.
        legacy = self.root / 'legacy-root'
        (config_home / 'config').write_text('[install]\nroot = "' + str(legacy) + '"\n')
        (config_home / 'config.toml').write_text('[install]\nroot = "unused"\n')
        self.env['FIXTURE_CONFIG_ROOT'] = str(legacy)
        self.run_install()
        self.assertEqual(self.receipt()['path'], str(legacy / 'bin/rustrace'))

    def test_cargo_config_relative_root_and_strict_match(self):
        config = self.root / 'cargo/config.toml'
        config.write_text('[install]\nroot = "relative-root"\n')
        expected = self.root / 'relative-root'
        self.env['FIXTURE_CONFIG_ROOT'] = str(expected)
        self.run_install()
        self.assertEqual(self.receipt()['path'], str(expected / 'bin/rustrace'))
        before = self.receipt()
        for content in ('[install]\nroot = "escaped\\troot"\n',
                        '[install]\nroot = "first"\nroot = "second"\n',
                        '[install]\nroot = """multiline"""\n'):
            with self.subTest(config=content):
                config.write_text(content)
                (self.root / 'argv').unlink()
                self.assertIn('Set CARGO_INSTALL_ROOT explicitly', self.run_install(1))
                self.assertNotIn('cargo', self.argv())
                self.assertEqual(self.receipt(), before)

    def test_symlink_install_root_stays_logical(self):
        physical = self.root / 'physical-root'
        physical.mkdir()
        logical = self.root / 'logical-root'
        logical.symlink_to(physical, target_is_directory=True)
        self.env['CARGO_INSTALL_ROOT'] = str(logical)
        self.run_install()
        self.assertEqual(self.receipt()['path'], str(logical / 'bin/rustrace'))
        result = subprocess.run(['/bin/sh', '-c', '. "$HOME/.zshrc"; printf "%s" "$PATH"'],
                                env=self.env, text=True, capture_output=True, check=True)
        self.assertIn(str(logical / 'bin'), result.stdout.split(':'))
        self.assertNotIn(str(physical / 'bin'), result.stdout.split(':'))

    def test_default_home_root_and_state(self):
        del self.env['CARGO_HOME']
        del self.env['XDG_STATE_HOME']
        self.tool('cargo', (self.bin / 'cargo').read_text().replace('root=${CARGO_INSTALL_ROOT:-$CARGO_HOME}', 'root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}'))
        self.run_install()
        receipt = json.loads((self.root / 'home/.local/state/rustrace/install.json').read_text())
        self.assertEqual(receipt['path'], str(self.root / 'home/.cargo/bin/rustrace'))

    def test_real_cargo_local_git(self):
        self.assertIsNotNone(REAL_RUSTUP, 'real rustup required for end-to-end test')
        self.assertIsNotNone(REAL_CARGO, 'real cargo required for end-to-end test')
        for name, path in [('rustup', REAL_RUSTUP), ('cargo', REAL_CARGO)]:
            (self.bin / name).unlink()
            (self.bin / name).symlink_to(path)
        (self.bin / 'rustc').symlink_to(REAL_RUSTC)
        self.env['PATH'] += os.pathsep + os.path.dirname(shutil.which('git')) + os.pathsep + '/usr/bin:/bin'
        # Cargo offline mode refuses even a fresh file:// checkout. This crate
        # has no dependencies and its only Git source is the local bare repo.
        self.env['CARGO_NET_OFFLINE'] = 'false'
        self.env['FAKE_VERSION'] = '1.2.3'
        source = self.root / 'crate'
        (source / 'src').mkdir(parents=True)
        (source / 'Cargo.toml').write_text('[package]\nname="rustrace"\nversion="1.2.3"\nedition="2024"\n[workspace]\n')
        (source / 'src/main.rs').write_text('fn main() { println!("rustrace 1.2.3\\npackage format: 1\\nassignment format: 2"); }\n')
        def run(*args):
            result = subprocess.run(args, cwd=source, env=self.env, capture_output=True, text=True, timeout=45)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        run(str(REAL_CARGO), '+1.98.1', 'generate-lockfile', '--offline')
        run('git', 'init', '-q')
        run('git', 'add', '.')
        run('git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture')
        run('git', 'tag', 'v1.2.3')
        run('git', 'clone', '--bare', str(source), str(self.repository_path))
        (self.root / 'cargo/config.toml').write_text('[install]\nroot = "real-installed"\n')
        self.run_install()
        self.run_install()
        self.assertEqual(self.receipt()['method'], 'cargo-git')
        self.assertEqual(self.receipt()['path'], str(self.root / 'real-installed/bin/rustrace'))
        self.assertTrue((self.root / 'real-installed/bin/rustrace').is_file())


class SelectedAwkTests(unittest.TestCase):
    def run_suite(self, implementation):
        if shutil.which(implementation) is None:
            self.skipTest(f'{implementation} is not installed')
        env = dict(os.environ, RUSTRACE_TEST_AWK=implementation)
        result = subprocess.run([shutil.which('python3'), str(SCRIPT.parent.parent /
                                'tests/support/install_sh_tests.py'), 'InstallerTests'],
                                env=env, text=True, capture_output=True, timeout=600)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('test_selected_awk_is_on_the_hermetic_path', result.stderr)
        print(f'{implementation}: ' + result.stderr.strip().splitlines()[-1])

    def test_explicit_wrapper_selection(self):
        # Always exercise a non-default selection, even without gawk or mawk.
        with tempfile.TemporaryDirectory(prefix='rustrace-selected-awk-') as folder:
            wrapper = pathlib.Path(folder) / 'selected-awk'
            wrapper.write_text('#!/bin/sh\nexec ' + shutil.which('awk') + ' "$@"\n')
            wrapper.chmod(0o755)
            self.run_suite(str(wrapper))

    def test_system_awk(self):
        self.run_suite('awk')

    def test_gawk(self):
        self.run_suite('gawk')

    def test_mawk(self):
        self.run_suite('mawk')


if __name__ == '__main__':
    unittest.main(verbosity=2)
