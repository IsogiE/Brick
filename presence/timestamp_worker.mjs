import { spawn } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
import { createReplaySyncLibrary } from './replay_sync.mjs';
const library = createReplaySyncLibrary({ dataDir: process.env.DATA_DIR || '/data' });
let stopped = false, child;
function terminate() { stopped = true; if (child) { try { process.kill(-child.pid, 'SIGKILL'); } catch (_) {} } }
process.on('SIGTERM', terminate);
process.on('SIGINT', terminate);
async function scan(job) {
  return new Promise(resolve => {
    let output = '', finished = false;
    child = spawn('python3', ['/app/timestamp_scan.py'], { detached: true, stdio: ['pipe', 'pipe', 'ignore'] });
    const processHandle = child;
    const stop = () => { try { process.kill(-processHandle.pid, 'SIGKILL'); } catch (_) {} };
    const timer = setTimeout(stop, 100_000);
    const done = value => { if (finished) return; finished = true; clearTimeout(timer); child = null; resolve(value); };
    processHandle.on('error', () => done(null));
    processHandle.stdout.on('data', chunk => { output += chunk; if (output.length > 4096) stop(); });
    processHandle.on('close', code => {
      if (code || output.length > 4096) return done(null);
      try { done(JSON.parse(output)); } catch (_) { done(null); }
    });
    processHandle.stdin.on('error', () => {});
    processHandle.stdin.end(JSON.stringify({ key: job.key, attempt: job.attempt }));
  });
}
while (!stopped) {
  const job = library.claim();
  if (!job) { await delay(5000); continue; }
  const started = Date.now();
  const measured = library.finish(job, await scan(job));
  // Log only public provider IDs and workload duration, never URLs or images.
  console.log(JSON.stringify({ provider: job.key.provider, video: job.key.videoId, measured, milliseconds: Date.now()-started }));
  if (!stopped) await delay(5000);
}
library.close();
