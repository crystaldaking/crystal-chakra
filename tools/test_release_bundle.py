#!/usr/bin/env python3
"""Run the release workflow's asset assembly against an isolated bundle."""

import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = (ROOT / '.github/workflows/release.yml').read_text()
ASSEMBLE = textwrap.dedent(
    WORKFLOW.split('      - name: Verify archives and create checksum manifest\n', 1)[1]
    .split('        run: |\n', 1)[1]
    .split('      - name: Extract release notes\n', 1)[0]
)
SMOKE_VERIFY = textwrap.dedent(
    WORKFLOW.split('      - name: Smoke-test the Unix installer\n', 1)[1]
    .split('        run: |\n', 1)[1]
    .split('          fixture=', 1)[0]
)
TAG = 'v0.4.0'
ARCHIVES = [
    f'chakra-{TAG}-x86_64-unknown-linux-gnu.tar.gz',
    f'chakra-{TAG}-x86_64-apple-darwin.tar.gz',
    f'chakra-{TAG}-aarch64-apple-darwin.tar.gz',
    f'chakra-{TAG}-x86_64-pc-windows-msvc.zip',
]


class ReleaseBundleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='chakra-release-bundle-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'bundle').mkdir()
        (self.root / 'tools').mkdir()
        for name in ARCHIVES:
            (self.root / 'bundle' / name).write_bytes(name.encode())
        for name in ('install.sh', 'install.ps1'):
            (self.root / 'tools' / name).write_bytes((ROOT / 'tools' / name).read_bytes())

    def run_script(self, script):
        return subprocess.run(
            ['bash', '-euo', 'pipefail', '-c', script], cwd=self.root,
            env={**os.environ, 'RELEASE_TAG': TAG}, capture_output=True, text=True,
        )

    def test_manifest_covers_exact_archives_and_installer_bytes(self):
        result = self.run_script(ASSEMBLE)
        self.assertEqual(result.returncode, 0, result.stderr)
        entries = {}
        for line in (self.root / 'bundle/SHA256SUMS').read_text().splitlines():
            digest, name = line.split('  ', 1)
            entries[name] = digest
        self.assertEqual(set(entries), {*ARCHIVES, 'install.sh', 'install.ps1'})
        for name, digest in entries.items():
            self.assertEqual(digest, hashlib.sha256((self.root / 'bundle' / name).read_bytes()).hexdigest())
        for name in ('install.sh', 'install.ps1'):
            self.assertEqual((self.root / 'bundle' / name).read_bytes(), (self.root / 'tools' / name).read_bytes())
        result = self.run_script(SMOKE_VERIFY)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_missing_archive_rejects_bundle(self):
        (self.root / 'bundle' / ARCHIVES[0]).unlink()
        self.assertNotEqual(self.run_script(ASSEMBLE).returncode, 0)
        self.assertFalse((self.root / 'bundle/SHA256SUMS').exists())

    def test_unexpected_archive_rejects_bundle(self):
        (self.root / 'bundle/extra.tar.gz').write_bytes(b'unexpected')
        self.assertNotEqual(self.run_script(ASSEMBLE).returncode, 0)
        self.assertFalse((self.root / 'bundle/SHA256SUMS').exists())

    def test_modified_installer_rejected_before_smoke_execution(self):
        result = self.run_script(ASSEMBLE)
        self.assertEqual(result.returncode, 0, result.stderr)
        with (self.root / 'bundle/install.sh').open('ab') as installer:
            installer.write(b'\n# changed after assembly\n')
        result = self.run_script(SMOKE_VERIFY)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('install.sh: FAILED', result.stdout)


if __name__ == '__main__':
    unittest.main()
