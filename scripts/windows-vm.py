#!/usr/bin/env python3
"""Control the dedicated, already-provisioned Brick Windows smoke VM."""
import argparse
import base64
import datetime
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import time

ROOT = Path(os.environ.get('BRICK_SMOKE_VM_DIR', str(Path.home() / '.local/share/brick-windows-smoke')))
CONTAINER = 'brick-windows-smoke'


class Monitor:
    def __init__(self, guest=False, timeout=15):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.settimeout(timeout)
        self.socket.connect(str(ROOT / 'storage' / ('brick-qga.sock' if guest else 'brick-qmp.sock')))
        self.stream = self.socket.makefile('rwb', buffering=0)
        if not guest:
            self.stream.readline()
            self.call('qmp_capabilities')

    def call(self, command, arguments=None):
        request = {'execute': command}
        if arguments:
            request['arguments'] = arguments
        self.stream.write((json.dumps(request) + '\n').encode())
        while True:
            line = self.stream.readline().lstrip(b'\xff')
            if not line:
                raise RuntimeError('VM monitor disconnected')
            result = json.loads(line)
            if 'error' in result:
                raise RuntimeError(result['error'])
            if 'return' in result:
                return result['return']

    def powershell(self, script):
        encoded = base64.b64encode(script.encode('utf-16le')).decode()
        result = self.call('guest-exec', {
            'path': r'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe',
            'arg': ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', encoded],
            'capture-output': True,
        })
        for _ in range(60):
            status = self.call('guest-exec-status', {'pid': result['pid']})
            if status.get('exited'):
                for key in ('out-data', 'err-data'):
                    if key in status:
                        print(base64.b64decode(status[key]).decode('utf-8', 'replace'))
                if status.get('exitcode'):
                    raise RuntimeError(f"Guest PowerShell exited {status['exitcode']}")
                return
            time.sleep(0.25)
        raise RuntimeError(f"Guest command is still running (PID {result['pid']})")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    for name in ('start', 'stop', 'status', 'screenshot'):
        sub.add_parser(name)
    ps = sub.add_parser('ps', help='Execute a local PowerShell script through the private guest agent')
    ps.add_argument('script', type=Path)
    run = sub.add_parser('run', help='Install and smoke test a candidate in the unelevated desktop session')
    run.add_argument('installer', type=Path)
    run.add_argument('--software-gl', action='store_true', help='Use the prepared VM-only Mesa fixture')
    run.add_argument('--seconds', type=int, default=60)
    args = parser.parse_args()
    if args.command == 'start':
        subprocess.run(['docker', 'start', CONTAINER], check=True)
        for _ in range(60):
            result = subprocess.run(['docker', 'exec', CONTAINER, 'chown', f'{os.getuid()}:{os.getgid()}',
                                     '/storage/brick-qmp.sock', '/storage/brick-qga.sock'],
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            if result.returncode == 0:
                try:
                    Monitor(timeout=0.5).call('query-status')
                except (OSError, RuntimeError):
                    pass  # A stopped VM can leave stale socket files behind.
                else:
                    print('Desktop: http://127.0.0.1:28006 (Windows may still be booting)')
                    return
            time.sleep(0.5)
        raise RuntimeError('VM monitoring sockets did not become ready')
    if args.command == 'stop':
        subprocess.run(['docker', 'stop', CONTAINER], check=True)
        return
    if args.command == 'status':
        subprocess.run(['docker', 'inspect', '--format', '{{.State.Status}}', CONTAINER], check=True)
        print('Desktop: http://127.0.0.1:28006')
        return
    if args.command == 'ps':
        Monitor(guest=True).powershell(args.script.read_text())
        return
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%d-%H%M%S')
    if args.command == 'screenshot':
        name = f'desktop-{stamp}.png'
        Monitor().call('screendump', {'filename': f'/shared/results/{name}', 'format': 'png'})
        print(ROOT / 'shared/results' / name)
        return
    if args.seconds < 1:
        parser.error('--seconds must be positive')
    if not args.installer.is_file():
        parser.error('Installer does not exist')
    shared = ROOT / 'shared'
    installer_name = f'candidate-{stamp}.exe'
    shutil.copy2(args.installer, shared / 'artifacts' / installer_name)
    shutil.copy2(Path(__file__).with_name('windows-smoke.ps1'), shared / 'windows-smoke.ps1')
    gl = r' -SoftwareOpenGLDirectory \\host.lan\Data\mesa' if args.software_gl else ''
    script = rf'''
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Get-Process brick -ErrorAction SilentlyContinue | Stop-Process -Force
Copy-Item '\\host.lan\Data\windows-smoke.ps1' C:\OEM\windows-smoke.ps1 -Force
$action = New-ScheduledTaskAction -Execute powershell.exe -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\OEM\windows-smoke.ps1 -Installer \\host.lan\Data\artifacts\{installer_name} -OutputDirectory \\host.lan\Data\results\{stamp} -SampleSeconds {args.seconds}{gl}'
$principal = New-ScheduledTaskPrincipal -UserId BrickTest -LogonType Interactive -RunLevel Limited
Register-ScheduledTask -TaskName BrickSmoke -Action $action -Principal $principal -Force | Out-Null
Start-ScheduledTask BrickSmoke
'''
    Monitor(guest=True).powershell(script)
    print('Started desktop smoke test. Results:', shared / 'results' / stamp)
    print('Desktop: http://127.0.0.1:28006')


if __name__ == '__main__':
    main()
