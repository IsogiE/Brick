import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { finalizeAppImage } from './finalize-appimage.mjs';

const source = path.resolve('packaging/linux/apprun.rs');
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const elfEnd = bytes => Number(bytes.readBigUInt64LE(40)) + bytes.readUInt16LE(58) * bytes.readUInt16LE(60);

async function fixture(t) {
  const root = await mkdtemp(path.join(os.tmpdir(), 'brick-apprun-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const bootstrap = path.join(root, 'bootstrap');
  execFileSync('rustc', ['--edition=2021', '-Dwarnings', '-Copt-level=z', '-Cpanic=abort', '-o', bootstrap, source], { stdio: 'pipe' });
  return { root, bootstrap };
}

test('native AppRun cleans the old library path before the next loader starts and preserves host/session values', async t => {
  const { root, bootstrap } = await fixture(t);
  const old = path.join(root, 'previous');
  const current = path.join(root, 'new image $(false)');
  const host = path.join(root, 'host-libraries');
  await mkdir(path.join(old, 'usr/bin'), { recursive: true });
  await mkdir(path.join(old, 'usr/lib'), { recursive: true });
  await mkdir(current); await mkdir(host);
  await writeFile(path.join(old, 'AppRun'), 'old launcher');
  await writeFile(path.join(old, 'usr/bin/brick'), 'old executable');
  await writeFile(path.join(root, 'good.c'), 'int brick_loader_probe(void) { return 42; }\n');
  await writeFile(path.join(root, 'bad.c'), 'int different_old_symbol(void) { return 17; }\n');
  for (const [name, directory] of [['good', host], ['bad', path.join(old, 'usr/lib')]]) {
    execFileSync('cc', ['-shared', '-fPIC', '-o', path.join(directory, 'libbrickprobe.so'), path.join(root, `${name}.c`)]);
  }
  await writeFile(path.join(root, 'child.c'), `#include <stdio.h>\n#include <stdlib.h>\nextern int brick_loader_probe(void);
int main(int argc, char **argv) {
  if (brick_loader_probe() != 42 || argc != 3) return 1;
  printf("%s\\n%s\\n%s\\n%s\\n%s\\n%s\\n", getenv("LD_LIBRARY_PATH"), getenv("APPDIR"), getenv("DBUS_SESSION_BUS_ADDRESS"), getenv("XDG_CONFIG_HOME"), argv[1], argv[2]);
  return 0;
}\n`);
  const child = path.join(current, 'AppRun.launcher');
  execFileSync('cc', [path.join(root, 'child.c'), '-L', host, '-lbrickprobe', '-o', child]);
  await copyFile(bootstrap, path.join(current, 'AppRun'));
  const env = { ...process.env, APPDIR: current, GTK_DATA_PREFIX: old, LD_LIBRARY_PATH: `${old}/usr/lib:${host}`, APPIMAGE: '/apps/Brick.AppImage', DBUS_SESSION_BUS_ADDRESS: 'unix:path=/session/bus', XDG_CONFIG_HOME: '/profile/config' };
  const args = ['--update-restart', 'literal $(false)'];
  const broken = spawnSync(child, args, { env, encoding: 'utf8' });
  assert.notEqual(broken.status, 0);
  assert.match(broken.stderr, /undefined symbol.*brick_loader_probe/);
  const output = execFileSync(path.join(current, 'AppRun'), args, { env, encoding: 'utf8' });
  assert.equal(output, `${host}\n${current}\nunix:path=/session/bus\n/profile/config\n--update-restart\nliteral $(false)\n`);
  await mkdir(path.join(current, 'usr/lib'), { recursive: true });
  await copyFile(path.join(old, 'usr/lib/libbrickprobe.so'), path.join(current, 'usr/lib/libbrickprobe.so'));
  env.GTK_DATA_PREFIX = current; env.LD_LIBRARY_PATH = `${current}/usr/lib:${host}`;
  assert.equal(execFileSync(path.join(current, 'AppRun'), args, { env, encoding: 'utf8' }), output);
  // An ordinary direct/extracted launch must preserve the host path too and
  // choose its own bundle even if the caller supplied an unrelated APPDIR.
  delete env.GTK_DATA_PREFIX; env.APPDIR = '/unrelated'; env.LD_LIBRARY_PATH = host;
  assert.equal(execFileSync(path.join(current, 'AppRun'), args, { env, encoding: 'utf8' }), output);
  const linked = path.join(root, 'linked launcher');
  await symlink(path.join(current, 'AppRun'), linked);
  assert.equal(execFileSync(linked, args, { env, encoding: 'utf8' }), output);
});

test('AppImage finalization preserves the runtime and generated hooks, is idempotent, and rejects unexpected launchers atomically', async t => {
  const { root, bootstrap } = await fixture(t);
  const appdir = path.join(root, 'input');
  await mkdir(path.join(appdir, 'usr/lib/brick'), { recursive: true });
  await copyFile(bootstrap, path.join(appdir, 'usr/lib/brick/apprun'));
  const launcher = '#! /usr/bin/env bash\nset -e\nprintf "unchanged hooks"\n';
  await writeFile(path.join(appdir, 'AppRun'), launcher, { mode: 0o755 });
  const squash = path.join(root, 'filesystem');
  execFileSync('mksquashfs', [appdir, squash, '-noappend', '-no-progress', '-comp', 'gzip', '-processors', '2'], { stdio: 'pipe' });
  const runtime = await readFile(bootstrap);
  assert.equal(elfEnd(runtime), runtime.length);
  const artifact = path.join(root, 'fixture.AppImage');
  await writeFile(artifact, Buffer.concat([runtime, await readFile(squash)]), { mode: 0o755 });
  await finalizeAppImage(artifact);
  const finalized = await readFile(artifact);
  assert(finalized.subarray(0, runtime.length).equals(runtime));
  await finalizeAppImage(artifact);
  assert.equal(digest(await readFile(artifact)), digest(finalized));
  const extracted = path.join(root, 'output');
  execFileSync('unsquashfs', ['-no-progress', '-processors', '2', '-offset', String(runtime.length), '-dest', extracted, artifact], { stdio: 'pipe' });
  assert((await readFile(path.join(extracted, 'AppRun'))).equals(runtime));
  assert.equal(await readFile(path.join(extracted, 'AppRun.launcher'), 'utf8'), launcher);
  await writeFile(path.join(appdir, 'AppRun'), 'unexpected launcher');
  execFileSync('mksquashfs', [appdir, squash, '-noappend', '-no-progress', '-comp', 'gzip', '-processors', '2'], { stdio: 'pipe' });
  const invalid = Buffer.concat([runtime, await readFile(squash)]);
  await writeFile(artifact, invalid); await chmod(artifact, 0o755);
  await assert.rejects(finalizeAppImage(artifact), /generated AppRun script/);
  assert((await readFile(artifact)).equals(invalid));
});
