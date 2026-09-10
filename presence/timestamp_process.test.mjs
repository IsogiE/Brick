import assert from 'node:assert/strict';
import { readFile, writeFile, mkdir, mkdtemp, readdir, rm, symlink } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { createScanner } from './timestamp_process.mjs';

const fixture = `
  const fs = require('node:fs');
  const { spawn } = require('node:child_process');
  let input = '';
  process.stdin.on('data', chunk => input += chunk);
  process.stdin.on('end', () => {
    const { key } = JSON.parse(input);
    const scratch = process.env.TMPDIR;
    if ([process.env.TMP, process.env.TEMP, process.env.XDG_CACHE_HOME].some(p => p !== scratch)) process.exit(2);
    fs.mkdirSync(scratch + '/frames');
    fs.writeFileSync(scratch + '/frames/sample.png', Buffer.alloc(1024));
    fs.writeFileSync(scratch + '/section.ts', Buffer.alloc(2048));
    if (key.ready) fs.writeFileSync(key.ready, 'ready');
    switch (key.mode) {
      case 'crash': process.exit(1);
      case 'malformed': console.log('invalid JSON'); break;
      case 'overflow': process.stdout.write('x'.repeat(8192)); setInterval(() => {}, 1000); break;
      case 'hang': setInterval(() => {}, 1000); break;
      case 'orphan': {
        const descendant = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: ['ignore', 'inherit', 'inherit'] });
        fs.writeFileSync(key.pid, String(descendant.pid));
        console.log('12345');
        process.exit(0);
      }
      default: console.log('12345');
    }
  });
`;

async function setup(t, options = {}) {
  const directory = await mkdtemp(path.join(tmpdir(), 'brick-scanner-test-'));
  t.after(() => rm(directory, { recursive: true, force: true }));
  const root = path.join(directory, 'scratch');
  const scan = await createScanner({ root, command: process.execPath, args: ['-e', fixture], timeoutMs: 3000, ...options });
  return { directory, root, scan };
}

async function waitForFile(file) {
  for (let attempt = 0; attempt < 200; attempt++) {
    try { return await readFile(file, 'utf8'); }
    catch (error) { if (error.code !== 'ENOENT') throw error; }
    await delay(10);
  }
  throw new Error('Scanner fixture did not start.');
}

test('scanner cleans frames and media after success, crash, malformed and oversized output', { skip: process.platform === 'win32' }, async t => {
  const { scan, root } = await setup(t);
  for (const mode of ['success', 'crash', 'malformed', 'overflow', 'success']) {
    assert.equal(await scan({ key: { mode }, attempt: 0 }), mode === 'success' ? 12345 : null, mode);
    assert.deepEqual(await readdir(root), [], mode);
  }
});

test('scanner cleans after a deadline and can run the next job', { skip: process.platform === 'win32' }, async t => {
  const { scan, root, directory } = await setup(t, { timeoutMs: 500 });
  const ready = path.join(directory, 'ready');
  assert.equal(await scan({ key: { mode: 'hang', ready }, attempt: 0 }), null);
  assert.equal(await readFile(ready, 'utf8'), 'ready');
  assert.deepEqual(await readdir(root), []);
  assert.equal(await scan({ key: { mode: 'success' }, attempt: 0 }), 12345);
  assert.deepEqual(await readdir(root), []);
});

test('shutdown cancels an active scan and prevents starting another', { skip: process.platform === 'win32' }, async t => {
  const { scan, root, directory } = await setup(t);
  const shutdown = new AbortController();
  const ready = path.join(directory, 'ready');
  const pending = scan({ key: { mode: 'hang', ready }, attempt: 0 }, shutdown.signal);
  await waitForFile(ready);
  await assert.rejects(scan({ key: {}, attempt: 0 }), /already running/);
  shutdown.abort();
  assert.equal(await pending, null);
  assert.deepEqual(await readdir(root), []);
  assert.equal(await scan({ key: {}, attempt: 0 }, shutdown.signal), null);
  assert.deepEqual(await readdir(root), []);
});

test('scanner terminates descendants left holding its output pipe', { skip: process.platform !== 'linux' }, async t => {
  const { scan, root, directory } = await setup(t);
  const pidFile = path.join(directory, 'descendant.pid');
  assert.equal(await scan({ key: { mode: 'orphan', pid: pidFile }, attempt: 0 }), 12345);
  assert.deepEqual(await readdir(root), []);
  const pid = Number(await readFile(pidFile, 'utf8'));
  // A zombie is already dead and cannot hold files; init reaps it separately.
  try { assert.match(await readFile(`/proc/${pid}/stat`, 'utf8'), /\) Z /); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
});

test('failed executable launch also removes the job directory', async t => {
  const { scan, root } = await setup(t, { command: '/nonexistent-brick-scanner' });
  assert.equal(await scan({ key: {}, attempt: 0 }), null);
  assert.deepEqual(await readdir(root), []);
});

test('startup recovers only owned jobs and never follows their symlinks', async t => {
  const { root, directory } = await setup(t);
  await mkdir(path.join(root, 'job-ABC123'));
  await writeFile(path.join(root, 'job-ABC123', 'leftover.ts'), 'unfinished');
  await mkdir(path.join(root, 'unrelated'));
  const outside = path.join(directory, 'outside');
  await mkdir(outside);
  await writeFile(path.join(outside, 'keep'), 'preserved');
  await symlink(outside, path.join(root, 'job-DEF456'));
  await createScanner({ root });
  assert.deepEqual(await readdir(root), ['unrelated']);
  assert.equal(await readFile(path.join(outside, 'keep'), 'utf8'), 'preserved');
  const linkedRoot = path.join(directory, 'linked-root');
  await symlink(outside, linkedRoot);
  await assert.rejects(createScanner({ root: linkedRoot }), /Invalid timestamp scratch directory/);
});
