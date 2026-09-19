#!/usr/bin/env python3
"""Exercise the workflow's actual manual-preflight identity checks."""

import os
from pathlib import Path
import subprocess
import textwrap
import unittest


class ReleasePreflightTests(unittest.TestCase):
    def test_only_main_and_matching_release_branches_pass(self):
        workflow = (Path(__file__).resolve().parents[1] / '.github/workflows/release.yml').read_text()
        script = textwrap.dedent(workflow.split('        run: |\n', 1)[1].split('          version="${tag#v}"', 1)[0])
        cases = [
            ('main', 'branch', 'v0.4.0', True),
            ('release/0.4.0', 'branch', 'v0.4.0', True),
            ('release/v0.4.0', 'branch', 'v0.4.0', True),
            ('release/0.3.2', 'branch', 'v0.4.0', False),
            ('develop', 'branch', 'v0.4.0', False),
            ('fix/example', 'branch', 'v0.4.0', False),
            ('main', 'tag', 'v0.4.0', False),
            ('release/v0.4.0', 'tag', 'v0.4.0', False),
            ('main', 'branch', 'v0.4.0-rc1', False),
            ('main', 'branch', '0.4.0', False),
        ]
        for branch, ref_type, tag, expected in cases:
            with self.subTest(branch=branch, ref_type=ref_type, tag=tag):
                result = subprocess.run(
                    ['bash', '-eu', '-c', script], capture_output=True, text=True,
                    env={**os.environ, 'GITHUB_EVENT_NAME': 'workflow_dispatch',
                         'GITHUB_REF_NAME': branch, 'GITHUB_REF_TYPE': ref_type,
                         'DISPATCH_TAG': tag},
                )
                self.assertEqual(result.returncode == 0, expected, result.stderr)


if __name__ == '__main__':
    unittest.main()
