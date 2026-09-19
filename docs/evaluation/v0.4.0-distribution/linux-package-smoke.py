import hashlib, json, os, platform, shutil, subprocess, tarfile, tempfile
from pathlib import Path
root = Path('/workspace')
out = Path('/evidence')
source = root / 'target/release/chakra'
records = []
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()
def run(label, args, env=None, expected=0):
    p = subprocess.run(args, text=True, capture_output=True, env=env, timeout=90)
    records.append({'step': label, 'exit_code': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr})
    (out / 'steps.json').write_text(json.dumps(records, indent=2) + '\n')
    if expected is None:
        assert p.returncode != 0, label
    else:
        assert p.returncode == expected, (label, p.returncode, p.stdout, p.stderr)
    return p
assert platform.system() == 'Linux' and platform.machine() == 'x86_64'
assert run('built-version', [str(source), '--version']).stdout.strip() == 'chakra 0.4.0'
run('runtime-linkage', ['ldd', str(source)])
bundle = 'chakra-v0.4.0-x86_64-unknown-linux-gnu'
with tempfile.TemporaryDirectory(prefix='chakra-linux-package-') as tmp:
    base = Path(tmp)
    download = base / 'download/v0.4.0'
    download.mkdir(parents=True)
    package = base / bundle
    package.mkdir()
    for src, name in [(source, 'chakra'), (root/'LICENSE', 'LICENSE'), (root/'README.md', 'README.md')]:
        shutil.copy2(src, package / name)
    archive = download / (bundle + '.tar.gz')
    with tarfile.open(archive, 'w:gz') as tar:
        tar.add(package, arcname=bundle)
    checksum = download/'SHA256SUMS'
    for name in ('install.sh', 'install.ps1'):
        shutil.copy2(root/'tools'/name, out/name)
    good_manifest = f'{digest(archive)}  {archive.name}\n' + ''.join(
        f'{digest(out/name)}  {name}\n' for name in ('install.sh', 'install.ps1'))
    checksum.write_text(good_manifest)
    shutil.copy2(archive, out/archive.name)
    shutil.copy2(checksum, out/'SHA256SUMS')
    run('bundle-checksums', ['sh', '-c', 'cd /evidence && sha256sum --check SHA256SUMS'])
    home = base/'home'
    home.mkdir()
    original_rc = '# User configuration retained by installer\nexport CHAKRA_SMOKE_CANARY=preserved\n'
    (home/'.bashrc').write_text(original_rc)
    env = {'HOME': str(home), 'PATH': '/usr/bin:/bin', 'SHELL': '/bin/bash', 'CHAKRA_UPDATE_CHECK': '0'}
    command = ['sh', str(out/'install.sh'), '--version', 'v0.4.0', '--base-url', base.as_uri()]
    run('clean-install', command, env)
    installed = home/'.local/bin/chakra'
    assert digest(installed) == digest(source)
    fresh = run('fresh-interactive-bash', ['/bin/bash', '-ic', 'test "$CHAKRA_SMOKE_CANARY" = preserved && command -v chakra && chakra --version'], env)
    assert fresh.stdout.splitlines() == [str(installed), 'chakra 0.4.0'], fresh.stdout
    run('repeat-install', command, env)
    rc_bytes = (home/'.bashrc').read_bytes()
    assert rc_bytes.decode().startswith(original_rc)
    assert rc_bytes.count(b'# >>> chakra path >>>') == 1
    assert digest(installed) == digest(source)
    checksum.write_text('0'*64+'  '+archive.name+'\n')
    failure = run('checksum-failure', command, env, expected=None)
    assert 'checksum' in failure.stderr.lower()
    assert digest(installed) == digest(source)
    assert (home/'.bashrc').read_bytes() == rc_bytes
    checksum.write_text(good_manifest)
    archive.rename(archive.with_suffix('.unavailable'))
    failure = run('missing-download', command, env, expected=None)
    assert 'download failed' in failure.stderr.lower()
    assert digest(installed) == digest(source)
    assert (home/'.bashrc').read_bytes() == rc_bytes
    run('preserved-version', [str(installed), '--version'], env)
    assert not list(installed.parent.glob('.chakra.new.*'))
    # A manual check deliberately still runs with automatic checks disabled.
    update = run('public-update-current', [str(installed), 'update', '--check'], env)
    assert 'installed: 0.4.0' in update.stdout and 'chakra is up to date' in update.stdout
    assert digest(installed) == digest(source)
    (out/'result.json').write_text(json.dumps({
        'status': 'pass', 'target': 'x86_64-unknown-linux-gnu', 'version': '0.4.0',
        'execution': 'Linux/amd64 Docker container on macOS ARM64; not physical x86-64 native runner',
        'binary_sha256': digest(source), 'archive_sha256': digest(out/archive.name),
        'installer_sha256': {name: digest(out/name) for name in ('install.sh', 'install.ps1')},
        'git_head': subprocess.check_output(['git','-C',str(root),'rev-parse','HEAD'],text=True).strip(),
        'source_state': 'uncommitted candidate; not final release provenance',
        'controlled_newer_release': 'not covered', 'steps': [r['step'] for r in records]
    }, indent=2)+'\n')
print('PASS real Linux x86-64 package/install/fresh Bash/reinstall/checksum and download preservation/public update discovery')
