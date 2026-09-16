"""Behavioral release contract tests, also run by cargo test."""
import json
import os
import tempfile
import pathlib
import subprocess
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
VERSION = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
OVERRIDE_VERSION = '0.2.0' if VERSION != '0.2.0' else '0.3.0'


class TagTests(unittest.TestCase):
    def test_matching_stable_tag(self):
        result = subprocess.run([str(ROOT / 'scripts/check-release-tag.sh'), 'v' + VERSION], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())

    def test_rejects_nonmatching_or_unstable_tags(self):
        for tag in ['v999.999.999', VERSION, 'v' + VERSION + '-rc.1', 'v' + VERSION + '+build', 'v01.0.0', 'v1.2', 'v1.2.3.4', 'v1.2.3\n']:
            with self.subTest(tag=tag):
                result = subprocess.run([str(ROOT / 'scripts/check-release-tag.sh'), tag], capture_output=True)
                self.assertNotEqual(result.returncode, 0)

    def test_manifest_override_controls_the_checked_version(self):
        with tempfile.TemporaryDirectory(prefix="rustrace-tag-") as root:
            manifest = pathlib.Path(root) / "Cargo.toml"
            manifest.write_text(f'[workspace.package]\nversion = "{OVERRIDE_VERSION}"\n')
            env = {**os.environ, "RUSTRACE_CARGO_MANIFEST": str(manifest)}
            result = subprocess.run([str(ROOT / 'scripts/check-release-tag.sh'), 'v' + OVERRIDE_VERSION], env=env, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            result = subprocess.run([str(ROOT / 'scripts/check-release-tag.sh'), 'v' + VERSION], env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)

    def test_rejects_unstable_tag_even_when_it_matches_the_manifest(self):
        with tempfile.TemporaryDirectory(prefix="rustrace-tag-") as root:
            manifest = pathlib.Path(root) / "Cargo.toml"
            for version in ["0.2.0-rc.1", "0.2.0+build", "01.2.0"]:
                with self.subTest(version=version):
                    manifest.write_text(f'[workspace.package]\nversion = "{version}"\n')
                    env = {**os.environ, "RUSTRACE_CARGO_MANIFEST": str(manifest)}
                    result = subprocess.run([str(ROOT / 'scripts/check-release-tag.sh'), 'v' + version], env=env, capture_output=True)
                    self.assertNotEqual(result.returncode, 0)

    def test_requires_one_argument(self):
        for args in [[], ['v' + VERSION, 'extra']]:
            result = subprocess.run([str(ROOT / 'scripts/check-release-tag.sh'), *args], capture_output=True)
            self.assertEqual(result.returncode, 2)


class ManifestTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="rustrace-manifest-")
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.commit = subprocess.check_output(["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip()
        self.dirs = []
        for target in ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]:
            directory = self.root / target
            directory.mkdir()
            (directory / "version.txt").write_text(f"rustrace {VERSION}\nbuild commit: {self.commit}\nevent format: 1\npackage format: 1\nassignment format: 2\ntarget: {target}\n")
            self.dirs.append(directory)
        self.expected = {
            "schema_version": 1, "version": VERSION, "tag": "v" + VERSION,
            "commit": self.commit, "event_format": 1, "package_format": 1, "assignment_format": 2,
            "source": {"repository": "https://github.com/baochunli/rustrace", "tag": "v" + VERSION}, "targets": {},
        }

    def run_manifest(self, *args):
        return subprocess.run([str(ROOT / "scripts/manifest.sh"), *map(str, args)], capture_output=True)

    def test_exact_source_manifest_and_verification(self):
        encoded = (json.dumps(self.expected, sort_keys=True, indent=2) + "\n").encode()
        for dirs in [self.dirs, self.dirs[::-1], self.dirs[:1]]:
            result = self.run_manifest("v" + VERSION, *dirs)
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(result.stdout, encoded)
        manifest = self.root / "latest.json"
        manifest.write_bytes(encoded)
        result = self.run_manifest("--verify", manifest, "v" + VERSION, *self.dirs)
        self.assertEqual(result.returncode, 0, result.stderr.decode())

    def test_manifest_keeps_root_cargo_identity_when_checker_override_is_set(self):
        fixture = self.root / "Cargo.toml"
        fixture.write_text(f'[workspace.package]\nversion = "{OVERRIDE_VERSION}"\n')
        result = subprocess.run(
            [str(ROOT / "scripts/manifest.sh"), "v" + OVERRIDE_VERSION, *map(str, self.dirs)],
            env={**os.environ, "RUSTRACE_CARGO_MANIFEST": str(fixture)}, capture_output=True,
        )
        self.assertNotEqual(result.returncode, 0)

    def test_formats_are_read_from_verbose_metadata(self):
        for directory in self.dirs:
            path = directory / "version.txt"
            path.write_text(path.read_text().replace("event format: 1", "event format: 7").replace("package format: 1", "package format: 9").replace("assignment format: 2", "assignment format: 11"))
        result = self.run_manifest("v" + VERSION, *self.dirs)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        actual = json.loads(result.stdout)
        self.assertEqual((actual["event_format"], actual["package_format"], actual["assignment_format"]), (7, 9, 11))

    def test_rejects_wrong_tag_and_mismatched_metadata(self):
        self.assertNotEqual(self.run_manifest("v999.999.999", *self.dirs).returncode, 0)
        path = self.dirs[0] / "version.txt"
        original = path.read_text()
        for old, new in [("rustrace " + VERSION, "rustrace 999.999.999"), (self.commit, "b" * 40), (self.commit, "unknown"), ("package format: 1", "package format: 2"), ("assignment format: 2", "assignment format: 1"), ("event format: 1", "event format: 0"), ("target: aarch64-apple-darwin", "target: x86_64-unknown-linux-gnu")]:
            with self.subTest(change=new):
                path.write_text(original.replace(old, new))
                self.assertNotEqual(self.run_manifest("v" + VERSION, *self.dirs).returncode, 0)
        for contents in [original + "build commit: " + self.commit + "\n", original.replace("build commit: " + self.commit + "\n", ""), original.replace("assignment format: 2\n", "")]:
            path.write_text(contents)
            self.assertNotEqual(self.run_manifest("v" + VERSION, *self.dirs).returncode, 0)
        path.unlink()
        self.assertNotEqual(self.run_manifest("v" + VERSION, *self.dirs).returncode, 0)

    def test_rejects_modified_manifest_and_noncanonical_json(self):
        manifest = self.root / "latest.json"
        for key, value in [("version", "999.999.999"), ("tag", "v999.999.999"), ("commit", "b" * 40), ("event_format", 2), ("package_format", 2), ("assignment_format", 1), ("targets", {"unexpected": {}}), ("source", {"repository": "https://example.com", "tag": "v" + VERSION})]:
            with self.subTest(key=key):
                data = {**self.expected, key: value}
                manifest.write_text(json.dumps(data, sort_keys=True, indent=2) + "\n")
                self.assertNotEqual(self.run_manifest("--verify", manifest, "v" + VERSION, *self.dirs).returncode, 0)
        manifest.write_text(json.dumps(self.expected))
        self.assertNotEqual(self.run_manifest("--verify", manifest, "v" + VERSION, *self.dirs).returncode, 0)

    def test_requires_tag_and_metadata(self):
        for args in [[], ["v" + VERSION], ["--verify"], ["--verify", "missing", "v" + VERSION]]:
            self.assertEqual(self.run_manifest(*args).returncode, 2)


if __name__ == '__main__':
    unittest.main()
