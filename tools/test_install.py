#!/usr/bin/env python3
"""Hermetic tests for tools/install.sh (issue #203).

Builds a fixture GitHub release layout on the local filesystem (file://)
and exercises clean install, idempotent PATH setup, upgrade, downgrade
guard, checksum failure preservation, unsupported platforms, custom
directories, --no-path-modify, unrecognized shells, and PATH conflicts.
"""

import hashlib
import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
INSTALL_SH = REPO_ROOT / "tools" / "install.sh"
REPO = "crystaldaking/crystal-chakra"


def host_target() -> str:
    system = platform.system()
    machine = platform.machine()
    if system == "Linux" and machine == "x86_64":
        return "x86_64-unknown-linux-gnu"
    if system == "Darwin" and machine == "arm64":
        return "aarch64-apple-darwin"
    if system == "Darwin" and machine == "x86_64":
        return "x86_64-apple-darwin"
    raise AssertionError(f"test host not in the release matrix: {system}/{machine}")


def make_fixture(base: Path, version: str, target: str, corrupt: bool = False,
                 binary_text: str | None = None) -> Path:
    """Create a fixture release: archive, SHA256SUMS, and the latest-API path."""
    bundle = base / f"chakra-{version}-{target}"
    bundle.mkdir(parents=True)
    binary = bundle / "chakra"
    binary.write_text(binary_text if binary_text is not None else
                      f'#!/bin/sh\necho "chakra {version.lstrip("v")}"\n')
    binary.chmod(binary.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    (bundle / "LICENSE").write_text("MIT\n")
    (bundle / "README.md").write_text("fixture\n")

    download = base / "download" / version
    download.mkdir(parents=True)
    archive = download / f"chakra-{version}-{target}.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        tar.add(bundle, arcname=bundle.name)

    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    if corrupt:
        digest = "0" * 64
    (download / "SHA256SUMS").write_text(f"{digest}  {archive.name}\n")

    api = base / "repos" / REPO / "releases"
    api.mkdir(parents=True)
    (api / "latest").write_text(json.dumps({"tag_name": version}))
    return base


class InstallEnv:
    def __init__(self, tmp: Path, shell: str = "/bin/zsh"):
        self.home = tmp / "home"
        self.home.mkdir()
        self.install_dir = self.home / ".local" / "bin"
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.home),
            "SHELL": shell,
            "TMPDIR": str(tmp / "tmp"),
        }
        (tmp / "tmp").mkdir()

    def run(self, fixture: Path, *args: str, extra_env: dict | None = None) -> subprocess.CompletedProcess:
        env = dict(self.env)
        if extra_env:
            env.update(extra_env)
        return subprocess.run(
            ["sh", str(INSTALL_SH), "--base-url", fixture.as_uri(), *args],
            capture_output=True,
            text=True,
            env=env,
        )

    @property
    def binary(self) -> Path:
        return self.install_dir / "chakra"

    def rc(self, shell: str = "/bin/zsh") -> Path:
        if shell.endswith("zsh"):
            return self.home / ".zshrc"
        if (self.home / ".bashrc").exists():
            return self.home / ".bashrc"
        return self.home / ".bash_profile"

    def version(self) -> str:
        return subprocess.run(
            [str(self.binary), "--version"], capture_output=True, text=True
        ).stdout.strip()


def test_clean_install(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    result = env.run(fixture)
    assert result.returncode == 0, result.stderr
    assert env.binary.exists()
    assert env.version() == "chakra 0.4.0"
    rc = env.rc()
    assert rc.exists()
    assert rc.read_text().count("# >>> chakra path >>>") == 1
    assert "new terminal" in result.stdout


def test_repeat_install_is_idempotent(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    assert env.run(fixture).returncode == 0
    assert env.run(fixture).returncode == 0
    assert env.rc().read_text().count("# >>> chakra path >>>") == 1


def test_custom_dir_and_existing_path_entry(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    custom = tmp / "custom bin"
    custom.mkdir()
    env.env["PATH"] = f"{custom}:{env.env['PATH']}"
    result = env.run(fixture, "--dir", str(custom))
    assert result.returncode == 0, result.stderr
    assert (custom / "chakra").exists()
    assert not env.rc().exists(), "no PATH block needed when dir is already on PATH"


def test_upgrade_and_downgrade_guard(tmp: Path) -> None:
    target = host_target()
    env = InstallEnv(tmp)
    fixture_new = make_fixture(tmp / "new", "v0.5.0", target)
    assert env.run(fixture_new).returncode == 0
    assert env.version() == "chakra 0.5.0"

    fixture_old = make_fixture(tmp / "old", "v0.4.0", target)
    result = env.run(fixture_old)
    assert result.returncode != 0, "implicit downgrade must fail"
    assert "newer" in result.stderr
    assert env.version() == "chakra 0.5.0", "previous binary preserved"

    result = env.run(fixture_old, "--version", "v0.4.0")
    assert result.returncode == 0, result.stderr
    assert env.version() == "chakra 0.4.0", "explicit downgrade allowed"


def test_checksum_failure_preserves_installation(tmp: Path) -> None:
    target = host_target()
    env = InstallEnv(tmp)
    good = make_fixture(tmp / "good", "v0.4.0", target)
    assert env.run(good).returncode == 0
    rc_before = env.rc().read_text()

    bad = make_fixture(tmp / "bad", "v0.5.0", target, corrupt=True)
    result = env.run(bad, "--version", "v0.5.0")
    assert result.returncode != 0
    assert "checksum" in result.stderr
    assert env.version() == "chakra 0.4.0", "previous binary preserved"
    assert env.rc().read_text() == rc_before, "PATH file untouched"


def test_unsupported_platform_fails_early(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    result = env.run(fixture, extra_env={"CHAKRA_OS": "FreeBSD"})
    assert result.returncode != 0
    assert "unsupported platform" in result.stderr
    assert not env.binary.exists()
    assert not env.rc().exists()


def test_runtime_or_version_failure_preserves_installation(tmp: Path) -> None:
    target = host_target()
    env = InstallEnv(tmp)
    good = make_fixture(tmp / "good", "v0.4.0", target)
    assert env.run(good).returncode == 0
    binary_before = env.binary.read_bytes()
    rc_before = env.rc().read_bytes()
    candidates = [
        "#!/bin/sh\necho simulated-loader-failure >&2\nexit 126\n",
        "#!/bin/sh\necho 'chakra 0.5.0'\nexit 1\n",
        "#!/bin/sh\necho 'chakra 0.3.0'\n",
    ]
    for index, candidate in enumerate(candidates):
        bad = make_fixture(tmp / f"bad-{index}", "v0.5.0", target, binary_text=candidate)
        result = env.run(bad, "--version", "v0.5.0")
        assert result.returncode != 0, result.stdout
        assert env.binary.read_bytes() == binary_before, result.stderr
        assert env.rc().read_bytes() == rc_before
        assert env.version() == "chakra 0.4.0"
        assert not list(env.install_dir.glob(".chakra.new.*")), "staging file cleanup"


def test_release_workflow_fresh_shell_checks_requested_version(tmp: Path) -> None:
    # Execute the actual workflow snippet, not a second copy of its env setup.
    # This catches losing RELEASE_TAG at the env -i boundary.
    if shutil.which("zsh") is None:
        print("skip fresh-shell smoke: zsh unavailable")
        return
    fixture = make_fixture(tmp / "fixture", "v0.4.0", host_target())
    env = InstallEnv(tmp)
    assert env.run(fixture).returncode == 0
    workflow = (REPO_ROOT / ".github/workflows/release.yml").read_text()
    section = workflow.split("# Fresh-shell PATH resolution through the managed block.\n", 1)[1]
    snippet = "\n".join(section.splitlines()[:2])
    for tag, success in [("v0.4.0", True), ("v0.5.0", False)]:
        result = subprocess.run(["bash", "-eu", "-c", snippet], text=True, capture_output=True,
                                env={**env.env, "smoke_home": str(env.home), "RELEASE_TAG": tag})
        assert (result.returncode == 0) == success, (result.stdout, result.stderr)


def test_foreign_binary_with_non_release_version_does_not_crash(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    env.install_dir.mkdir(parents=True)
    foreign = env.binary
    foreign.write_text("#!/bin/sh\necho 'chakra 0.4.0-beta-local'\n")
    foreign.chmod(0o755)
    result = env.run(fixture)
    assert result.returncode == 0, result.stderr
    assert "downgrade guard" in result.stdout
    assert env.version() == "chakra 0.4.0"


def test_no_path_modify(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    result = env.run(fixture, "--no-path-modify")
    assert result.returncode == 0, result.stderr
    assert not env.rc().exists()


def test_unrecognized_shell_gets_manual_instructions(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp, shell="/bin/fish")
    result = env.run(fixture)
    assert result.returncode == 0, result.stderr
    assert "manually" in result.stdout
    assert not (env.home / ".config").exists()


def test_conflicting_path_entry_warns(tmp: Path) -> None:
    target = host_target()
    fixture = make_fixture(tmp / "fixture", "v0.4.0", target)
    env = InstallEnv(tmp)
    shadow = tmp / "shadow"
    shadow.mkdir()
    fake = shadow / "chakra"
    fake.write_text("#!/bin/sh\necho shadow\n")
    fake.chmod(0o755)
    env.env["PATH"] = f"{shadow}:{env.env['PATH']}"
    result = env.run(fixture)
    assert result.returncode == 0, result.stderr
    assert "shadow" in result.stdout or "warning" in result.stdout


def main() -> int:
    tests = [
        value
        for name, value in sorted(globals().items())
        if name.startswith("test_") and callable(value)
    ]
    failures = 0
    for test in tests:
        with tempfile.TemporaryDirectory() as directory:
            try:
                test(Path(directory))
            except AssertionError as error:
                failures += 1
                print(f"FAIL {test.__name__}: {error}", file=sys.stderr)
            else:
                print(f"ok {test.__name__}")
    if failures:
        print(f"{failures} of {len(tests)} install tests failed", file=sys.stderr)
        return 1
    print(f"all {len(tests)} install tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
