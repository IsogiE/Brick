import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { constants } from 'node:fs';
import { mkdtemp, open, readdir, rename, rm } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const MAX_BYTES = 256 * 1024 * 1024;
const READ_FLAGS = constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK;

async function readRegularFile(filename) {
  const file = await open(filename, READ_FLAGS);
  try {
    const info = await file.stat();
    assert(info.isFile() && info.size > 0 && info.size <= MAX_BYTES, 'Invalid AppImage file');
    const bytes = Buffer.alloc(info.size);
    let position = 0;
    while (position < bytes.length) {
      const { bytesRead } = await file.read(bytes, position, Math.min(1024 * 1024, bytes.length - position), position);
      assert(bytesRead > 0, 'AppImage file changed while reading');
      position += bytesRead;
    }
    const extra = await file.read(Buffer.alloc(1), 0, 1, position);
    const after = await file.stat();
    assert(extra.bytesRead === 0 && after.size === info.size
      && after.mtimeMs === info.mtimeMs && after.ctimeMs === info.ctimeMs, 'AppImage file changed while reading');
    return { bytes, info };
  } finally {
    await file.close();
  }
}

async function readBundledFile(directory, relative) {
  const parts = relative.split('/');
  const parents = [];
  try {
    let parent = directory;
    for (const part of parts.slice(0, -1)) {
      parent = await open(`/proc/self/fd/${parent.fd}/${part}`, READ_FLAGS | constants.O_DIRECTORY);
      parents.push(parent);
    }
    return await readRegularFile(`/proc/self/fd/${parent.fd}/${parts.at(-1)}`);
  } finally {
    for (const parent of parents.reverse()) await parent.close();
  }
}

async function writeExclusive(filename, bytes, mode) {
  const file = await open(filename, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, mode);
  try {
    await file.writeFile(bytes);
    await file.chmod(mode);
  } finally {
    await file.close();
  }
}

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
  const { bytes, info } = await readRegularFile(artifact);
  const { offset, compression, blockSize } = filesystem(bytes);
  // Keep the original type-2 runtime byte-for-byte. All work stays next to the
  // package, so a failed rebuild cannot truncate it or spill into the host home.
  const temporary = await mkdtemp(path.join(path.dirname(artifact), '.brick-apprun-'));
  try {
    // Extract exactly the bytes validated through the original file handle.
    // The caller's artifact path is never reopened by the extraction process.
    const snapshot = path.join(temporary, 'input.AppImage');
    await writeExclusive(snapshot, bytes, 0o600);
    const appdir = path.join(temporary, 'AppDir');
    execFileSync('unsquashfs', ['-no-progress', '-processors', '4', '-offset', String(offset), '-dest', appdir, snapshot], { stdio: 'ignore', timeout: 120_000 });
    const apprun = path.join(appdir, 'AppRun');
    const launcher = path.join(appdir, 'AppRun.launcher');
    const directory = await open(appdir, READ_FLAGS | constants.O_DIRECTORY);
    try {
      const { bytes: bootstrapBytes } = await readBundledFile(directory, 'usr/lib/brick/apprun');
      assert(bootstrapBytes.subarray(0, 4).equals(Buffer.from([127, 69, 76, 70])), 'AppRun bootstrap must be native');
      const { bytes: existing, info: launcherInfo } = await readBundledFile(directory, 'AppRun');
      if (existing.equals(bootstrapBytes)) {
        const generated = await readBundledFile(directory, 'AppRun.launcher');
        assert(generated.bytes.subarray(0, 2).toString() === '#!', 'Missing generated AppRun');
        // Even an idempotent call publishes the verified bytes atomically;
        // a replaced caller path must not escape finalization unchecked.
        const unchanged = path.join(temporary, 'unchanged.AppImage');
        await writeExclusive(unchanged, bytes, info.mode & 0o777);
        await rename(unchanged, artifact);
        return;
      }
      assert(existing.subarray(0, 2).toString() === '#!', 'Expected the generated AppRun script');
      await writeExclusive(launcher, existing, launcherInfo.mode & 0o777);
      const native = path.join(temporary, 'native.AppRun');
      await writeExclusive(native, bootstrapBytes, 0o755);
      await rename(native, apprun);
    } finally {
      await directory.close();
    }
    const squash = path.join(temporary, 'filesystem');
    execFileSync('mksquashfs', [appdir, squash, '-noappend', '-all-root', '-no-progress', '-comp', compression, '-b', String(blockSize)], { stdio: 'ignore', timeout: 180_000 });
    const rebuilt = Buffer.concat([bytes.subarray(0, offset), (await readRegularFile(squash)).bytes]);
    assert(rebuilt.length <= MAX_BYTES, 'Final AppImage exceeds the updater size limit');
    filesystem(rebuilt);
    const output = path.join(temporary, 'updated.AppImage');
    await writeExclusive(output, rebuilt, info.mode & 0o777);
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
