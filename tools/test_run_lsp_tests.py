#!/usr/bin/env python3
"""Exercise the Docker test wrapper without Docker or a terminal."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


WRAPPER = Path(__file__).resolve().with_name("run_lsp_tests.sh")
MOCK_DOCKER = """#!/usr/bin/env python3
import json
import os
import sys

with open(os.environ["CHAKRA_TEST_DOCKER_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\\n")
if sys.argv[-3:] == ["test", "-f", "/seed/.keep"]:
    sys.exit(0 if os.environ["CHAKRA_TEST_VOLUME_SEEDED"] == "1" else 1)
if os.environ.get("CHAKRA_TEST_FAIL_PACKAGE") in sys.argv:
    sys.exit(1)
"""


class WrapperTests(unittest.TestCase):
    def run_wrapper(self, arguments=(), *, seeded=True, fail_package=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            docker = root / "docker"
            docker.write_text(MOCK_DOCKER)
            docker.chmod(0o755)
            log = root / "docker.jsonl"
            env = dict(os.environ)
            env["PATH"] = str(root) + os.pathsep + env["PATH"]
            env["CHAKRA_TEST_DOCKER_LOG"] = str(log)
            env["CHAKRA_TEST_VOLUME_SEEDED"] = "1" if seeded else "0"
            env.pop("CHAKRA_TEST_FAIL_PACKAGE", None)
            if fail_package:
                env["CHAKRA_TEST_FAIL_PACKAGE"] = fail_package
            # Explicitly use /bin/bash so macOS CI covers its shipped 3.2,
            # even if a newer Homebrew Bash happens to precede it on PATH.
            result = subprocess.run(
                ["/bin/bash", str(WRAPPER), *arguments],
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                env=env,
                timeout=10,
            )
            self.assertEqual(result.returncode, 1 if fail_package else 0, result.stdout + result.stderr)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
        self.assertEqual(calls[0][0], "build")
        self.assertTrue(all("-t" not in call for call in calls[1:]))
        tests = [call for call in calls if "cargo" in call]
        for call in tests:
            self.assertEqual(call[:5], ["run", "--rm", "--init", "--platform", "linux/amd64"])
        return calls, [call[call.index("cargo"):] for call in tests]

    def test_default_runs_workspace_then_only_real_provider_targets(self):
        _, tests = self.run_wrapper()
        self.assertEqual(tests[0], ["cargo", "test", "--locked", "--workspace"])
        packages = [
            "chakra-provider-rust-analyzer",
            "chakra-provider-clangd",
            "chakra-provider-csharp-ls",
            "chakra-provider-gopls",
            "chakra-provider-terraform-ls",
            "chakra-provider-pyright",
            "chakra-provider-vtsls",
            "chakra-provider-jdtls",
            "chakra-provider-bash-language-server",
            "chakra-provider-kotlin-lsp",
        ]
        self.assertEqual(tests[1:-1], [
            ["cargo", "test", "--locked", "-p", package, "--test", "real_provider", "--", "--ignored", "--test-threads=1"]
            for package in packages
        ])
        self.assertEqual(tests[-1], [
            "cargo", "test", "--locked", "-p", "chakra-provider-kotlin-lsp",
            "--lib", "import::tests::real_gradle_", "--", "--ignored", "--test-threads=1",
        ])

    def test_selective_arguments_are_preserved_and_empty_volume_is_seeded(self):
        arguments = ["-p", "chakra-provider-gopls", "--", "--ignored", "filter with spaces"]
        calls, tests = self.run_wrapper(arguments, seeded=False)
        self.assertEqual(tests, [["cargo", "test", "--locked", *arguments]])
        self.assertEqual(sum(call[-2:] == ["touch", "/seed/.keep"] for call in calls), 1)

    def test_failure_does_not_skip_later_providers(self):
        _, tests = self.run_wrapper(fail_package="chakra-provider-clangd")
        self.assertEqual(len(tests), 12)
        self.assertIn("chakra-provider-kotlin-lsp", tests[-1])

    def test_workspace_failure_is_retained_after_successful_providers(self):
        _, tests = self.run_wrapper(fail_package="--workspace")
        self.assertEqual(len(tests), 12)
        self.assertIn("import::tests::real_gradle_", tests[-1])

    def test_gradle_contract_failure_fails_the_default_gate(self):
        _, tests = self.run_wrapper(fail_package="import::tests::real_gradle_")
        self.assertEqual(len(tests), 12)


if __name__ == "__main__":
    unittest.main()
