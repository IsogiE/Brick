"""Test device reset against synthetic data in a private filesystem and bus.

The ordinary keyring tests intentionally do not reset an entire application.
This fixture requires a separate mount namespace, HOME and Secret Service.
"""
import argparse
import json
import os
import shutil
from pathlib import Path
import subprocess
import tempfile


BOOTSTRAP = r'''
from pathlib import Path
import os, subprocess, time
if os.getuid() == 0 or os.getgid() == 0:
    raise RuntimeError('Fixture must run as an unprivileged user')
status = dict(line.split(':', 1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
if any(int(status[name].strip(), 16) for name in ['CapEff', 'CapPrm', 'CapAmb']):
    raise RuntimeError('Fixture must run without capabilities')
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
    parser.add_argument('--isolated-network', action='store_true',
                        help='Use an existing namespace with only the loopback interface.')
    parser.add_argument('--privileged-setup', action='store_true',
                        help='Create namespaces as root, then run as an unprivileged fixture user.')
    args = parser.parse_args()
    if args.privileged_setup:
        if os.geteuid() != 0 or not args.isolated_network:
            raise RuntimeError('Privileged setup requires root in an isolated network namespace')
        # Use a non-root fixture identity without assuming the runner's UID.
        fixture_uid = int(os.environ['SUDO_UID'])
        fixture_gid = int(os.environ['SUDO_GID'])
        if fixture_uid <= 0 or fixture_gid <= 0:
            raise RuntimeError('Privileged setup requires an unprivileged invoking user')
    else:
        if os.geteuid() == 0:
            raise RuntimeError('Root invocation requires explicit privileged fixture setup')
        fixture_uid, fixture_gid = os.getuid(), os.getgid()
    if args.isolated_network:
        # CI creates this namespace before bwrap. Some nested runner policies
        # prevent an unprivileged process from configuring loopback itself.
        # Never share an ordinary runner/host network as a fallback.
        interfaces = [line.split(':', 1)[0].strip()
                      for line in Path('/proc/self/net/dev').read_text().splitlines()[2:]
                      if ':' in line]
        if interfaces != ['lo']:
            raise RuntimeError('Expected an isolated loopback-only network namespace')
    binary = test_binary(args)
    # Privileged bwrap maps the fixture identity to the invoking root UID,
    # then drops capabilities before resolving bind sources. It cannot cross
    # a private runner home owned by another UID. Stage only fixture inputs
    # outside that home, retaining mode 0700 and deterministic cleanup.
    scratch_parent = '/var/tmp' if args.privileged_setup else args.scratch_parent
    with tempfile.TemporaryDirectory(prefix='brick-erasure-', dir=scratch_parent) as temporary:
        root = Path(temporary)
        if args.privileged_setup:
            staged_binary = root / 'test-binary'
            shutil.copyfile(binary, staged_binary)
            staged_binary.chmod(0o500)
            binary = staged_binary
        (root / 'bootstrap.py').write_text(BOOTSTRAP)
        (root / 'passwd').write_text(
            f'fixture:x:{fixture_uid}:{fixture_gid}:Fixture:/fixture/home:/bin/sh\n')
        (root / 'group').write_text(f'fixture:x:{fixture_gid}:\n')
        command = [
            'bwrap', '--die-with-parent', '--new-session', '--unshare-all', '--clearenv',
            '--uid', str(fixture_uid), '--gid', str(fixture_gid), '--cap-drop', 'ALL',
            '--ro-bind', '/usr', '/usr', '--ro-bind', '/lib', '/lib',
            '--ro-bind', '/lib64', '/lib64', '--symlink', 'usr/bin', '/bin',
            '--dir', '/etc', '--ro-bind', str(root / 'passwd'), '/etc/passwd',
            '--ro-bind', str(root / 'group'), '/etc/group',
            '--ro-bind', '/etc/machine-id', '/etc/machine-id',
            '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp', '--dir', '/run',
            '--bind', str(root), '/fixture', '--ro-bind', str(binary), '/fixture-test',
        ]
        if args.isolated_network:
            command += ['--share-net']
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
