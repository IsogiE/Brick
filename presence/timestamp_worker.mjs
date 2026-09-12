import { setTimeout as delay } from 'node:timers/promises';
import { readFile } from 'node:fs/promises';
import { createTimestampClient } from './timestamp_queue.mjs';
import { syncKey } from './replay_sync.mjs';
import { createScanner } from './timestamp_process.mjs';
const library = createTimestampClient({ url: process.env.BRICK_TIMESTAMP_QUEUE_URL || 'http://api:8081/',
  token: (await readFile(process.env.BRICK_TIMESTAMP_TOKEN_FILE, 'utf8')).trim() });
const shutdown = new AbortController();
function terminate() { shutdown.abort(); }
process.on('SIGTERM', terminate);
process.on('SIGINT', terminate);
try {
  const scan = await createScanner();
  while (!shutdown.signal.aborted) {
    let job;
    try { job = await library.claim(shutdown.signal); }
    catch { if (!shutdown.signal.aborted) console.error('Timestamp queue unavailable'); }
    if (job) {
      const started = Date.now();
      syncKey(job.key);
      const result = await scan(job, shutdown.signal);
      let measured = false;
      try { measured = await library.finish(job, result, shutdown.signal); }
      catch { if (!shutdown.signal.aborted) console.error('Timestamp result could not be submitted'); }
      const reason = !measured && ['youtube_auth_required', 'provider_rate_limited'].includes(result?.error)
        ? result.error : undefined;
      // Log only public provider IDs and workload duration, never URLs or images.
      console.log(JSON.stringify({ provider: job.key.provider, video: job.key.videoId, measured, reason, milliseconds: Date.now()-started }));
    }
    if (!shutdown.signal.aborted) {
      await delay(5000, undefined, { signal: shutdown.signal }).catch(error => {
        if (error.name !== 'AbortError') throw error;
      });
    }
  }
} finally {
  process.off('SIGTERM', terminate);
  process.off('SIGINT', terminate);
}
