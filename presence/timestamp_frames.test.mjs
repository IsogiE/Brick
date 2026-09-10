import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
test('frames are discarded during capture and partial files are ignored', { skip: process.platform === 'win32' }, () => {
  const result = spawnSync('python3', ['-B', fileURLToPath(new URL('./timestamp_frames.test.py', import.meta.url))], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr || result.stdout);
});
