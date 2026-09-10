import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

test('YouTube session isolation and sanitized failure reporting', { skip: process.platform === 'win32' }, () => {
  const result = spawnSync('python3', ['-B', fileURLToPath(new URL('./timestamp_youtube.test.py', import.meta.url))],
    { encoding: 'utf8', timeout: 15_000 });
  assert.equal(result.status, 0, result.stderr || result.error?.message);
});
