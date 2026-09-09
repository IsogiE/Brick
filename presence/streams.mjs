import path from "node:path";
import { constants, promises as fs } from "node:fs";
import { randomUUID } from "node:crypto";
import { HttpError, readBounded } from "./security.mjs";
import { createVodService } from "./stream_vods.mjs";
import { youtubeControlTags, twitchControlTags } from "./player_control.mjs";
import { createReplayService } from "./stream_review.mjs";

const REFRESH_MS = 30_000;
const YOUTUBE_DAILY_BUDGET = 8_000;
const YOUTUBE_FRESH_MS = 90_000;
const PROVIDER_BACKOFF_MS = 15 * 60_000;
const FIRST_CHECK_WAIT_MS = 5_000;
const FIRST_CHECK_QUEUE_LIMIT = 128;
const FIRST_CHECK_YOUTUBE_DAILY_BUDGET = 256;
const MAX_STREAMS = 1000;
const MAX_FILE_BYTES = 1024 * 1024;
const TWITCH_RESERVED = new Set(["directory", "downloads", "jobs", "p", "search", "settings", "subscriptions", "turbo", "videos", "wallet"]);

export function parseStreamUrl(value) {
  const invalid = () => { throw new HttpError(400, "Enter a Twitch channel URL or a YouTube live video URL."); };
  if (typeof value !== "string" || value.length > 2048) return invalid();
  const input = value.trim();
  // Parse URLs only. Never request a submitted URL or follow a redirect.
  if (!/^https:\/\//i.test(input) || /[\s\\\x00-\x1f\x7f]/.test(input)) return invalid();
  let url;
  try { url = new URL(input); } catch { return invalid(); }
  if (url.username || url.password || url.port) return invalid();
  const host = url.hostname.toLowerCase();
  if (["twitch.tv", "www.twitch.tv", "m.twitch.tv"].includes(host)) {
    const match = url.pathname.match(/^\/([a-zA-Z0-9_]{1,25})\/?$/);
    if (!match || TWITCH_RESERVED.has(match[1].toLowerCase())) return invalid();
    const channelId = match[1].toLowerCase();
    return { provider: "twitch", channelId, url: `https://www.twitch.tv/${channelId}` };
  }
  let channelId;
  if (["youtube.com", "www.youtube.com", "m.youtube.com"].includes(host)) {
    if (url.pathname === "/watch" && url.searchParams.getAll("v").length === 1) {
      channelId = url.searchParams.get("v");
    } else {
      channelId = url.pathname.match(/^\/live\/([a-zA-Z0-9_-]{11})\/?$/)?.[1];
    }
  } else if (host === "youtu.be") {
    channelId = url.pathname.match(/^\/([a-zA-Z0-9_-]{11})\/?$/)?.[1];
  }
  if (!channelId || !/^[a-zA-Z0-9_-]{11}$/.test(channelId)) return invalid();
  return { provider: "youtube", channelId, url: `https://www.youtube.com/watch?v=${channelId}` };
}

export function parsePlayerOrigin(value) {
  if (!value?.trim()) return null;
  let origin;
  try { origin = new URL(value); } catch { throw new Error("BRICK_PLAYER_ORIGIN must be an HTTPS origin."); }
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(origin.hostname);
  if (origin.username || origin.password || origin.pathname !== "/" || origin.search || origin.hash
    || (origin.protocol !== "https:" && !(origin.protocol === "http:" && local))) {
    throw new Error("BRICK_PLAYER_ORIGIN must be an HTTPS origin (HTTP is allowed on loopback for local tests).");
  }
  return origin;
}

export function streamPlayerPage(stream, origin, playback = null) {
  if (!origin) throw new HttpError(503, "Stream playback is not configured.");
  const embed = stream.provider === "twitch"
    ? new URL("https://player.twitch.tv/")
    : new URL(`https://www.youtube.com/embed/${stream.channelId}`);
  if (stream.provider === "twitch") {
    embed.search = new URLSearchParams({ channel: stream.channelId, parent: origin.hostname, autoplay: playback?.paused ? "false" : "true", muted: "true" });
    if (playback) {
      embed.searchParams.delete("channel");
      embed.searchParams.set("video", `v${playback.videoId}`);
      embed.searchParams.set("time", `${playback.seconds}s`);
    }
  } else {
    embed.search = new URLSearchParams({ autoplay: playback?.paused ? "0" : "1", mute: "1", playsinline: "1", enablejsapi: "1", origin: origin.origin });
    // YouTube's URL start parameter is integral. Its player API receives the
    // precise offset separately so switching views does not discard milliseconds.
    if (playback) embed.searchParams.set("start", String(Math.floor(playback.seconds)));
  }
  const escaped = embed.href.replaceAll("&", "&amp;").replaceAll('"', "&quot;");
  const preciseStart = playback ? ` data-start="${Number(playback.seconds).toFixed(3)}"` : "";
  const iframe = `<iframe id="media"${preciseStart} title="Guild stream" src="${escaped}" referrerpolicy="strict-origin-when-cross-origin" allow="autoplay; encrypted-media; fullscreen; picture-in-picture" allowfullscreen></iframe>`;
  const media = stream.provider === "twitch" && playback
    ? `<div id="media" data-src="${escaped}"></div>${twitchControlTags}`
    : `${iframe}${stream.provider === "youtube" ? youtubeControlTags : ""}`;
  return `<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Brick stream</title><style>html,body,iframe,#media{margin:0;width:100%;height:100%;border:0;background:#14161a;overflow:hidden}iframe{display:block}</style></head><body>${media}</body></html>`;
}

export async function createStreamService({ dataDir, env, fetch, now = Date.now, firstCheckWaitMs = FIRST_CHECK_WAIT_MS }) {
  const filePath = path.join(dataDir, "streams.json");
  const credential = async (name) => env[`${name}_FILE`]
    ? (await fs.readFile(env[`${name}_FILE`], "utf8")).trim() : env[name]?.trim() || "";
  const [clientId, clientSecret, youtubeKey] = await Promise.all([
    credential("TWITCH_CLIENT_ID"), credential("TWITCH_CLIENT_SECRET"), credential("YOUTUBE_API_KEY"),
  ]);
  const providers = { twitch: Boolean(clientId && clientSecret), youtube: Boolean(youtubeKey) };
  const playerOrigin = parsePlayerOrigin(env.BRICK_PLAYER_ORIGIN);
  let registrations = null;
  let loading = null;
  let writes = Promise.resolve();
  let refresh = null;
  let refreshAt = 0;
  let firstCheckTimer = null;
  const firstChecks = new Map();
  const manualYoutubeChecks = [];
  let statuses = new Map();
  let youtubeNextCheckAt = 0;
  let youtubeBackoffUntil = 0;
  const youtubeCache = new Map();
  let twitchToken = null;
  const shutdown = new AbortController();
  const twitchRequest = async (pathname, signal) => {
    if (!providers.twitch || !/^\/videos\?/.test(pathname)) throw new Error("Twitch recording requests are unavailable");
    const token = await getTwitchToken(signal);
    try {
      return await providerJson(`https://api.twitch.tv/helix${pathname}`, {
        headers: { authorization: `Bearer ${token}`, "client-id": clientId },
      }, signal);
    } catch (error) { if (error.status === 401) twitchToken = null; throw error; }
  };
  const vods = await createVodService({ dataDir, fetch, now, twitchRequest });
  const replay = createReplayService({ twitchRequest, now });

  async function load() {
    if (registrations) return registrations;
    if (!loading) loading = (async () => {
      let parsed;
      try {
        const file = await fs.open(filePath, constants.O_RDONLY | constants.O_NOFOLLOW);
        try {
          const stat = await file.stat();
          if (!stat.isFile() || stat.size > MAX_FILE_BYTES) throw new Error("Invalid stream registry file");
          parsed = JSON.parse(await file.readFile("utf8"));
        } finally { await file.close(); }
      } catch (error) {
        if (error.code !== "ENOENT") throw error;
        parsed = { version: 2, users: {} };
      }
      if (![1, 2].includes(parsed?.version) || !parsed.users || typeof parsed.users !== "object" || Array.isArray(parsed.users)
        || Object.keys(parsed.users).length > MAX_STREAMS) throw new Error("Invalid stream registry");
      const users = new Map();
      for (const [userId, entry] of Object.entries(parsed.users)) {
        if (!/^[0-9]{1,20}$/.test(userId)) throw new Error("Invalid stream registry member");
        const platforms = new Map();
        if (parsed.version === 1) {
          const stream = parseStreamUrl(entry?.url);
          platforms.set(stream.provider, stream);
        } else {
          if (!entry || typeof entry !== "object" || Array.isArray(entry)) throw new Error("Invalid stream registry platforms");
          for (const [provider, value] of Object.entries(entry)) {
            const stream = parseStreamUrl(value?.url);
            if (provider !== stream.provider || !["twitch", "youtube"].includes(provider)) throw new Error("Invalid stream registry provider");
            platforms.set(provider, stream);
          }
        }
        if (platforms.size) users.set(userId, platforms);
      }
      registrations = users;
      return users;
    })().finally(() => { loading = null; });
    return loading;
  }

  async function update(userId, entry, provider) {
    const run = writes.catch(() => {}).then(async () => {
      const next = new Map(await load());
      if (entry) {
        if (!next.has(userId) && next.size >= MAX_STREAMS) throw new HttpError(503, "The stream directory is full.");
        const platforms = new Map(next.get(userId));
        platforms.set(entry.provider, entry);
        next.set(userId, platforms);
      } else if (provider) {
        const platforms = new Map(next.get(userId));
        platforms.delete(provider);
        if (platforms.size) next.set(userId, platforms);
        else next.delete(userId);
      } else next.delete(userId);
      await fs.mkdir(dataDir, { recursive: true });
      const temp = `${filePath}.${randomUUID()}.tmp`;
      let file;
      try {
        file = await fs.open(temp, "wx", 0o600);
        await file.writeFile(`${JSON.stringify({ version: 2, users: Object.fromEntries([...next].map(([id, platforms]) => [id, Object.fromEntries(platforms)])) })}\n`);
        await file.sync();
        await file.close();
        file = null;
        await fs.rename(temp, filePath);
        registrations = next;
      } finally {
        await file?.close();
        await fs.unlink(temp).catch(error => { if (error.code !== "ENOENT") throw error; });
      }
    });
    writes = run.catch(() => {});
    await run;
  }

  async function providerJson(url, options, signal) {
    const response = await fetch(url, { ...options, signal, redirect: "error" });
    if (!response.ok) {
      await response.body?.cancel();
      const error = new Error("Stream provider request failed");
      error.status = response.status;
      const retryAfter = response.headers.get("retry-after");
      if (retryAfter && retryAfter.length <= 100) {
        const delay = /^\d+$/.test(retryAfter) ? Number(retryAfter) * 1000 : Date.parse(retryAfter) - now();
        if (Number.isFinite(delay) && delay > 0) error.retryAfterMs = Math.min(PROVIDER_BACKOFF_MS, delay);
      }
      throw error;
    }
    return JSON.parse(await readBounded(response, 1024 * 1024));
  }

  async function getTwitchToken(signal) {
    if (twitchToken?.expiresAt > now()) return twitchToken.token;
    const body = new URLSearchParams({ client_id: clientId, client_secret: clientSecret, grant_type: "client_credentials" });
    const response = await providerJson("https://id.twitch.tv/oauth2/token", {
      method: "POST", headers: { "content-type": "application/x-www-form-urlencoded" }, body,
    }, signal);
    if (typeof response.access_token !== "string" || !/^[\x21-\x7e]{1,4096}$/.test(response.access_token)
      || !Number.isFinite(response.expires_in) || response.expires_in <= 0) throw new Error("Invalid Twitch token response");
    twitchToken = { token: response.access_token, expiresAt: now() + Math.max(0, response.expires_in - 60) * 1000 };
    return twitchToken.token;
  }

  const keyFor = entry => `${entry.provider}:${entry.channelId}`;
  const uncheckedStatus = entry => providers[entry.provider]
    && !(entry.provider === "youtube" && now() < youtubeBackoffUntil) ? "checking" : "unknown";
  const title = value => typeof value === "string" ? value.replace(/[\x00-\x1f\x7f]/g, " ").slice(0, 300) : "";
  const viewers = value => Number.isSafeInteger(Number(value)) && Number(value) >= 0 && value !== null && value !== ""
    ? { viewerCount: Number(value) } : {};

  function manualYoutubeAvailable() {
    while (manualYoutubeChecks.length && manualYoutubeChecks[0] <= now() - 86_400_000) manualYoutubeChecks.shift();
    return manualYoutubeChecks.length < FIRST_CHECK_YOUTUBE_DAILY_BUDGET;
  }

  async function refreshStatuses(entries, targeted = false) {
    // Schedule from the start so a slow provider or recording write cannot
    // add another whole interval after an earlier result expires.
    const startedAt = now();
    // A single deadline and worker pool bound all provider activity, including token acquisition.
    const signal = AbortSignal.any([AbortSignal.timeout(targeted ? FIRST_CHECK_WAIT_MS : 10_000), shutdown.signal]);
    const submissions = targeted ? new Map(entries.map(entry => [keyFor(entry), entry])) : null;
    const targets = new Map();
    for (const entry of [...entries, ...(targeted ? [] : await vods.targets())]) {
      const key = keyFor(entry);
      const target = targets.get(key) || { provider: entry.provider, channelId: entry.channelId, owners: [] };
      target.owners = [...new Set([...target.owners, ...(entry.owners || [])])];
      targets.set(key, target);
    }
    entries = [...targets.values()];
    const jobs = [];
    // A saved target can wait for the next shared provider round. Keep that
    // initial wait distinct from an attempted check that failed or went stale.
    const result = new Map(entries.map(entry => {
      const previous = statuses.get(keyFor(entry));
      return [keyFor(entry), { status: !previous || previous.status === "checking"
        ? uncheckedStatus(entry) : "unknown", title: "" }];
    }));
    const observed = new Set();
    let token;
    const twitch = [...new Set(entries.filter(entry => entry.provider === "twitch").map(entry => entry.channelId))];
    const youtube = [...new Set(entries.filter(entry => entry.provider === "youtube").map(entry => entry.channelId))];
    const youtubeTargets = new Set(youtube);
    if (providers.twitch && twitch.length) {
      for (const id of twitch) result.set(`twitch:${id}`, { status: "unknown", title: "", checkedAt: now() });
      try { token = await getTwitchToken(signal); } catch { /* Unavailable stays unknown until the next shared refresh. */ }
    }
    if (token) for (let start = 0; start < twitch.length; start += 100) {
      const ids = twitch.slice(start, start + 100);
      jobs.push(async () => {
        const query = new URLSearchParams({ first: "100" });
        ids.forEach(id => query.append("user_login", id));
        let body;
        try {
          body = await providerJson(`https://api.twitch.tv/helix/streams?${query}`, {
            headers: { authorization: `Bearer ${token}`, "client-id": clientId },
          }, signal);
        } catch (error) { if (error.status === 401) twitchToken = null; throw error; }
        // At most 100 filtered channels fit in the requested 100-item page.
        // Twitch can still return a cursor for a complete short page.
        const requested = new Set(ids);
        if (!Array.isArray(body?.data) || body.data.length > ids.length
          || body.data.some(item => typeof item?.user_login !== "string" || item.type !== "live"
            || !requested.has(item.user_login.toLowerCase()))) {
          throw new Error("Invalid Twitch streams response");
        }
        const live = new Map(body.data.map(item => [item.user_login.toLowerCase(), item]));
        if (live.size !== body.data.length) throw new Error("Invalid Twitch streams response");
        for (const id of ids) {
          const item = live.get(id);
          result.set(`twitch:${id}`, item
            ? { status: "live", title: title(item.title), ...viewers(item.viewer_count), checkedAt: now(),
              startedAt: item.started_at, twitchUserId: item.user_id, streamId: item.id }
            : { status: "offline", title: "", checkedAt: now() });
          observed.add(`twitch:${id}`);
        }
      });
    }
    const checkYoutube = providers.youtube && youtube.length && now() >= youtubeBackoffUntil
      && (targeted ? manualYoutubeAvailable() : now() >= youtubeNextCheckAt);
    if (checkYoutube) {
      if (!targeted) for (const id of youtube) result.set(`youtube:${id}`, { status: "unknown", title: "", checkedAt: now() });
      // videos.list costs one unit per batch. Keep room in the daily project
      // quota as registrations and pending recording targets grow.
      if (!targeted) youtubeNextCheckAt = now() + Math.max(REFRESH_MS,
        Math.ceil(Math.ceil(youtube.length / 50) * 86_400_000 / YOUTUBE_DAILY_BUDGET));
      for (const id of youtube) youtubeCache.delete(id);
    }
    if (checkYoutube) for (let start = 0; start < youtube.length; start += 50) {
      const ids = youtube.slice(start, start + 50);
      jobs.push(async () => {
        if (now() < youtubeBackoffUntil) return;
        // Manual checks use a small, bounded part of the quota headroom left
        // above the normal 8,000-unit schedule; they never bypass backoff.
        if (targeted) {
          if (!manualYoutubeAvailable()) return;
          manualYoutubeChecks.push(now());
          for (const id of ids) result.set(`youtube:${id}`, { status: "unknown", title: "", checkedAt: now() });
        }
        const query = new URLSearchParams({ part: "snippet,status,liveStreamingDetails", id: ids.join(","), key: youtubeKey });
        let body;
        try {
          body = await providerJson(`https://www.googleapis.com/youtube/v3/videos?${query}`, {}, signal);
        } catch (error) {
          if ([403, 429].includes(error.status)) youtubeBackoffUntil = Math.max(youtubeBackoffUntil,
            now() + (error.status === 403 ? PROVIDER_BACKOFF_MS : Math.max(REFRESH_MS, error.retryAfterMs || 60_000)));
          throw error;
        }
        if (!Array.isArray(body?.items) || body.items.some(item => typeof item?.id !== "string" || !item.snippet || !item.status)) {
          throw new Error("Invalid YouTube videos response");
        }
        const videos = new Map(body.items.map(item => [item.id, item]));
        for (const id of ids) {
          const item = videos.get(id);
          const details = item?.liveStreamingDetails;
          const live = item?.snippet?.liveBroadcastContent === "live" && !details?.actualEndTime;
          const status = item?.status?.privacyStatus === "private" || (live && item.status.embeddable === false)
            ? "unknown" : live ? "live" : "offline";
          youtubeCache.set(id, { checkedAt: now(), state: { status, title: title(item?.snippet?.title), ...(live ? viewers(details?.concurrentViewers) : {}),
            startedAt: details?.actualStartTime, endedAt: details?.actualEndTime,
            ...(status !== "unknown" ? { broadcastState: details?.actualEndTime ? "ended" : live ? "live"
              : item?.snippet?.liveBroadcastContent === "upcoming" ? "upcoming" : "notLive" } : {}) } });
          observed.add(`youtube:${id}`);
        }
      });
    }
    await Promise.all(Array.from({ length: Math.min(3, jobs.length) }, async () => {
      while (jobs.length && !signal.aborted) {
        try { await jobs.shift()(); } catch { /* Do not log provider URLs, submitted IDs, or credentials. */ }
      }
    }));
    for (const [id, cached] of youtubeCache) {
      if (targeted && !youtubeTargets.has(id)) continue;
      if (now() < cached.checkedAt + YOUTUBE_FRESH_MS) result.set(`youtube:${id}`, { ...cached.state, checkedAt: cached.checkedAt });
    }
    // Failed batches immediately withdraw previously live entries. An outage never reports them offline.
    if (targeted) {
      for (const [key, state] of result) statuses.set(key, state);
      if (refreshAt === 0) refreshAt = startedAt + REFRESH_MS;
    } else {
      // A save can finish while a normal provider round is in flight. Retain
      // a fresh first-check result for any newly registered target it omitted.
      const registered = new Set([...(registrations?.values() || [])].flatMap(platforms => [...platforms.values()].map(keyFor)));
      const retained = new Set([...targets.keys(), ...registered]);
      for (const [key, state] of statuses) if (!result.has(key) && registered.has(key)) result.set(key, state);
      for (const key of result.keys()) if (!retained.has(key)) result.delete(key);
      for (const id of youtubeCache.keys()) if (!retained.has(`youtube:${id}`)) youtubeCache.delete(id);
      statuses = result;
      refreshAt = startedAt + REFRESH_MS;
    }
    try {
      await vods.observe(entries.map(entry => ({ ...entry,
        ...(observed.has(keyFor(entry)) ? result.get(keyFor(entry)) : { status: "unknown", title: "" }),
        ...(targeted ? { owners: submissions.get(keyFor(entry)).owners } : {}) })), signal, { discoverArchives: !targeted });
    } catch {
      // A recording-store/provider failure must not turn a verified live status into offline.
      console.error("Stream recording refresh failed");
    }
  }

  function freshCache(entry) {
    const cached = statuses.get(keyFor(entry));
    const lifetime = entry.provider === "youtube" ? YOUTUBE_FRESH_MS : REFRESH_MS;
    return cached?.checkedAt !== undefined && now() < cached.checkedAt + lifetime ? cached : null;
  }

  function row(userId, name, entry) {
    const cached = statuses.get(keyFor(entry));
    const state = freshCache(entry);
    return { userId, name, ...entry, status: state?.status || (!cached || cached.status === "checking"
      ? uncheckedStatus(entry) : "unknown"), title: state?.title || "",
      ...(state?.viewerCount !== undefined ? { viewerCount: state.viewerCount } : {}),
      ...(state?.broadcastState ? { broadcastState: state.broadcastState } : {}) };
  }

  function finishRefresh() {
    refresh = null;
    scheduleFirstChecks();
  }

  function scheduleFirstChecks() {
    if (shutdown.signal.aborted || refresh || firstCheckTimer || !firstChecks.size) return;
    firstCheckTimer = setTimeout(() => {
      firstCheckTimer = null;
      if (refresh || shutdown.signal.aborted) return;
      const batch = [];
      for (const job of firstChecks.values()) {
        if (job.running) continue;
        const cached = freshCache(job.entry);
        const registered = job.entry.owners.some(id => registrations.get(id)?.get(job.entry.provider)?.channelId === job.entry.channelId);
        if (!registered || (cached && cached.status !== "checking")) {
          firstChecks.delete(keyFor(job.entry));
          job.resolve();
        } else batch.push(job);
      }
      if (!batch.length) return;
      for (const job of batch) job.running = true;
      // The same work lock protects normal and manual rounds. Their existing
      // three-worker pool is the total concurrency limit, not one pool per save.
      refresh = refreshStatuses(batch.map(job => job.entry), true).catch(() => {}).finally(() => {
        for (const job of batch) {
          firstChecks.delete(keyFor(job.entry));
          job.resolve();
        }
        finishRefresh();
      });
    }, 50);
  }

  async function firstCheck(entry, userId, waitMs) {
    if (waitMs <= 0 || shutdown.signal.aborted || !providers[entry.provider]
      || (entry.provider === "youtube" && (now() < youtubeBackoffUntil || !manualYoutubeAvailable()))) return;
    const key = keyFor(entry);
    const cached = freshCache(entry);
    if (cached && cached.status !== "checking") return;
    let job = firstChecks.get(key);
    if (!job) {
      if (firstChecks.size >= FIRST_CHECK_QUEUE_LIMIT) return;
      let resolve;
      const promise = new Promise(done => { resolve = done; });
      job = { entry: { ...entry, owners: [userId] }, promise, resolve, waiters: 0, running: false };
      firstChecks.set(key, job);
    } else job.entry.owners = [...new Set([...job.entry.owners, userId])];
    job.waiters++;
    scheduleFirstChecks();
    try {
      await new Promise(resolve => {
        const timer = setTimeout(resolve, waitMs);
        job.promise.then(() => { clearTimeout(timer); resolve(); });
      });
    } finally {
      job.waiters--;
      if (!job.waiters && !job.running) {
        firstChecks.delete(key);
        job.resolve();
      }
    }
  }

  async function poll(members) {
    // Give a due normal round priority after an in-flight first check.
    if (refresh) await refresh;
    await writes;
    const users = await load();
    const eligible = new Map(members.map(member => [member.userId, member.name]));
    const entries = [...users].filter(([id]) => eligible.has(id)).flatMap(([userId, platforms]) =>
      [...platforms.values()].map(entry => ({ ...entry, owners: [userId] })));
    if (now() >= refreshAt && !refresh) refresh = refreshStatuses(entries).finally(finishRefresh);
    if (refresh) await refresh;
  }

  async function snapshot(requester, members) {
    await poll(members);
    const eligible = new Map(members.map(member => [member.userId, member.name]));
    const current = registrations;
    const checked = [...current].filter(([id]) => eligible.has(id))
      .flatMap(([id, platforms]) => [...platforms.values()].map(entry => row(id, eligible.get(id), entry)));
    const streams = checked.filter(entry => entry.status === "live")
      .sort((a, b) => a.name.localeCompare(b.name, "en", { sensitivity: "base" }) || a.userId.localeCompare(b.userId) || a.provider.localeCompare(b.provider));
    const ownStreams = [...(current.get(requester.id)?.values() || [])].map(entry => row(requester.id, requester.name, entry))
      .sort((a, b) => a.provider.localeCompare(b.provider));
    return {
      generatedAt: new Date(now()).toISOString(), refreshAfterSeconds: REFRESH_MS / 1000,
      streams, ownStreams, ownStream: ownStreams[0] || null, providers,
      unverifiedCount: checked.filter(entry => entry.status === "unknown").length,
    };
  }

  return {
    snapshot, playerOrigin, poll,
    async replay(stream) { return replay(stream, freshCache(stream)); },
    nextPollDelayMs: () => refreshAt ? Math.max(1_000, Math.min(REFRESH_MS, refreshAt - now())) : REFRESH_MS,
    async recordings() { return vods.list(); },
    close() {
      shutdown.abort();
      clearTimeout(firstCheckTimer);
      firstCheckTimer = null;
      for (const job of firstChecks.values()) job.resolve();
      firstChecks.clear();
    },
    async needsPolling() { return (await load()).size > 0 || (await vods.targets()).length > 0; },
    async save(requester, value, { checkTimeoutMs = firstCheckWaitMs } = {}) {
      const deadline = performance.now() + Math.max(0, Math.min(FIRST_CHECK_WAIT_MS, firstCheckWaitMs, checkTimeoutMs));
      const entry = parseStreamUrl(value);
      await update(requester.id, entry);
      // Persistence succeeds independently of provider availability. A failed
      // or delayed first check must never turn a saved link into a failed save.
      await firstCheck(entry, requester.id, deadline - performance.now());
      const current = registrations.get(requester.id);
      const saved = current?.get(entry.provider);
      return { ownStream: saved ? row(requester.id, requester.name, saved) : null,
        ownStreams: [...(current?.values() || [])].map(stream => row(requester.id, requester.name, stream)) };
    },
    async remove(requester, provider) {
      if (provider !== undefined && !["twitch", "youtube"].includes(provider)) throw new HttpError(400, "Choose Twitch or YouTube to remove.");
      await update(requester.id, null, provider);
      return { ok: true };
    },
  };
}
