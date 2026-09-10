import { setTimeout as delay } from 'node:timers/promises';
import { createReplaySyncLibrary } from './replay_sync.mjs';
import { createScanner } from './timestamp_process.mjs';
const library = createReplaySyncLibrary({ dataDir: process.env.DATA_DIR || '/data' });
const shutdown = new AbortController();
function terminate() { shutdown.abort(); }
process.on('SIGTERM', terminate);
process.on('SIGINT', terminate);
try {
  const scan = await createScanner();
  while (!shutdown.signal.aborted) {
    const job = library.claim();
    if (job) {
      const started = Date.now();
      const result = await scan(job, shutdown.signal);
      const measured = library.finish(job, result);
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
  library.close();
  process.off('SIGTERM', terminate);
  process.off('SIGINT', terminate);
}
