import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readdir, rm, stat } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const directory = path.resolve(process.argv[2] ?? 'dist/packages');
const names = (await readdir(directory)).filter((name) => name.endsWith('.AppImage'));
assert.equal(names.length, 1, 'Expected exactly one AppImage to verify');
const artifact = path.join(directory, names[0]);
const size = (await stat(artifact)).size;
// Existing Brick clients enforce this limit when downloading an app update.
assert(size > 0 && size <= 256 * 1024 * 1024, `AppImage exceeds the 256 MiB updater limit: ${size} bytes`);
const temporary = await mkdtemp(path.join(os.tmpdir(), 'brick-appimage-check-'));
try {
  execFileSync(artifact, ['--appimage-extract'], { cwd: temporary, stdio: 'ignore', timeout: 120_000 });
  const appdir = path.join(temporary, 'squashfs-root');
  assert((await stat(path.join(appdir, 'usr/bin/bwrap'))).isFile(), 'Missing sandbox relocation wrapper');
  const files = await readdir(path.join(appdir, 'usr/lib'), { recursive: true });
  for (const library of ['libwebkit2gtk-4.1.so', 'libjavascriptcoregtk-4.1.so', 'libsoup-3.0.so', 'libgtk-3.so', 'libgstreamer-1.0.so']) {
    assert(files.some((file) => path.basename(file).startsWith(library)), `Missing bundled ${library}`);
  }
  const required = [
    'usr/bin/brick', 'usr/bin/bwrap', 'usr/bin/brick-bwrap', 'usr/bin/xdg-dbus-proxy',
    'usr/lib/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner',
    ...['coreelements', 'playback', 'soup', 'isomp4', 'hls', 'libav', 'audioconvert'].map((name) => `usr/lib/gstreamer-1.0/libgst${name}.so`),
  ];
  for (const name of ['WebKitWebProcess', 'WebKitNetworkProcess', 'libwebkit2gtkinjectedbundle.so']) {
    const file = files.find((file) => path.basename(file) === name);
    assert(file, `Missing matching WebKit helper ${name}`);
    required.push(`usr/lib/${file}`);
  }
  const env = { ...process.env, LD_LIBRARY_PATH: `${appdir}/usr/lib:${appdir}/usr/lib/${process.arch === 'x64' ? 'x86_64' : 'aarch64'}-linux-gnu` };
  for (const relative of required) {
    const file = path.join(appdir, relative);
    assert((await stat(file)).isFile(), `Missing runtime file ${relative}`);
    const dependencies = execFileSync('ldd', [file], { env, encoding: 'utf8', timeout: 10_000 });
    assert(!dependencies.includes('not found'), `Unresolved runtime dependency in ${relative}:\n${dependencies}`);
    if (relative.endsWith('/brick') || relative.includes('/WebKit')) {
      for (const line of dependencies.split('\n')) {
        if (/lib(webkit2gtk|javascriptcoregtk|soup-3|gtk-3|gstreamer)/.test(line)) {
          assert(line.includes(appdir), `Runtime escaped the AppImage in ${relative}: ${line}`);
        }
      }
    }
  }
  console.log(`Verified ${names[0]}: ${(size / 1024 / 1024).toFixed(1)} MiB; bundled GTK, WebKit, media and sandbox helpers`);
} finally {
  await rm(temporary, { recursive: true, force: true });
}
