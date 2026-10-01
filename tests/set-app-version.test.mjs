import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const script = fileURLToPath(new URL('../scripts/set-app-version.mjs', import.meta.url));
for (const eol of ['\n', '\r\n']) {
  test(`release version preserves locked dependencies with ${JSON.stringify(eol)} line endings`, () => {
    const dir = mkdtempSync(join(tmpdir(), 'brick-version-test-'));
    try {
      const lock = [
        'version = 4', '', '[[package]]', 'name = "before"', 'version = "1.2.3"',
        'checksum = "unchanged"', '', '[[package]]', 'name = "brick"', 'version = "0.7.1"',
        'dependencies = ["before", "after"]', '', '[[package]]', 'name = "after"', 'version = "4.5.6"', '',
      ].join(eol);
      writeFileSync(join(dir, 'Cargo.lock'), lock);
      writeFileSync(join(dir, 'Cargo.toml'), '[package]\nname = "brick"\nversion = "0.7.1"\n');
      writeFileSync(join(dir, 'Packager.toml'), 'version = "0.7.1"\n');
      writeFileSync(join(dir, 'package.json'), '{"version":"0.7.1"}');
      const result = spawnSync(process.execPath, [script], {
        cwd: dir, env: { ...process.env, BRICK_APP_VERSION: '0.7.2' }, encoding: 'utf8',
      });
      assert.equal(result.status, 0, result.stderr);
      assert.equal(readFileSync(join(dir, 'Cargo.lock'), 'utf8'), lock.replace('version = "0.7.1"', 'version = "0.7.2"'));
      assert.match(readFileSync(join(dir, 'Cargo.toml'), 'utf8'), /version = "0.7.2"/);
      assert.match(readFileSync(join(dir, 'Packager.toml'), 'utf8'), /version = "0.7.2"/);
      assert.equal(JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8')).version, '0.7.2');
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
}
