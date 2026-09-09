import { HttpError } from "./security.mjs";

export function videoDuration(value) {
  if (typeof value !== "string") return null;
  const match = value.match(/^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/);
  if (!match || !match.slice(1).some(Boolean)) return null;
  const seconds = Number(match[1] || 0) * 3600 + Number(match[2] || 0) * 60 + Number(match[3] || 0);
  return Number.isSafeInteger(seconds) && seconds > 0 && seconds <= 7 * 86400 ? seconds : null;
}

export function youtubeDuration(value) {
  if (typeof value !== "string") return null;
  const match = value.match(/^P(?:(\d+)D)?T(?:(\d+)H)?(?:(\d+)M)?(?:(\d+(?:\.\d+)?)S)?$/);
  if (!match || !match.slice(1).some(Boolean)) return null;
  const seconds = Number(match[1] || 0) * 86400 + Number(match[2] || 0) * 3600
    + Number(match[3] || 0) * 60 + Number(match[4] || 0);
  return Number.isFinite(seconds) && seconds >= 1 && seconds <= 7 * 86400 ? Math.floor(seconds) : null;
}

// Saved recordings are independent of today's live directory. Revalidate the
// exact provider video; never select a newer recording on the same channel.
export function createRecordingReplayService({ twitchRequest, youtubeRequest, now = Date.now }) {
  const cache = new Map();
  let active = 0;
  return async record => {
    const key = JSON.stringify([record.provider, record.id, record.startedAt, record.broadcastId, record.twitchUserId]);
    for (const [id, item] of cache) if (item.until <= now()) cache.delete(id);
    let entry = cache.get(key);
    if (!entry) {
      if (cache.size >= 1000 || active >= 3) throw new HttpError(503, "Replay checks are busy. Try again shortly.");
      active++;
      entry = { until: now() + 30_000 };
      entry.pending = (async () => {
        if (record.provider === "youtube") {
          if (!/^[a-zA-Z0-9_-]{11}$/.test(record.id)) throw new HttpError(400, "Invalid recording.");
          const response = await youtubeRequest(record.id, AbortSignal.timeout(8_000));
          const video = response?.items?.find(video => video.id === record.id);
          if (!video || !["public", "unlisted"].includes(video.status?.privacyStatus) || video.status?.embeddable !== true) {
            throw new HttpError(410, "This recording is no longer available to play.");
          }
          const startedAt = Date.parse(video.liveStreamingDetails?.actualStartTime || record.startedAt);
          const duration = youtubeDuration(video.contentDetails?.duration);
          if (!Number.isFinite(startedAt) || startedAt > now() || !duration) {
            throw new HttpError(409, "This recording's timing is not available yet.");
          }
          return { provider: "youtube", videoId: record.id, broadcastId: record.id,
            startedAt: new Date(startedAt).toISOString(), availableSeconds: duration };
        }
        if (record.provider !== "twitch" || !/^[0-9]{1,30}$/.test(record.id)) throw new HttpError(400, "Invalid recording.");
        const response = await twitchRequest(`/videos?${new URLSearchParams({ id: record.id })}`, AbortSignal.timeout(8_000));
        const video = response?.data?.find(video => video.id === record.id && video.type === "archive"
          && (!record.broadcastId || video.stream_id === record.broadcastId)
          && (!record.twitchUserId || video.user_id === record.twitchUserId));
        if (!video || !/^[0-9]{1,30}$/.test(video.stream_id)) throw new HttpError(410, "This recording is no longer available to play.");
        const startedAt = Date.parse(record.startedAt);
        const duration = videoDuration(video.duration);
        if (!Number.isFinite(startedAt) || startedAt > now() || !duration) {
          throw new HttpError(409, "This recording's broadcast timing is unavailable.");
        }
        return { provider: "twitch", videoId: record.id, broadcastId: video.stream_id,
          startedAt: new Date(startedAt).toISOString(), availableSeconds: duration };
      })().then(value => { entry.value = value; entry.until = now() + 300_000; }, error => {
        entry.error = error instanceof HttpError ? error : new HttpError(502, "Couldn't check this recording.");
      }).finally(() => { active--; });
      cache.set(key, entry);
    }
    await entry.pending;
    if (entry.error) throw entry.error;
    return entry.value;
  };
}

// Discover the replay of this exact broadcast. A newer/nearby video is not proof
// that it contains the selected stream, including after a reconnect.
export function createReplayService({ twitchRequest, now = Date.now }) {
  const cache = new Map();
  return async function replay(entry, state) {
    const startedAt = Date.parse(state?.startedAt);
    if (state?.status !== "live" || !Number.isFinite(startedAt) || startedAt > now()) {
      throw new HttpError(409, "The broadcast timing is not available yet.");
    }
    if (entry.provider === "youtube") {
      return { provider: "youtube", videoId: entry.channelId, broadcastId: entry.channelId,
        startedAt: new Date(startedAt).toISOString(), availableSeconds: Math.max(0, Math.floor((now() - startedAt) / 1000) - 30) };
    }
    if (!/^[0-9]{1,30}$/.test(state.streamId) || !/^[0-9]{1,30}$/.test(state.twitchUserId)) {
      throw new HttpError(409, "The broadcast timing is not available yet.");
    }
    const key = `${state.twitchUserId}:${state.streamId}`;
    for (const [id, item] of cache) if (item.until <= now() && !item.pending) cache.delete(id);
    let cached = cache.get(key);
    if (!cached) {
      if (cache.size >= 1000) throw new HttpError(503, "Replay checks are busy. Try again shortly.");
      cached = { until: now() + 30_000 };
      cached.pending = (async () => {
        const query = new URLSearchParams({ user_id: state.twitchUserId, type: "archive", first: "100" });
        const response = await twitchRequest(`/videos?${query}`, AbortSignal.timeout(8_000));
        const video = response?.data?.find(video => video.stream_id === state.streamId
          && video.user_id === state.twitchUserId && video.type === "archive" && /^[0-9]{1,30}$/.test(video.id));
        const duration = videoDuration(video?.duration);
        if (!video || !duration) throw new HttpError(409, "Twitch has not made this broadcast's replay available yet.");
        return { provider: "twitch", videoId: video.id, broadcastId: state.streamId,
          startedAt: new Date(startedAt).toISOString(), availableSeconds: duration };
      })().then(value => { cached.value = value; }, error => {
        cached.error = error instanceof HttpError ? error : new HttpError(502, "Couldn't check this broadcast's replay.");
      }).finally(() => { cached.pending = null; });
      cache.set(key, cached);
    }
    await cached.pending;
    if (cached.error) throw cached.error;
    return cached.value;
  };
}

// Only transient PKCE authorization codes pass through the service. Access and
// refresh tokens are exchanged by the desktop and kept in its protected store.
export function createLogsHandoff({ now = Date.now }) {
  const pending = new Map();
  const validState = value => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
  const prune = () => { for (const [state, item] of pending) if (item.expiresAt <= now()) pending.delete(state); };
  return {
    receive(params) {
      prune();
      const state = params.get("state");
      const code = params.get("code");
      if (!validState(state) || params.getAll("state").length !== 1 || params.getAll("code").length > 1
        || (!params.has("error") && (typeof code !== "string" || !/^[\x21-\x7e]{1,8192}$/.test(code)))) {
        throw new HttpError(400, "Invalid Warcraft Logs callback.");
      }
      if (pending.has(state)) throw new HttpError(409, "This login callback was already received.");
      if (pending.size >= 512) throw new HttpError(503, "Login is busy. Try again shortly.");
      pending.set(state, { expiresAt: now() + 180_000, used: false,
        payload: params.has("error") ? { error: "Warcraft Logs sign-in was cancelled." } : { code } });
    },
    take(state) {
      prune();
      if (!validState(state)) throw new HttpError(400, "Invalid login state.");
      const item = pending.get(state);
      if (!item || item.used) return null;
      item.used = true;
      const payload = item.payload;
      delete item.payload;
      return payload;
    },
  };
}
