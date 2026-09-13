import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { chmod, copyFile, mkdir, mkdtemp, readFile, readdir, rename, rm, symlink, writeFile } from 'node:fs/promises';
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

async function packageFixture(t) {
  const result = await fixture(t);
  const appdir = path.join(result.root, 'input');
  await mkdir(path.join(appdir, 'usr/lib/brick'), { recursive: true });
  await copyFile(result.bootstrap, path.join(appdir, 'usr/lib/brick/apprun'));
  await writeFile(path.join(appdir, 'AppRun'), '#!/usr/bin/env bash\nexit 0\n', { mode: 0o755 });
  const runtime = await readFile(result.bootstrap);
  const artifact = path.join(result.root, 'fixture.AppImage');
  async function pack() {
    const squash = path.join(result.root, 'filesystem');
    execFileSync('mksquashfs', [appdir, squash, '-noappend', '-no-progress', '-comp', 'gzip', '-processors', '2'], { stdio: 'pipe' });
    const bytes = Buffer.concat([runtime, await readFile(squash)]);
    await writeFile(artifact, bytes, { mode: 0o755 });
    return bytes;
  }
  return { ...result, appdir, runtime, artifact, pack };
}

test('finalization rejects input and bundled symlinks without following leaf or ancestor targets', async t => {
  const { root, appdir, artifact, pack } = await packageFixture(t);
  const original = await pack();
  const linked = path.join(root, 'linked.AppImage');
  await symlink(artifact, linked);
  await assert.rejects(finalizeAppImage(linked), { code: 'ELOOP' });
  assert((await readFile(artifact)).equals(original));
  for (const relative of ['AppRun', 'usr/lib/brick/apprun', 'usr/lib/brick']) {
    const target = path.join(appdir, relative);
    const saved = path.join(root, 'original-target');
    await rename(target, saved);
    await symlink(saved, target);
    const bytes = await pack();
    await assert.rejects(finalizeAppImage(artifact), error => ['ELOOP', 'ENOTDIR'].includes(error.code));
    assert((await readFile(artifact)).equals(bytes));
    assert(!(await readdir(root)).some(name => name.startsWith('.brick-apprun-')));
    await rm(target); await rename(saved, target);
  }
});

test('extraction uses the private validated snapshot when the artifact path is replaced by a symlink', async t => {
  const { root, artifact, runtime, pack } = await packageFixture(t);
  const original = await pack();
  const victim = path.join(root, 'unrelated-file');
  const contents = 'This file must never be read as an AppImage or overwritten.';
  await writeFile(victim, contents);
  const tools = path.join(root, 'tools'); await mkdir(tools);
  const actual = execFileSync('sh', ['-c', 'command -v unsquashfs'], { encoding: 'utf8' }).trim();
  const wrapper = `#!${process.execPath}
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, renameSync, statSync, symlinkSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
const input = process.argv.at(-1);
assert.notEqual(input, ${JSON.stringify(artifact)});
assert.equal(statSync(path.dirname(input)).mode & 0o777, 0o700);
assert.equal(createHash('sha256').update(readFileSync(input)).digest('hex'), ${JSON.stringify(digest(original))});
renameSync(${JSON.stringify(artifact)}, ${JSON.stringify(path.join(root, 'original.AppImage'))});
symlinkSync(${JSON.stringify(victim)}, ${JSON.stringify(artifact)});
execFileSync(${JSON.stringify(actual)}, process.argv.slice(2), { stdio: 'inherit' });
`;
  await writeFile(path.join(tools, 'unsquashfs'), wrapper, { mode: 0o755 });
  const previousPath = process.env.PATH;
  try {
    process.env.PATH = `${tools}:${previousPath}`;
    await finalizeAppImage(artifact);
  } finally {
    process.env.PATH = previousPath;
  }
  assert.equal(await readFile(victim, 'utf8'), contents);
  assert((await readFile(artifact)).subarray(0, runtime.length).equals(runtime));
  assert(!(await readdir(root)).some(name => name.startsWith('.brick-apprun-')));
});
