import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { cp, chmod, mkdir, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

// cargo-packager also invokes this hook for Windows and native Linux packages.
const formats = (process.env.CARGO_PACKAGER_FORMATS ?? 'appimage').split(',');
if (process.platform !== 'linux' || !formats.includes('appimage')) process.exit(0);

const plugins = [
  {
    name: 'gtk',
    revision: 'b5eb8d05b4c0ed40107fe2158c5d8527f94568ef',
    sha256: 'cb379f9b0733e9ad9f8bd78f8c2fa038aef2478523bb7d4c8e64ff6a1ea3501a',
  },
  {
    name: 'gstreamer',
    revision: '2a2e67491c32995a3f279ad0ecbe77abd512b42a',
    sha256: 'c107b49d84edbffc6ab226ed1007e0626a4f7aa2c3a36b7782bef62351d49e94',
  },
];
const cache = path.join(process.env.XDG_CACHE_HOME || path.join(os.homedir(), '.cache'), '.cargo-packager', 'AppImage');
await mkdir(cache, { recursive: true });
for (const plugin of plugins) {
  const file = path.join(cache, `linuxdeploy-plugin-${plugin.name}.sh`);
  let bytes = await readFile(file).catch(() => null);
  const digest = (value) => createHash('sha256').update(value).digest('hex');
  if (!bytes || digest(bytes) !== plugin.sha256) {
    const url = `https://raw.githubusercontent.com/tauri-apps/linuxdeploy-plugin-${plugin.name}/${plugin.revision}/linuxdeploy-plugin-${plugin.name}.sh`;
    const response = await fetch(url, { signal: AbortSignal.timeout(30_000), redirect: 'error' });
    if (!response.ok) throw new Error(`Cannot download ${plugin.name}: HTTP ${response.status}`);
    const chunks = [];
    let length = 0;
    for await (const chunk of response.body) {
      length += chunk.length;
      if (length > 128 * 1024) throw new Error(`Oversized ${plugin.name} packaging plugin`);
      chunks.push(chunk);
    }
    bytes = Buffer.concat(chunks);
    if (digest(bytes) !== plugin.sha256) throw new Error(`Invalid ${plugin.name} packaging plugin checksum`);
    await writeFile(file, bytes, { mode: 0o755 });
  }
  await chmod(file, 0o755);
}

const output = path.resolve('target/appimage-runtime');
await rm(output, { recursive: true, force: true });
const libdir = execFileSync('pkg-config', ['--variable=libdir', 'webkit2gtk-4.1'], { encoding: 'utf8' }).trim();
const roots = [...new Set([libdir, '/usr/lib', '/usr/lib64', '/usr/libexec'])].filter((dir) => dir.startsWith('/usr/'));
let webkit;
for (const root of roots) {
  const candidate = path.join(root, 'webkit2gtk-4.1');
  if (await readFile(path.join(candidate, 'WebKitWebProcess')).then(() => true, () => false)) {
    webkit = candidate;
    break;
  }
}
if (!webkit) throw new Error('WebKitGTK 4.1 helper processes are required to build the AppImage');

// Keep helpers at the paths compiled into this exact WebKit build. The GTK
// plugin relocates those prefixes; mixing host helpers with bundled libraries
// can otherwise crash before a stream loads.
const required = [
  path.join(webkit, 'WebKitWebProcess'),
  path.join(webkit, 'WebKitNetworkProcess'),
  path.join(webkit, 'injected-bundle/libwebkit2gtkinjectedbundle.so'),
  '/usr/bin/bwrap',
  '/usr/bin/xdg-dbus-proxy',
];
const gpu = path.join(webkit, 'WebKitGPUProcess');
if (await readFile(gpu).then(() => true, () => false)) required.push(gpu);
for (const source of required) {
  const relative = source === '/usr/bin/bwrap' ? 'usr/bin/brick-bwrap' : source.slice(1);
  const target = path.join(output, relative);
  await mkdir(path.dirname(target), { recursive: true });
  await cp(source, target, { dereference: true });
  await chmod(target, 0o755);
}

// Winit dlopens the X11 companion, so ldd cannot discover it. Its private
// keymap objects must match the bundled base library, including at shutdown.
const xkbLibdir = execFileSync('pkg-config', ['--variable=libdir', 'xkbcommon-x11'], { encoding: 'utf8' }).trim();
for (const name of ['libxkbcommon.so.0', 'libxkbcommon-x11.so.0', 'libxcb-xkb.so.1']) {
  const target = path.join(output, 'usr/lib', name);
  await mkdir(path.dirname(target), { recursive: true });
  await cp(path.join(xkbLibdir, name), target, { dereference: true });
}

execFileSync('cc', ['-O2', '-Wall', '-Wextra', '-Werror', '-o', path.join(output, 'usr/bin/bwrap'), 'packaging/linux/bwrap-wrapper.c']);
await chmod(path.join(output, 'usr/bin/bwrap'), 0o755);

// Preserve the distribution's copyright notices for bundled runtime packages.
// These small notices also identify upstream sources and applicable licenses.
const notices = path.join(output, 'usr/share/doc/brick-runtime');
for (const name of await readdir('/usr/share/doc')) {
  const source = path.join('/usr/share/doc', name, 'copyright');
  const bytes = await readFile(source).catch(() => null);
  if (!bytes) continue;
  const target = path.join(notices, name);
  await mkdir(target, { recursive: true });
  await writeFile(path.join(target, 'copyright'), bytes);
}

const hooks = path.join(output, 'apprun-hooks');
await mkdir(hooks, { recursive: true });
// Do not let the bundled plugin scanner replace the user's system registry.
await writeFile(path.join(hooks, 'brick-media.sh'), `#!/bin/sh
export GST_REGISTRY_1_0="\${XDG_CACHE_HOME:-$HOME/.cache}/dev.isogi.brick/gstreamer-registry.bin"
mkdir -p "\${GST_REGISTRY_1_0%/*}"
`, { mode: 0o755 });
console.log(`Prepared AppImage media runtime from ${webkit}; sandbox helpers included`);
