import { spawn } from 'node:child_process';
import { chmod, lstat, mkdir, mkdtemp, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

const JOB_DIRECTORY = /^job-[a-zA-Z0-9]{6}$/;
const REMOVE = { recursive: true, force: true, maxRetries: 3, retryDelay: 50 };

// One supervisor owns this private scratch root. Never sweep /tmp or /data.
export async function createScanner({
  root = process.env.BRICK_TIMESTAMP_SCRATCH_DIR || path.join(tmpdir(), 'brick-timestamps'),
  command = 'python3', args = ['/app/timestamp_scan.py'], timeoutMs = 100_000,
} = {}) {
  await mkdir(root, { mode: 0o700 }).catch(error => {
    if (error.code !== 'EEXIST') throw error;
  });
  const directory = await lstat(root);
  if (!directory.isDirectory() || directory.isSymbolicLink()) {
    throw new Error('Invalid timestamp scratch directory.');
  }
  await chmod(root, 0o700);
  // A killed supervisor cannot run finally. Recover its job directories on
  // startup; rm removes symlinks themselves without traversing their targets.
  for (const entry of await readdir(root)) {
    if (JOB_DIRECTORY.test(entry)) await rm(path.join(root, entry), REMOVE);
  }

  let running = false;
  return async function scan(job, signal) {
    if (running) throw new Error('A timestamp scan is already running.');
    if (signal?.aborted) return null;
    const input = JSON.stringify({ key: job.key, attempt: job.attempt });
    running = true;
    let scratch;
    try {
      scratch = await mkdtemp(path.join(root, 'job-'));
      if (signal?.aborted) return null;
      return await new Promise(resolve => {
        const child = spawn(command, args, {
          detached: true, stdio: ['pipe', 'pipe', 'ignore'],
          env: { ...process.env, TMPDIR: scratch, TMP: scratch, TEMP: scratch, XDG_CACHE_HOME: scratch },
        });
        const chunks = [];
        let bytes = 0, failed = false;
        const killGroup = () => {
          if (child.pid) {
            try { process.kill(-child.pid, 'SIGKILL'); } catch (error) {
              if (error.code !== 'ESRCH') throw error;
            }
          }
        };
        const cancel = () => { failed = true; killGroup(); };
        const timer = setTimeout(cancel, timeoutMs);
        signal?.addEventListener('abort', cancel, { once: true });
        child.on('error', cancel);
        // Python may exit while ffmpeg still holds inherited pipes or scratch
        // files. Stop its whole process group before waiting for pipe closure.
        child.on('exit', killGroup);
        child.stdout.on('data', chunk => {
          if (failed) return;
          bytes += chunk.length;
          if (bytes > 4096) return cancel();
          chunks.push(chunk);
        });
        child.on('close', code => {
          clearTimeout(timer);
          signal?.removeEventListener('abort', cancel);
          if (failed || code !== 0) return resolve(null);
          try { resolve(JSON.parse(Buffer.concat(chunks).toString('utf8'))); }
          catch { resolve(null); }
        });
        child.stdin.on('error', () => {});
        child.stdin.end(input);
      });
    } finally {
      // The parent survives scanner timeouts/crashes. Do not start the next
      // job until cleanup succeeds; an error lets the container restart cleanly.
      if (scratch) await rm(scratch, REMOVE);
      running = false;
    }
  };
}
