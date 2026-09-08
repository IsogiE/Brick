import path from "node:path";
import { constants, promises as fs } from "node:fs";
import { randomUUID } from "node:crypto";

const MAX_FILE_BYTES = 16 * 1024 * 1024;
const MAX_HISTORY = 10_000;
const MAX_ACTIVE = 2_000;
const MAX_OBSERVATIONS = 4_000;
const RETRY_MS = 60_000;
const ARCHIVE_WAIT_MS = 48 * 60 * 60 * 1000;
const ABANDONED_MS = 30 * 24 * 60 * 60 * 1000;
const memberId = value => typeof value === "string" && /^[0-9]{1,20}$/.test(value);
const twitchId = value => typeof value === "string" && /^[0-9]{1,32}$/.test(value);
const videoId = value => typeof value === "string" && /^[a-zA-Z0-9_-]{11}$/.test(value);
const opaqueId = value => typeof value === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(value);
const title = value => typeof value === "string" ? value.replace(/[\x00-\x1f\x7f]/g, " ").slice(0, 300) : "";
const timestamp = value => typeof value === "string" && value.length <= 40 && Number.isFinite(Date.parse(value))
  ? new Date(value).toISOString() : undefined;
const channelValid = (provider, channel) => provider === "youtube" ? videoId(channel)
  : provider === "twitch" && typeof channel === "string" && /^[a-z0-9_]{1,25}$/.test(channel);
const sessionKey = item => `${item.provider}:${item.provider === "youtube" ? item.channelId : item.streamId}`;
const historyKey = item => `${item.userId}:${item.provider}:${item.id}`;
const vodUrl = (provider, id) => provider === "youtube"
  ? `https://www.youtube.com/watch?v=${id}` : `https://www.twitch.tv/videos/${id}`;

// The caller authenticates directory reads. Construction does not touch disk or
// call a provider; the shared service also invokes observe from its own worker.
export function createVodService({ dataDir, now = Date.now, twitchRequest }) {
  const filePath = path.join(dataDir, "stream-vods.json");
  let state;
  let loading;
  let queue = Promise.resolve();

  async function load() {
    if (state) return state;
    if (!loading) loading = (async () => {
      let parsed;
      try {
        const file = await fs.open(filePath, constants.O_RDONLY | constants.O_NOFOLLOW);
        try {
          const stat = await file.stat();
          if (!stat.isFile() || stat.size > MAX_FILE_BYTES) throw new Error("Invalid stream history file");
          parsed = JSON.parse(await file.readFile("utf8"));
        } finally { await file.close(); }
      } catch (error) {
        if (error.code !== "ENOENT") throw error;
        parsed = { version: 1, active: [], history: [] };
      }
      if (parsed?.version !== 1 || !Array.isArray(parsed.active) || !Array.isArray(parsed.history)
        || parsed.active.length > MAX_ACTIVE || parsed.history.length > MAX_HISTORY) {
        throw new Error("Invalid stream history");
      }
      for (const entry of parsed.active) {
        if (!channelValid(entry?.provider, entry.channelId) || !Array.isArray(entry.owners)
          || !entry.owners.length || entry.owners.length > 1000 || !entry.owners.every(memberId)
          || !Number.isFinite(entry.lastSeenAt) || !Number.isFinite(entry.nextAttemptAt)
          || (entry.provider === "twitch" && (!opaqueId(entry.streamId) || !twitchId(entry.twitchUserId)))
          || typeof entry.live !== "boolean" || typeof entry.title !== "string"
          || entry.title.length > 300 || (entry.endedAt && !timestamp(entry.endedAt))
          || (entry.startedAt && !timestamp(entry.startedAt))) {
          throw new Error("Invalid pending stream archive");
        }
      }
      parsed.history = parsed.history.map(entry => {
        if (!memberId(entry?.userId) || !(entry.provider === "youtube" ? videoId(entry.id)
          : entry.provider === "twitch" && twitchId(entry.id)) || !timestamp(entry.endedAt)
          || typeof entry.title !== "string" || entry.title.length > 300
          || (entry.startedAt && !timestamp(entry.startedAt))) throw new Error("Invalid stream archive");
        return {
          userId: entry.userId, provider: entry.provider, id: entry.id,
          url: vodUrl(entry.provider, entry.id), title: title(entry.title),
          ...(entry.startedAt ? { startedAt: timestamp(entry.startedAt) } : {}),
          endedAt: timestamp(entry.endedAt),
        };
      });
      state = parsed;
      return state;
    })().finally(() => { loading = null; });
    return loading;
  }

  async function persist(next) {
    const serialized = `${JSON.stringify(next)}\n`;
    // A successful write must remain readable on restart. Historical owner
    // associations can grow even while the number of pending sessions is capped.
    if (Buffer.byteLength(serialized, "utf8") > MAX_FILE_BYTES) {
      throw new Error("Stream history storage is full");
    }
    await fs.mkdir(dataDir, { recursive: true, mode: 0o700 });
    const temp = `${filePath}.${randomUUID()}.tmp`;
    let file;
    try {
      file = await fs.open(temp, "wx", 0o600);
      await file.writeFile(serialized);
      await file.sync();
      await file.close();
      file = null;
      await fs.rename(temp, filePath);
      state = next;
    } finally {
      await file?.close();
      await fs.unlink(temp).catch(error => { if (error.code !== "ENOENT") throw error; });
    }
  }

  function addArchive(next, session, id, archiveTitle) {
    const index = new Set(next.history.map(historyKey));
    for (const userId of session.owners) {
      const row = {
        userId, provider: session.provider, id, url: vodUrl(session.provider, id),
        title: title(archiveTitle) || session.title,
        ...(session.startedAt ? { startedAt: session.startedAt } : {}),
        endedAt: session.endedAt,
      };
      if (!index.has(historyKey(row))) {
        next.history.push(row);
        index.add(historyKey(row));
      }
    }
    next.history.sort((a, b) => b.endedAt.localeCompare(a.endedAt) || historyKey(a).localeCompare(historyKey(b)));
    next.history = next.history.slice(0, MAX_HISTORY);
  }

  async function findTwitchArchives(next, signal) {
    if (!twitchRequest) return;
    const due = next.active.filter(session => session.provider === "twitch" && !session.live
      && session.endedAt && session.nextAttemptAt <= now());
    const users = [...new Set(due.map(session => session.twitchUserId))].slice(0, 8);
    const completed = new Set();
    let index = 0;
    await Promise.all(Array.from({ length: Math.min(2, users.length) }, async () => {
      while (index < users.length && !signal?.aborted) {
        const user = users[index++];
        const sessions = due.filter(session => session.twitchUserId === user);
        for (const session of sessions) session.nextAttemptAt = now() + RETRY_MS;
        try {
          const query = new URLSearchParams({ user_id: user, type: "archive", first: "100" });
          const response = await twitchRequest(`/videos?${query}`, signal);
          if (!Array.isArray(response?.data)) continue;
          for (const session of sessions) {
            // A nearby upload or a previous broadcast is never a substitute for
            // the exact stream id observed while this member was live.
            const video = response.data.find(video => video.stream_id === session.streamId
              && video.user_id === user && video.type === "archive" && twitchId(video.id));
            if (video) {
              addArchive(next, session, video.id, video.title);
              completed.add(sessionKey(session));
            }
          }
        } catch { /* Archives can appear later or be disabled; retry within the bounded window. */ }
      }
    }));
    next.active = next.active.filter(session => !completed.has(sessionKey(session)));
  }

  async function observe(observations, signal = AbortSignal.timeout(10_000), { discoverArchives = true } = {}) {
    if (!Array.isArray(observations) || observations.length > MAX_OBSERVATIONS) throw new Error("Invalid stream observations");
    const operation = queue.catch(() => {}).then(async () => {
      const next = structuredClone(await load());
      const before = JSON.stringify(next);
      const currentTime = now();
      for (const observation of observations) {
        if (!channelValid(observation?.provider, observation.channelId)
          || !["live", "offline", "unknown"].includes(observation.status)) continue;
        if (observation.status === "unknown") continue;
        const owners = [...new Set((Array.isArray(observation.owners) ? observation.owners : []).filter(memberId))].slice(0, 1000);
        const matches = next.active.filter(session => session.provider === observation.provider
          && session.channelId === observation.channelId);
        const endedAt = timestamp(observation.endedAt);
        const isLive = observation.status === "live" && !endedAt;
        let session;
        if (isLive) {
          const validIdentity = observation.provider === "youtube"
            || (opaqueId(observation.streamId) && twitchId(observation.twitchUserId));
          if (!validIdentity) continue;
          session = matches.find(item => sessionKey(item) === sessionKey(observation));
          if (!session && owners.length && next.active.length < MAX_ACTIVE) {
            session = {
              provider: observation.provider, channelId: observation.channelId, owners,
              title: title(observation.title), live: true, lastSeenAt: currentTime, nextAttemptAt: 0,
              ...(timestamp(observation.startedAt) ? { startedAt: timestamp(observation.startedAt) } : {}),
              ...(observation.provider === "twitch" ? { streamId: observation.streamId, twitchUserId: observation.twitchUserId } : {}),
            };
            next.active.push(session);
          }
          if (session) {
            session.owners = [...new Set([...session.owners, ...owners])].slice(0, 1000);
            session.title = title(observation.title) || session.title;
            session.live = true;
            session.lastSeenAt = currentTime;
            delete session.endedAt;
          }
        }
        for (const previous of matches) {
          if (isLive && previous === session) continue;
          if (previous.provider === "twitch") {
            previous.live = false;
            previous.endedAt ||= endedAt || new Date(currentTime).toISOString();
          } else if (endedAt) {
            previous.endedAt = endedAt;
            addArchive(next, previous, previous.channelId, observation.title);
            next.active = next.active.filter(item => item !== previous);
          }
        }
        // A saved YouTube live URL can already have ended when Brick next starts.
        // Require the provider's actual end time, never an offline/error guess.
        if (observation.provider === "youtube" && endedAt && owners.length) {
          addArchive(next, {
            provider: "youtube", owners, title: title(observation.title), endedAt,
            ...(timestamp(observation.startedAt) ? { startedAt: timestamp(observation.startedAt) } : {}),
          }, observation.channelId, observation.title);
        }
      }
      next.active = next.active.filter(session => session.endedAt
        ? currentTime - Date.parse(session.endedAt) <= ARCHIVE_WAIT_MS
        : currentTime - session.lastSeenAt <= ABANDONED_MS);
      if (discoverArchives) await findTwitchArchives(next, signal);
      if (JSON.stringify(next) !== before) await persist(next);
    });
    queue = operation.catch(() => {});
    await operation;
  }

  async function list() {
    await queue;
    return (await load()).history.map(entry => ({ ...entry }));
  }

  async function targets() {
    await queue;
    const unique = new Map((await load()).active.map(session => [
      `${session.provider}:${session.channelId}`, { provider: session.provider, channelId: session.channelId },
    ]));
    return [...unique.values()];
  }

  return { observe, list, targets };
}
