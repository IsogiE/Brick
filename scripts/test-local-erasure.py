"""Test device reset against synthetic data in a private filesystem and bus.

The ordinary keyring tests intentionally do not reset an entire application.
This fixture requires a separate mount namespace, HOME and Secret Service.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


BOOTSTRAP = r'''
from pathlib import Path
import subprocess, time
for name in ['home', 'config', 'cache', 'data', 'runtime']:
    (Path('/fixture') / name).mkdir(mode=0o700, exist_ok=True)
subprocess.run(['dbus-daemon', '--session', '--address=unix:path=/fixture/bus', '--fork'], check=True)
result = subprocess.run([
    'gnome-keyring-daemon', '--unlock', '--components=secrets',
    '--control-directory=/fixture/runtime/keyring',
], input=b'synthetic-test-password\n', capture_output=True, timeout=15)
if result.returncode:
    raise RuntimeError('Disposable Secret Service did not start')
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    try:
        ready = subprocess.run([
            'gdbus', 'call', '--session', '--timeout', '1',
            '--dest', 'org.freedesktop.secrets',
            '--object-path', '/org/freedesktop/secrets/aliases/default',
            '--method', 'org.freedesktop.DBus.Properties.Get',
            'org.freedesktop.Secret.Collection', 'Locked',
        ], capture_output=True, text=True, timeout=2)
        if ready.returncode == 0 and 'false' in ready.stdout:
            break
    except subprocess.TimeoutExpired:
        pass
    time.sleep(0.1)
else:
    raise RuntimeError('Disposable Secret Service did not become ready')
result = subprocess.run([
    '/fixture-test', '--exact',
    'credential_store::tests::linux_device_reset_removes_orphan_namespaces_and_fences_late_saves',
    '--ignored', '--nocapture',
], timeout=55)
raise SystemExit(result.returncode)
'''


def test_binary(args):
    if args.test_binary:
        path = Path(args.test_binary)
    else:
        matches = []
        for line in Path(args.cargo_artifacts).read_text().splitlines():
            value = json.loads(line)
            if (value.get('reason') == 'compiler-artifact'
                    and value.get('target', {}).get('name') == 'brick'
                    and value.get('profile', {}).get('test')
                    and value.get('executable')):
                matches.append(Path(value['executable']))
        if len(matches) != 1:
            raise RuntimeError('Expected exactly one compiled Brick test executable')
        path = matches[0]
    path = path.resolve(strict=True)
    if not path.is_file():
        raise RuntimeError('Expected a compiled Brick test executable')
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--test-binary')
    group.add_argument('--cargo-artifacts')
    parser.add_argument('--scratch-parent', default=os.environ.get('RUNNER_TEMP'))
    args = parser.parse_args()
    binary = test_binary(args)
    with tempfile.TemporaryDirectory(prefix='brick-erasure-', dir=args.scratch_parent) as temporary:
        root = Path(temporary)
        (root / 'bootstrap.py').write_text(BOOTSTRAP)
        (root / 'passwd').write_text(
            f'fixture:x:{os.getuid()}:{os.getgid()}:Fixture:/fixture/home:/bin/sh\n')
        (root / 'group').write_text(f'fixture:x:{os.getgid()}:\n')
        command = [
            'bwrap', '--die-with-parent', '--new-session', '--unshare-all', '--clearenv',
            '--ro-bind', '/usr', '/usr', '--ro-bind', '/lib', '/lib',
            '--ro-bind', '/lib64', '/lib64', '--symlink', 'usr/bin', '/bin',
            '--dir', '/etc', '--ro-bind', str(root / 'passwd'), '/etc/passwd',
            '--ro-bind', str(root / 'group'), '/etc/group',
            '--ro-bind', '/etc/machine-id', '/etc/machine-id',
            '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp', '--dir', '/run',
            '--bind', str(root), '/fixture', '--ro-bind', str(binary), '/fixture-test',
        ]
        for name, value in {
            'HOME': '/fixture/home', 'XDG_CONFIG_HOME': '/fixture/config',
            'XDG_DATA_HOME': '/fixture/data', 'XDG_CACHE_HOME': '/fixture/cache',
            'XDG_RUNTIME_DIR': '/fixture/runtime',
            'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/fixture/bus',
            'BRICK_LOCAL_ERASURE_FIXTURE': '1', 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8',
        }.items():
            command += ['--setenv', name, value]
        command += ['--chdir', '/fixture', '/usr/bin/python3', '/fixture/bootstrap.py']
        subprocess.run(command, check=True, timeout=90)


if __name__ == '__main__':
    main()
