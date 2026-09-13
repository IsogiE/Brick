import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { chmod, copyFile, lstat, mkdtemp, readFile, readdir, rename, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const MAX_BYTES = 256 * 1024 * 1024;

function filesystem(bytes) {
  assert(bytes.length >= 64 && bytes.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70]))
    && bytes[4] === 2 && bytes[5] === 1, 'Expected a 64-bit little-endian AppImage');
  const end = bytes.readBigUInt64LE(40) + BigInt(bytes.readUInt16LE(58) * bytes.readUInt16LE(60));
  assert(end >= 64n && end + 96n <= BigInt(bytes.length), 'Invalid AppImage runtime boundary');
  const offset = Number(end);
  assert(bytes.subarray(offset, offset + 4).toString() === 'hsqs'
    && bytes.readUInt16LE(offset + 28) === 4 && bytes.readUInt16LE(offset + 30) === 0,
  'Expected the existing SquashFS filesystem after the runtime');
  const compression = new Map([[1, 'gzip'], [4, 'xz'], [6, 'zstd']]).get(bytes.readUInt16LE(offset + 20));
  const blockLog = bytes.readUInt16LE(offset + 22);
  assert(compression && blockLog >= 12 && blockLog <= 20, 'Unsupported AppImage filesystem parameters');
  return { offset, compression, blockSize: 2 ** blockLog };
}

export async function finalizeAppImage(artifact) {
  const info = await lstat(artifact);
  assert(info.isFile() && info.size > 0 && info.size <= MAX_BYTES, 'Invalid AppImage package');
  const bytes = await readFile(artifact);
  const { offset, compression, blockSize } = filesystem(bytes);
  // Keep the original type-2 runtime byte-for-byte. All work stays next to the
  // package, so a failed rebuild cannot truncate it or spill into the host home.
  const temporary = await mkdtemp(path.join(path.dirname(artifact), '.brick-apprun-'));
  try {
    const appdir = path.join(temporary, 'AppDir');
    execFileSync('unsquashfs', ['-no-progress', '-processors', '4', '-offset', String(offset), '-dest', appdir, artifact], { stdio: 'ignore', timeout: 120_000 });
    const apprun = path.join(appdir, 'AppRun');
    const launcher = path.join(appdir, 'AppRun.launcher');
    const bootstrap = path.join(appdir, 'usr/lib/brick/apprun');
    assert((await lstat(apprun)).isFile() && (await lstat(bootstrap)).isFile(), 'Missing regular AppRun/bootstrap');
    const bootstrapBytes = await readFile(bootstrap);
    assert(bootstrapBytes.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70])), 'AppRun bootstrap must be native');
    const existing = await readFile(apprun);
    if (existing.equals(bootstrapBytes)) {
      assert((await lstat(launcher)).isFile() && (await readFile(launcher)).subarray(0, 2).toString() === '#!', 'Missing generated AppRun');
      return;
    }
    assert(existing.subarray(0, 2).toString() === '#!', 'Expected the generated AppRun script');
    assert(!(await lstat(launcher).catch(error => { if (error.code !== 'ENOENT') throw error; })), 'AppRun launcher already exists');
    await rename(apprun, launcher);
    await copyFile(bootstrap, apprun);
    await chmod(apprun, 0o755);
    const squash = path.join(temporary, 'filesystem');
    execFileSync('mksquashfs', [appdir, squash, '-noappend', '-all-root', '-no-progress', '-comp', compression, '-b', String(blockSize)], { stdio: 'ignore', timeout: 180_000 });
    const rebuilt = Buffer.concat([bytes.subarray(0, offset), await readFile(squash)]);
    assert(rebuilt.length <= MAX_BYTES, 'Final AppImage exceeds the updater size limit');
    filesystem(rebuilt);
    const output = path.join(temporary, 'updated.AppImage');
    await writeFile(output, rebuilt, { mode: info.mode & 0o777 });
    await rename(output, artifact);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const directory = path.resolve(process.argv[2] ?? 'dist/packages');
  const names = (await readdir(directory)).filter(name => name.endsWith('.AppImage'));
  assert.equal(names.length, 1, 'Expected exactly one AppImage to finalize');
  await finalizeAppImage(path.join(directory, names[0]));
  console.log(`Prepared native startup for ${names[0]}`);
}
