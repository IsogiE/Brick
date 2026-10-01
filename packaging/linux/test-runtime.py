#!/usr/bin/env python3
"""Exercise the installed release runtime with local JavaScript and H.264 media."""
from functools import partial
import http.server
import json
import os
from pathlib import Path
import shlex
import subprocess
import tempfile
import threading

HERE = Path(__file__).resolve().parent

def main():
    lock = json.loads((HERE / 'runtime.lock.json').read_text())
    with tempfile.TemporaryDirectory(prefix='brick-runtime-smoke-', dir='/var/tmp') as temporary:
        root = Path(temporary)
        (root / 'index.html').write_text('<!doctype html><meta charset="utf-8"><script>window.brickProbe=6*7</script><video muted autoplay loop playsinline src="probe.mp4"></video>')
        subprocess.run(['ffmpeg', '-hide_banner', '-loglevel', 'error', '-f', 'lavfi', '-i',
                        'testsrc=size=64x64:rate=10', '-t', '2', '-c:v', 'libx264', '-pix_fmt', 'yuv420p',
                        '-movflags', '+faststart', str(root / 'probe.mp4')], check=True, timeout=30)
        flags = shlex.split(subprocess.check_output(['pkg-config', '--cflags', '--libs', 'webkit2gtk-4.1'], text=True))
        subprocess.run(['cc', '-Wall', '-Wextra', '-Werror', str(HERE / 'runtime-smoke.c'), *flags,
                        '-o', str(root / 'probe')], check=True, timeout=30)
        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self, *_):
                pass
        with http.server.ThreadingHTTPServer(('127.0.0.1', 0), partial(Handler, directory=str(root))) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                env = os.environ.copy()
                env.update(GDK_BACKEND='x11', LIBGL_ALWAYS_SOFTWARE='1', WEBKIT_DISABLE_DMABUF_RENDERER='1')
                result = subprocess.run(['xvfb-run', '-a', 'dbus-run-session', '--', str(root / 'probe'),
                                         f'http://127.0.0.1:{server.server_port}/'], env=env, text=True,
                                        capture_output=True, timeout=45)
                if result.returncode:
                    raise RuntimeError('Installed runtime playback failed: ' + result.stderr[-3000:])
                report = next(json.loads(line) for line in result.stdout.splitlines() if line.startswith('{'))
                assert report['version'] == [int(part) for part in lock['version'].split('.')]
                assert report['javascript'] and report['decodedVideo']
                print(json.dumps({'passed': True, **report}))
            finally:
                server.shutdown()
                thread.join(timeout=5)

if __name__ == '__main__':
    main()
