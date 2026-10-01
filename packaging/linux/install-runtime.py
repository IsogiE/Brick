#!/usr/bin/env python3
"""Install only the reviewed, hash-pinned WebKit artifact in a disposable build VM."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.parse
import urllib.request

LIB = 'usr/lib/x86_64-linux-gnu/'
PREFIXES = ('usr/include/webkitgtk-4.1/', LIB + 'webkit2gtk-4.1/', 'usr/share/doc/brick-webkit-runtime/')

def allowed_file(name):
    return (any(name.startswith(prefix) for prefix in PREFIXES)
            or re.fullmatch(re.escape(LIB) + r'lib(?:webkit2gtk|javascriptcoregtk)-4\.1\.so(?:\.[0-9]+)*', name)
            or name in (LIB + 'pkgconfig/webkit2gtk-4.1.pc', LIB + 'pkgconfig/javascriptcoregtk-4.1.pc', LIB + 'pkgconfig/webkit2gtk-web-extension-4.1.pc', 'usr/bin/jsc')
            or re.fullmatch(r'usr/share/locale/[A-Za-z0-9_@.-]+/LC_MESSAGES/WebKitGTK-4\.1\.mo', name))

def check_lock(lock):
    assert lock.get('schema') == 1 and re.fullmatch(r'2\.[0-9]+\.[0-9]+', lock.get('version', '')), 'Invalid runtime lock'
    assert lock.get('distribution') == 'ubuntu-24.04' and lock.get('architecture') == 'amd64', 'Unexpected runtime target'
    version = lock['version']
    assert lock.get('tag') == f'runtime-webkitgtk-{version}-ubuntu24.04-amd64-r1', 'Unexpected runtime tag'
    assert lock.get('fileName') == f'brick-webkitgtk-{version}-ubuntu24.04-amd64.tar.xz', 'Unexpected runtime filename'
    assert lock.get('url') == f"https://github.com/IsogiE/Brick-Releases/releases/download/{lock['tag']}/{lock['fileName']}", 'Unexpected runtime URL'
    assert re.fullmatch(r'[a-f0-9]{64}', lock.get('sha256', '')), 'Missing runtime digest'
    assert type(lock.get('bytes')) is int and 0 < lock['bytes'] <= 256 * 1024**2, 'Invalid runtime size'
    dependencies = lock.get('dependencies')
    assert isinstance(dependencies, list) and 0 < len(dependencies) <= 150
    assert all(isinstance(name, str) and re.fullmatch(r'[a-z0-9][a-z0-9+.-]*(?::amd64)?', name) for name in dependencies), 'Invalid package name'
    return lock

class RuntimeRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        url = urllib.parse.urlsplit(newurl)
        if url.scheme != 'https' or url.hostname not in {'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com'}:
            raise ValueError('Unexpected runtime download redirect')
        return super().redirect_request(req, fp, code, msg, headers, newurl)

def download(lock, destination):
    opener = urllib.request.build_opener(RuntimeRedirect())
    request = urllib.request.Request(lock['url'], headers={'User-Agent': 'Brick-runtime-verifier'})
    size = 0
    with opener.open(request, timeout=60) as response, destination.open('xb') as output:
        while chunk := response.read(1024**2):
            size += len(chunk)
            if size > lock['bytes']:
                raise ValueError('Runtime download exceeds its pinned size')
            output.write(chunk)
    assert size == lock['bytes'], 'Truncated runtime download'

def verify_archive(archive, lock):
    assert archive.stat().st_size == lock['bytes'], 'Runtime archive size mismatch'
    with archive.open('rb') as stream:
        assert hashlib.file_digest(stream, 'sha256').hexdigest() == lock['sha256'], 'Runtime archive digest mismatch'
    members = []
    seen = set()
    total = 0
    with tarfile.open(archive, 'r:xz') as runtime:
        for member in runtime:
            name = member.name.rstrip('/')
            parts = PurePosixPath(name).parts
            assert name and not name.startswith('/') and '\\' not in name and '..' not in parts and '.' not in parts, 'Unsafe runtime path'
            assert name not in seen, 'Duplicate runtime archive path'
            seen.add(name)
            assert member.isfile() or member.isdir() or member.issym(), 'Unsupported runtime archive entry'
            assert not member.mode & 0o6000, 'Privileged runtime file mode'
            if member.isdir():
                assert name == 'usr' or name.startswith('usr/'), 'Unexpected runtime directory'
            else:
                assert allowed_file(name), 'Unexpected runtime file: ' + name
            if member.issym():
                assert '/' not in member.linkname and '\\' not in member.linkname and member.linkname not in ('', '.', '..'), 'Unsafe runtime symlink'
                assert allowed_file(str(PurePosixPath(name).parent / member.linkname)), 'Unexpected symlink target'
            if member.isfile():
                total += member.size
                assert 0 <= member.size <= 512 * 1024**2 and total <= 2 * 1024**3, 'Expanded runtime exceeds its limit'
            members.append(member)
            assert len(members) <= 20000, 'Too many runtime entries'
        required = {LIB + 'libwebkit2gtk-4.1.so', LIB + 'libjavascriptcoregtk-4.1.so',
                    LIB + 'webkit2gtk-4.1/WebKitWebProcess', LIB + 'webkit2gtk-4.1/WebKitNetworkProcess',
                    LIB + 'pkgconfig/webkit2gtk-4.1.pc', 'usr/share/doc/brick-webkit-runtime/copyright'}
        assert required <= seen, 'Runtime is missing a library, matching helper, metadata or license'
    return members

def install(archive, members, stage):
    with tarfile.open(archive, 'r:xz') as runtime:
        runtime.extractall(stage, members=members, filter='data')
    for member in sorted(members, key=lambda item: (len(PurePosixPath(item.name).parts), item.name)):
        source = stage / member.name
        target = Path('/') / member.name
        assert target.parent.resolve().is_relative_to(Path('/usr')) or target == Path('/usr'), 'Runtime target escaped /usr'
        if member.isdir():
            target.mkdir(exist_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        descriptor, temporary = tempfile.mkstemp(prefix='.brick-runtime-', dir=target.parent)
        os.close(descriptor)
        temporary = Path(temporary)
        try:
            if member.issym():
                temporary.unlink()
                temporary.symlink_to(member.linkname)
            else:
                shutil.copyfile(source, temporary)
                temporary.chmod(0o755 if member.mode & 0o111 else 0o644)
            temporary.replace(target)
        finally:
            temporary.unlink(missing_ok=True)
    subprocess.run(['/usr/sbin/ldconfig'], check=True)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--lock', type=Path, default=Path(__file__).with_name('runtime.lock.json'))
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--verify-only', action='store_true')
    args = parser.parse_args()
    lock = check_lock(json.loads(args.lock.read_text(encoding='utf-8-sig')))
    if not args.verify_only:
        os_release = platform.freedesktop_os_release()
        assert os.geteuid() == 0 and os_release.get('ID') == 'ubuntu' and os_release.get('VERSION_ID') == '24.04', 'Use an Ubuntu 24.04 build VM as root'
        assert platform.machine() == 'x86_64', 'Unexpected build architecture'
    scratch_parent = '/var/tmp' if Path('/var/tmp').is_dir() else None
    with tempfile.TemporaryDirectory(prefix='brick-prebuilt-runtime-', dir=scratch_parent) as temporary:
        root = Path(temporary)
        archive = args.archive or root / lock['fileName']
        if args.archive is None:
            download(lock, archive)
        members = verify_archive(archive, lock)
        if not args.verify_only:
            subprocess.run(['apt-get', 'install', '-y', '--no-install-recommends', *lock['dependencies']], check=True)
            stage = root / 'stage'
            stage.mkdir()
            install(archive, members, stage)
            actual = subprocess.check_output(['pkg-config', '--modversion', 'webkit2gtk-4.1'], text=True).strip()
            assert actual == lock['version'], 'Installed runtime version mismatch'
        print(json.dumps({'verified': True, 'installed': not args.verify_only, 'version': lock['version'], 'sha256': lock['sha256']}))

if __name__ == '__main__':
    main()
