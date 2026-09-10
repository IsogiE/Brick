import { syncKey } from './replay_sync.mjs';

// Provider metadata only: at most two requests at once, independent of HTTP
// lookup latency. No video, log events or credentials are retained here.
export function createReplayWarmup({ streams, library, now = Date.now }) {
  const known = new Map();
  let running = false, closed = false, last = -Infinity;
  function remember(stream, replay) {
    for (const [id, row] of known) if (row.until <= now()) known.delete(id);
    if (known.size >= 1000) known.delete(known.keys().next().value);
    known.set(`${replay.provider}/${replay.videoId}`, { ...replay, userId: stream.userId, until: now()+900_000 });
    return replay;
  }
  function allows(key, eligible) {
    const replay = known.get(`${key.provider}/${key.videoId}`);
    return !!replay && replay.until > now() && eligible.has(replay.userId)
      && replay.broadcastId === key.broadcastId && Date.parse(replay.startedAt) === key.recordingStartMs;
  }
  async function prepare(keys, requester, members) {
    if (closed || running || now()-last < 30_000 || !keys.length) return;
    running = true; last = now();
    try {
      const snapshot = await streams.snapshot(requester, members);
      const live = snapshot.streams.slice(0,40);
      let cursor = 0;
      await Promise.all(Array.from({length:2}, async () => {
        while (!closed && cursor < live.length) {
          const stream = live[cursor++];
          try {
            const replay = remember(stream, await streams.replay(stream));
            const start = Date.parse(replay.startedAt);
            const jobs = keys.slice(0,2).filter(p => p.startMs >= start && p.startMs < start+replay.availableSeconds*1000)
              .map(p => syncKey({...p, provider: replay.provider, videoId: replay.videoId,
                broadcastId: replay.broadcastId, recordingStartMs: start}).key);
            if (!closed) library.enqueue(jobs);
          } catch (_) { /* A live archive may not be ready yet. Next metadata pass retries. */ }
        }
      }));
    } catch (_) {} finally { running = false; }
  }
  return { remember, allows, prepare, close() { closed = true; known.clear(); } };
}
