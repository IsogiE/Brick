import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { closeSync, mkdtempSync, mkdirSync, openSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const temporary = mkdtempSync(path.join(os.tmpdir(), 'brick-sandbox-test-'));
const initial = path.join(temporary, 'Original AppDir');
const runtime = path.join(temporary, 'Moved AppDir', 'usr');
mkdirSync(path.join(initial, 'usr/bin'), { recursive: true });
execFileSync('cc', ['-O2', '-Wall', '-Wextra', '-Werror', '-o', path.join(initial, 'usr/bin/bwrap'), fileURLToPath(new URL('./bwrap-wrapper.c', import.meta.url))]);
writeFileSync(path.join(initial, 'usr/bin/brick-bwrap'), `#!/usr/bin/python3
import fcntl, json, os, sys
result = {'argv': sys.argv[1:]}
if '--args' in sys.argv:
    result['block'] = os.read(3, 4 * 1024 * 1024).decode().split('\\0')[:-1]
    result['marker'] = os.read(4, 100).decode()
    result['seals'] = fcntl.fcntl(3, fcntl.F_GET_SEALS)
print(json.dumps(result))
`, { mode: 0o755 });
renameSync(initial, path.dirname(runtime));
const wrapper = path.join(runtime, 'bin/bwrap');
after(() => rmSync(temporary, { recursive: true, force: true }));

function runBlock(block) {
  const input = path.join(temporary, 'arguments');
  const marker = path.join(temporary, 'seccomp-marker');
  writeFileSync(input, block);
  writeFileSync(marker, 'descriptor-preserved');
  const descriptors = [openSync(input, 'r'), openSync(marker, 'r')];
  try {
    const result = spawnSync(wrapper, ['--args', '3', '././/bin/xdg-dbus-proxy'], {
      cwd: temporary, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe', ...descriptors], timeout: 10_000,
    });
    assert.deepEqual(readFileSync(input), Buffer.from(block), 'parent argument data must remain unchanged');
    return result;
  } finally {
    descriptors.forEach(closeSync);
  }
}
const packed = (args) => Buffer.from(`${args.join('\0')}\0`);

test('relocates only the generated prefix after AppDir moves, without a shell', () => {
  const args = ['', 'literal $(touch nope); \"quote\"\nnewline', '/usr/lib', '../relative', '././lib/helper', '././/bin/helper'];
  const result = spawnSync(wrapper, args, { cwd: temporary, encoding: 'utf8', timeout: 10_000 });
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout).argv, [...args.slice(0, 4), `${runtime}/lib/helper`, `${runtime}//bin/helper`]);
});

test('keeps sandbox flags, FD numbers, seals and unrelated inherited data intact', () => {
  const result = runBlock(packed(['--unshare-net', '--seccomp', '4', '--ro-bind-try', '././/lib', '././/lib', '--die-with-parent', '']));
  assert.equal(result.status, 0, result.stderr);
  const data = JSON.parse(result.stdout);
  assert.deepEqual(data.argv, ['--args', '3', `${runtime}//bin/xdg-dbus-proxy`]);
  assert.deepEqual(data.block, ['--unshare-net', '--seccomp', '4', '--ro-bind-try', '/usr/lib', '/usr/lib', '--ro-bind-try', `${runtime}//lib`, `${runtime}//lib`, '--die-with-parent', '']);
  assert.equal(data.marker, 'descriptor-preserved');
  assert.equal(data.seals, 15);
});

test('does not add system mounts for writable, mismatched or non-runtime paths', () => {
  const args = ['--bind', '././/lib', '././/lib', '--ro-bind-try', '././/lib', '././/share', '--ro-bind-try', '././/home', '././/home'];
  const result = runBlock(packed(args));
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(JSON.parse(result.stdout).block, args.map((value) => value.startsWith('././') ? `${runtime}/${value.slice(4)}` : value));
});

test('rejects malformed, oversized or nested argument blocks before execution', () => {
  for (const bytes of [Buffer.from('missing-terminator'), Buffer.alloc(1024 * 1024 + 1), packed(['--args', '4'])]) {
    const result = runBlock(bytes);
    assert.equal(result.status, 1);
    assert.equal(result.stdout, '');
    assert.match(result.stderr, /Brick sandbox runtime:/);
  }
});
