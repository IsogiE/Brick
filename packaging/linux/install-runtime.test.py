import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('runtime_installer', Path(__file__).with_name('install-runtime.py'))
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)

class RuntimeArchiveTests(unittest.TestCase):
    def archive(self, extras=()):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        archive = Path(directory.name) / 'runtime.tar.xz'
        names = [installer.LIB + 'libwebkit2gtk-4.1.so', installer.LIB + 'libjavascriptcoregtk-4.1.so',
                 installer.LIB + 'webkit2gtk-4.1/WebKitWebProcess', installer.LIB + 'webkit2gtk-4.1/WebKitNetworkProcess',
                 installer.LIB + 'pkgconfig/webkit2gtk-4.1.pc', 'usr/share/doc/brick-webkit-runtime/copyright']
        with tarfile.open(archive, 'w:xz') as output:
            for name in names:
                member = tarfile.TarInfo(name)
                member.size = 3
                member.mode = 0o644
                output.addfile(member, io.BytesIO(b'abc'))
            for member in extras:
                output.addfile(member, io.BytesIO(b'') if member.isfile() else None)
        lock = {'bytes': archive.stat().st_size, 'sha256': hashlib.sha256(archive.read_bytes()).hexdigest()}
        return archive, lock

    def test_valid_archive_without_execution(self):
        archive, lock = self.archive()
        self.assertEqual(len(installer.verify_archive(archive, lock)), 6)

    def test_digest_change_is_rejected_before_extraction(self):
        archive, lock = self.archive()
        lock['sha256'] = '0' * 64
        with self.assertRaisesRegex(AssertionError, 'digest mismatch'):
            installer.verify_archive(archive, lock)

    def test_size_change_is_rejected(self):
        archive, lock = self.archive()
        lock['bytes'] += 1
        with self.assertRaisesRegex(AssertionError, 'size mismatch'):
            installer.verify_archive(archive, lock)

    def test_paths_outside_the_runtime_are_rejected(self):
        for path in ('../../etc/passwd', '/usr/bin/jsc', 'usr/bin/bash', 'usr/share/doc/brick-webkit-runtime/../../../../etc/passwd'):
            with self.subTest(path=path):
                archive, lock = self.archive([tarfile.TarInfo(path)])
                with self.assertRaises(AssertionError):
                    installer.verify_archive(archive, lock)

    def test_escaping_symlink_is_rejected(self):
        member = tarfile.TarInfo(installer.LIB + 'webkit2gtk-4.1/escape')
        member.type = tarfile.SYMTYPE
        member.linkname = '../../../../../etc'
        archive, lock = self.archive([member])
        with self.assertRaisesRegex(AssertionError, 'Unsafe runtime symlink'):
            installer.verify_archive(archive, lock)

    def test_device_entry_is_rejected(self):
        member = tarfile.TarInfo(installer.LIB + 'webkit2gtk-4.1/device')
        member.type = tarfile.CHRTYPE
        archive, lock = self.archive([member])
        with self.assertRaisesRegex(AssertionError, 'Unsupported runtime archive entry'):
            installer.verify_archive(archive, lock)

    def test_setuid_entry_is_rejected(self):
        member = tarfile.TarInfo(installer.LIB + 'webkit2gtk-4.1/elevated')
        member.mode = 0o4755
        archive, lock = self.archive([member])
        with self.assertRaisesRegex(AssertionError, 'Privileged runtime file mode'):
            installer.verify_archive(archive, lock)

    def test_duplicate_entry_is_rejected(self):
        archive, lock = self.archive([tarfile.TarInfo(installer.LIB + 'libwebkit2gtk-4.1.so')])
        with self.assertRaisesRegex(AssertionError, 'Duplicate runtime archive path'):
            installer.verify_archive(archive, lock)

if __name__ == '__main__':
    unittest.main()
