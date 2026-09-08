import http from "node:http";
import path from "node:path";
import { promises as fs } from "node:fs";
import { createHash, randomUUID } from "node:crypto";
import { pathToFileURL } from "node:url";
import { HttpError, RateLimit, readBounded, clientAddress } from "./security.mjs";
import { createStreamService, streamPlayerPage } from "./streams.mjs";

export async function createPresenceServer({ env = process.env, fetch = globalThis.fetch, now = Date.now, firstStreamCheckWaitMs = 5_000 } = {}) {
  const DISCORD_API = "https://discord.com/api/v10";
  const MAX_BODY_BYTES = 32 * 1024;
  const GATEWAY_INTENTS = 1;
  const GATEWAY_RECONNECT_MIN_MS = 5_000;
  const GATEWAY_RECONNECT_MAX_MS = 60_000;

  const dataDir = env.DATA_DIR || "/data";
  const heartbeatFile = path.join(dataDir, "heartbeats.json");

  const guildId = requireEnv("DISCORD_GUILD_ID");
  const botToken = env.DISCORD_BOT_TOKEN_FILE
    ? (await fs.readFile(env.DISCORD_BOT_TOKEN_FILE, "utf8")).trim()
    : requireEnv("DISCORD_BOT_TOKEN");
  if (!botToken) throw new Error("Discord bot token is empty");
  const officerRoleId = requireEnv("DISCORD_OFFICER_ROLE_ID");
  const raiderRoleId = requireEnv("DISCORD_RAIDER_ROLE_ID");
  const onlineWindowSeconds = parsePositiveInt(env.ONLINE_WINDOW_SECONDS, 180);
  const rosterCacheSeconds = parsePositiveInt(env.ROSTER_CACHE_SECONDS, 60);
  const heartbeatRetentionSeconds = parsePositiveInt(
    env.HEARTBEAT_RETENTION_SECONDS,
    14 * 24 * 60 * 60,
  );
  const requesterCacheSeconds = parsePositiveInt(env.REQUESTER_CACHE_SECONDS, 5 * 60);
  const requesterStaleSeconds = parsePositiveInt(env.REQUESTER_STALE_SECONDS, 60 * 60);
  const oauthHandoffTtlSeconds = parsePositiveInt(env.OAUTH_HANDOFF_TTL_SECONDS, 3 * 60);
  const oauthHandoffMaxEntries = parsePositiveInt(env.OAUTH_HANDOFF_MAX_ENTRIES, 512);
  const gatewayEnabled = env.DISCORD_GATEWAY_ENABLED?.trim().toLowerCase() !== "false";
  const streams = await createStreamService({ dataDir, env, fetch, now, firstCheckWaitMs: firstStreamCheckWaitMs });
  const requestDeadlines = new WeakMap();

  let rosterCache = null;
  let rosterInFlight = null;
  let discordInFlight = 0;
  const discordRetryAt = new Map();
  const requests = new RateLimit(600, 10, 300, 5);
  const verifications = new RateLimit(60, 1, 12, 0.2);
  const callbacks = new RateLimit(60, 0.5, 10, 0.1);
  const streamChanges = new RateLimit(120, 1, 6, 1 / 10);
  const failures = new Map();
  const address = (request) => clientAddress(request, env.TRUST_PROXY === "true");
  const oauthHandoffs = new Map();
  const requesterCache = new Map();
  const requesterInFlight = new Map();
  let heartbeatWriteQueue = Promise.resolve();

  function requireEnv(name) {
    const value = env[name]?.trim();
    if (!value) {
      throw new Error(`${name} is required`);
    }
    return value;
  }

  function parsePositiveInt(value, fallback) {
    const number = Number.parseInt(value ?? "", 10);
    return Number.isFinite(number) && number > 0 ? number : fallback;
  }

  function sendJson(response, status, payload) {
    response.writeHead(status, {
      "x-content-type-options": "nosniff",
      "referrer-policy": "no-referrer",
      "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
      ...(status === 429 || status === 503 ? { "retry-after": "5" } : {}),
      "content-type": "application/json; charset=utf-8",
      "cache-control": "no-store",
    });
    response.end(JSON.stringify(payload));
  }

  function sendHtml(response, status, body, player = false) {
    response.writeHead(status, {
      "x-content-type-options": "nosniff",
      "referrer-policy": player ? "strict-origin-when-cross-origin" : "no-referrer",
      "content-security-policy": `default-src 'none'; style-src 'unsafe-inline'; ${player ? "frame-src https://player.twitch.tv https://www.youtube.com; " : ""}frame-ancestors 'none'; base-uri 'none'; form-action 'none'`,
      ...(status === 429 || status === 503 ? { "retry-after": "5" } : {}),
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
    });
    response.end(body);
  }

  function bearerToken(request) {
    const auth = request.headers.authorization || "";
    const match = auth.match(/^Bearer\s+(.+)$/i);
    const token = match?.[1]?.trim();
    return token && token.length <= 2048 && /^[\x21-\x7e]+$/.test(token) ? token : null;
  }

  function requestUrl(request) {
    return new URL(request.url || "/", "http://localhost");
  }

  function callbackPage(title, body) {
    return `<!doctype html><html><head><meta charset="utf-8"><title>${title}</title><style>body{margin:0;background:#15171c;color:#f1f4f8;font:16px system-ui,sans-serif;display:grid;place-items:center;height:100vh}main{border:1px solid #2f343d;border-radius:10px;background:#1d2026;padding:28px 34px;box-shadow:0 24px 60px rgba(0,0,0,.35)}h1{margin:0 0 8px;font-size:24px}p{margin:0;color:#b7c2d0}</style></head><body><main><h1>${title}</h1><p>${body}</p></main></body></html>`;
  }

  async function readJsonBody(request) {
    let bytes = 0;
    const chunks = [];

    for await (const chunk of request) {
      bytes += chunk.length;
      if (bytes > MAX_BODY_BYTES) {
        throw new HttpError(413, "Request body is too large.");
      }
      chunks.push(chunk);
    }

    if (chunks.length === 0) {
      return {};
    }

    try {
      const value = JSON.parse(Buffer.concat(chunks).toString("utf8"));
      if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error();
      return value;
    } catch {
      throw new HttpError(400, "Request body must be valid JSON.");
    }
  }

  async function discordJson(pathname, authorization) {
    const kind = authorization.startsWith("Bot ") ? "bot" : "user";
    if (discordRetryAt.get(kind) > Date.now()) throw new HttpError(429, "Discord is busy. Try again shortly.");
    if (discordInFlight >= 16) throw new HttpError(503, "Discord requests are busy. Try again shortly.");
    discordInFlight++;
    try {
      const response = await fetch(`${DISCORD_API}${pathname}`, {
        headers: { authorization, "user-agent": "BrickPresence/1.0" },
        redirect: "error",
        signal: AbortSignal.timeout(10_000),
      });
      const text = await readBounded(response, 2 * 1024 * 1024);
      let body;
      try { body = text ? JSON.parse(text) : null; }
      catch { throw new HttpError(502, "Discord returned an invalid response."); }
      if (!response.ok) {
        if (response.status === 429) {
          const delay = Math.min(60, Math.max(1, Number(body?.retry_after) || 5));
          discordRetryAt.set(kind, Date.now() + delay * 1000);
        }
        const status = [401, 403, 429].includes(response.status) ? response.status : 502;
        // Do not reflect upstream error text, which may contain request data.
        throw new HttpError(status, `Discord request failed (HTTP ${response.status}).`);
      }
      return body;
    } finally {
      discordInFlight--;
    }
  }

  function roleFromIds(roleIds) {
    if (!Array.isArray(roleIds)) {
      return null;
    }
    if (roleIds.includes(officerRoleId)) {
      return "Officer";
    }
    if (roleIds.includes(raiderRoleId)) {
      return "Raider";
    }
    return null;
  }

  function displayName(member, user) {
    return optionalText(
      member?.nick ||
      member?.user?.global_name ||
      member?.user?.username ||
      user?.global_name ||
      user?.username ||
      "Unknown", 128
    ) || "Unknown";
  }

  async function verifyRequester(request, { allowStale = true } = {}) {
    const token = bearerToken(request);
    if (!token) {
      throw new HttpError(401, "Missing Discord authorization token.");
    }

    const cacheKey = tokenCacheKey(token);
    // A roster request may use a bounded stale session during Discord rate limits.
    // Stream access fails closed and must never share that in-flight fallback.
    const inFlightKey = `${cacheKey}:${allowStale ? "roster" : "streams"}`;
    const cached = requesterCache.get(cacheKey);
    const now = Date.now();
    if (cached && cached.expiresAt > now) {
      return cached.user;
    }

    if (requesterInFlight.has(inFlightKey)) {
      return requesterInFlight.get(inFlightKey);
    }

    for (const [key, expires] of failures) if (expires <= now) failures.delete(key);
    if (failures.has(cacheKey)) throw new HttpError(401, "Discord authorization failed.");
    verifications.take(address(request));
    if (requesterInFlight.size >= 8) throw new HttpError(503, "Login checks are busy. Try again shortly.");
    const verification = verifyRequesterFromDiscord(token, cacheKey, cached, now, allowStale).catch((error) => {
      if (error instanceof HttpError && [401, 403].includes(error.status) && failures.size < 1024) {
        failures.set(cacheKey, Date.now() + 30_000);
      }
      throw error;
    }).finally(() => {
      requesterInFlight.delete(inFlightKey);
    });
    requesterInFlight.set(inFlightKey, verification);
    return verification;
  }

  async function verifyRequesterFromDiscord(token, cacheKey, cached, now, allowStale) {
    const authorization = `Bearer ${token}`;
    let user;
    let member;
    try {
      [user, member] = await Promise.all([
        discordJson("/users/@me", authorization),
        discordJson(`/users/@me/guilds/${guildId}/member`, authorization),
      ]);
    } catch (error) {
      if (allowStale && error instanceof HttpError && error.status === 429 && cached?.staleUntil > now) {
        console.error(`Discord requester verification rate limited; using stale user ${cached.user.id}`);
        return cached.user;
      }
      requesterCache.delete(cacheKey);
      throw error;
    }

    const role = roleFromIds(member?.roles);
    if (!role) {
      requesterCache.delete(cacheKey);
      throw new HttpError(403, "Discord user does not have the required guild role.");
    }

    if (!/^[0-9]{1,20}$/.test(user?.id)) throw new HttpError(502, "Discord returned an invalid user.");
    const verified = {
      id: user.id,
      name: displayName(member, user),
      role,
    };
    pruneRequesterCache(now);
    if (!requesterCache.has(cacheKey) && requesterCache.size >= 4096) {
      requesterCache.delete(requesterCache.keys().next().value);
    }
    requesterCache.set(cacheKey, {
      user: verified,
      expiresAt: now + requesterCacheSeconds * 1000,
      staleUntil: now + requesterStaleSeconds * 1000,
    });
    pruneRequesterCache(now);

    return verified;
  }

  function tokenCacheKey(token) {
    return createHash("sha256").update(token).digest("hex");
  }

  function pruneRequesterCache(now) {
    for (const [cacheKey, cached] of requesterCache) {
      if (cached.staleUntil <= now) {
        requesterCache.delete(cacheKey);
      }
    }
  }

  async function loadHeartbeats() {
    try {
      const raw = await fs.readFile(heartbeatFile, "utf8");
      const parsed = JSON.parse(raw);
      return parsed?.users && typeof parsed.users === "object" ? parsed.users : {};
    } catch (error) {
      if (error.code === "ENOENT") {
        return {};
      }
      throw error;
    }
  }

  async function saveHeartbeats(users) {
    await fs.mkdir(dataDir, { recursive: true });
    const tempFile = `${heartbeatFile}.${randomUUID()}.tmp`;
    let file;
    try {
      file = await fs.open(tempFile, "wx", 0o600);
      await file.writeFile(`${JSON.stringify({ version: 1, users }, null, 2)}\n`);
      await file.sync();
      await file.close();
      file = null;
      await fs.rename(tempFile, heartbeatFile);
    } finally {
      await file?.close();
      await fs.unlink(tempFile).catch((error) => { if (error.code !== "ENOENT") throw error; });
    }
  }

  async function updateHeartbeats(mutator) {
    const run = heartbeatWriteQueue.catch(() => {}).then(async () => {
      const users = await loadHeartbeats();
      const result = mutator(users);
      pruneHeartbeats(users);
      await saveHeartbeats(users);
      return result;
    });

    heartbeatWriteQueue = run.catch(() => {});
    return run;
  }

  function pruneHeartbeats(users) {
    const cutoff = Date.now() - heartbeatRetentionSeconds * 1000;
    for (const [userId, heartbeat] of Object.entries(users)) {
      if (Date.parse(heartbeat.lastSeenAt || "") < cutoff) {
        delete users[userId];
      }
    }
  }

  function optionalText(value, maxLength) {
    if (typeof value !== "string") {
      return null;
    }
    const trimmed = value.trim();
    if (!trimmed || trimmed.length > maxLength) {
      return null;
    }
    return trimmed;
  }

  function pruneOauthHandoffs(now = Date.now()) {
    for (const [state, entry] of oauthHandoffs) {
      if (entry.expiresAt <= now) {
        oauthHandoffs.delete(state);
      }
    }
  }

  function saveOauthHandoff(state, payload) {
    const now = Date.now();
    pruneOauthHandoffs(now);

    if (oauthHandoffs.has(state)) throw new HttpError(409, "This login callback was already received.");
    if (oauthHandoffs.size >= oauthHandoffMaxEntries) {
      throw new HttpError(503, "Login is busy. Try again shortly.");
    }

    oauthHandoffs.set(state, {
      ...payload,
      expiresAt: now + oauthHandoffTtlSeconds * 1000,
    });
  }

  function takeOauthHandoff(state) {
    pruneOauthHandoffs();
    const entry = oauthHandoffs.get(state) || null;
    if (entry) {
      oauthHandoffs.delete(state);
    }
    return entry;
  }

  function handleDiscordCallback(url, response) {
    const state = optionalText(url.searchParams.get("state"), 256);
    if (!state || !/^[a-f0-9]{64}$/.test(state)) {
      sendHtml(
        response,
        400,
        callbackPage("Brick login failed", "Return to Brick and try again."),
      );
      return;
    }

    const error = optionalText(url.searchParams.get("error_description"), 1024)
      || optionalText(url.searchParams.get("error"), 256);
    if (error) {
      saveOauthHandoff(state, { error });
      sendHtml(
        response,
        200,
        callbackPage("Brick login failed", "Return to Brick and try again."),
      );
      return;
    }

    const code = optionalText(url.searchParams.get("code"), 2048);
    if (!code) {
      sendHtml(
        response,
        400,
        callbackPage("Brick login failed", "Return to Brick and try again."),
      );
      return;
    }

    saveOauthHandoff(state, { code });
    sendHtml(response, 200, callbackPage("Brick login complete", "You can return to Brick."));
  }

  function handleAuthCallbackPoll(url, response) {
    const state = optionalText(url.searchParams.get("state"), 256);
    if (!state || !/^[a-f0-9]{64}$/.test(state)) {
      sendJson(response, 400, { error: "Missing OAuth state." });
      return;
    }

    const handoff = takeOauthHandoff(state);
    if (!handoff) {
      sendJson(response, 202, { status: "pending" });
      return;
    }

    if (handoff.error) {
      sendJson(response, 400, { error: `Discord rejected the login: ${handoff.error}` });
      return;
    }

    sendJson(response, 200, { code: handoff.code });
  }

  async function handleHeartbeat(request, response) {
    const requester = await verifyRequester(request);
    const body = await readJsonBody(request);
    const now = new Date().toISOString();

    await updateHeartbeats((users) => {
      users[requester.id] = {
        userId: requester.id,
        displayName: requester.name,
        role: requester.role,
        appVersion: optionalText(body.appVersion, 64),
        platform: optionalText(body.platform, 64),
        lastSeenAt: now,
      };
    });

    sendJson(response, 200, {
      ok: true,
      user: requester,
      onlineWindowSeconds,
    });
  }

  async function fetchRosterMembers({ strict = false } = {}) {
    const fresh = () => rosterCache?.verifiedAt + rosterCacheSeconds * 1000 > Date.now();
    if (rosterCache && rosterCache.expiresAt > Date.now() && (!strict || fresh())) {
      return rosterCache.members;
    }

    if (!rosterInFlight) rosterInFlight = refreshRosterMembers().finally(() => { rosterInFlight = null; });
    const members = await rosterInFlight;
    if (strict && !fresh()) throw new HttpError(503, "Guild access could not be checked. Try again shortly.");
    return members;
  }

  async function refreshRosterMembers() {
    try {
      const members = await fetchRosterMembersFromDiscord();
      rosterCache = {
        expiresAt: Date.now() + rosterCacheSeconds * 1000,
        verifiedAt: Date.now(),
        members,
      };
      return members;
    } catch (error) {
      if (rosterCache?.members?.length) {
        console.error(`Discord roster refresh failed; serving stale roster: ${error.message}`);
        rosterCache.expiresAt = Date.now() + 5_000;
        return rosterCache.members;
      }
      throw error;
    }
  }

  async function fetchRosterMembersFromDiscord() {
    const members = [];
    let after = "0";

    for (let pageNumber = 0; pageNumber < 250; pageNumber += 1) {
      const params = new URLSearchParams({ limit: "1000", after });
      const page = await discordJson(`/guilds/${guildId}/members?${params}`, `Bot ${botToken}`);
      if (!Array.isArray(page)) {
        throw new HttpError(502, "Discord roster response was not a member list.");
      }

      if (members.length + page.length > 10_000) throw new HttpError(502, "Discord roster is too large.");
      members.push(...page);
      if (page.length < 1000) {
        break;
      }

      const lastUserId = page.at(-1)?.user?.id;
      if (!lastUserId || lastUserId === after) {
        break;
      }
      after = lastUserId;
    }

    return members;
  }

  function rosterRow(member, heartbeat, now) {
    const role = roleFromIds(member.roles);
    if (!role || member.user?.bot) {
      return null;
    }

    const lastSeenMs = Date.parse(heartbeat?.lastSeenAt || "");
    const online = Number.isFinite(lastSeenMs) && now - lastSeenMs <= onlineWindowSeconds * 1000;

    return {
      userId: member.user?.id || "",
      name: displayName(member, member.user),
      role,
      online,
      lastSeenAt: heartbeat?.lastSeenAt || null,
      appVersion: heartbeat?.appVersion || null,
      platform: heartbeat?.platform || null,
    };
  }

  function sortRosterRows(left, right) {
    if (left.online !== right.online) {
      return left.online ? -1 : 1;
    }
    return left.name.localeCompare(right.name, "en", { sensitivity: "base" });
  }

  async function handleRoster(request, response) {
    await verifyRequester(request);

    const [members, heartbeats] = await Promise.all([fetchRosterMembers(), loadHeartbeats()]);
    const now = Date.now();
    const officers = [];
    const raiders = [];

    for (const member of members) {
      const userId = member.user?.id;
      const row = rosterRow(member, userId ? heartbeats[userId] : null, now);
      if (!row) {
        continue;
      }

      if (row.role === "Officer") {
        officers.push(row);
      } else {
        raiders.push(row);
      }
    }

    officers.sort(sortRosterRows);
    raiders.sort(sortRosterRows);

    sendJson(response, 200, {
      generatedAt: new Date(now).toISOString(),
      onlineWindowSeconds,
      officers,
      raiders,
    });
  }

  async function handleStreams(request, response, url) {
    // Authorization always precedes registry access, parsing bodies, and provider requests.
    const requester = await verifyRequester(request, { allowStale: false });
    const members = await streamMembers();
    // Apply current guild eligibility to reads and writes alike, even while the
    // OAuth identity token remains in its short verification cache.
    if (!members.some(member => member.userId === requester.id)) throw new HttpError(403, "Discord user does not have the required guild role.");
    if (url.pathname === "/v1/streams/me" && ["PUT", "DELETE"].includes(request.method)) {
      streamChanges.take(requester.id);
      const body = await readJsonBody(request);
      sendJson(response, 200, request.method === "PUT"
        ? await streams.save(requester, body.url, { checkTimeoutMs: Math.max(0, requestDeadlines.get(request) - performance.now() - 500) })
        : await streams.remove(requester, body.provider));
      return;
    }
    const player = url.pathname.match(/^\/v1\/streams\/player\/([0-9]{1,20})(?:\/(twitch|youtube))?$/);
    const recording = url.pathname.match(/^\/v1\/streams\/vods(?:\/([0-9]{1,20}))?$/);
    if (request.method !== "GET" || (url.pathname !== "/v1/streams" && !player && !recording)) {
      throw new HttpError(404, "Not found.");
    }
    if (recording) {
      const eligible = new Map(members.map(member => [member.userId, member.name]));
      if (recording[1] && !eligible.has(recording[1])) throw new HttpError(404, "This member is unavailable.");
      const vods = (await streams.recordings()).filter(vod => eligible.has(vod.userId) && (!recording[1] || recording[1] === vod.userId))
        .map(vod => ({ ...vod, name: eligible.get(vod.userId) }));
      sendJson(response, 200, { vods });
      return;
    }
    const snapshot = await streams.snapshot(requester, members);
    if (player) {
      const stream = snapshot.streams.find(entry => entry.userId === player[1] && (!player[2] || entry.provider === player[2]));
      if (!stream) throw new HttpError(404, "This stream is no longer live.");
      sendHtml(response, 200, streamPlayerPage(stream, streams.playerOrigin), true);
    } else sendJson(response, 200, snapshot);
  }

  async function streamMembers() {
    return (await fetchRosterMembers({ strict: true }))
      .filter(member => roleFromIds(member.roles) && !member.user?.bot && /^[0-9]{1,20}$/.test(member.user?.id))
      .map(member => ({ userId: member.user.id, name: displayName(member, member.user) }));
  }

  async function handleRequest(request, response) {
    requests.take(address(request));
    const url = requestUrl(request);

    if (request.method === "GET" && url.pathname === "/discord/callback") {
      callbacks.take(address(request));
      handleDiscordCallback(url, response);
      return;
    }

    if (request.method === "GET" && url.pathname === "/health") {
      sendJson(response, 200, { ok: true, service: "brick-presence", time: new Date().toISOString() });
      return;
    }

    if (request.method === "GET" && url.pathname === "/v1/auth/callback") {
      handleAuthCallbackPoll(url, response);
      return;
    }

    if (request.method === "POST" && url.pathname === "/v1/heartbeat") {
      await handleHeartbeat(request, response);
      return;
    }

    if (request.method === "GET" && url.pathname === "/v1/roster") {
      await handleRoster(request, response);
      return;
    }

    if (url.pathname === "/v1/streams" || url.pathname.startsWith("/v1/streams/")) {
      await handleStreams(request, response, url);
      return;
    }

    sendJson(response, 404, { error: "Not found." });
  }

  let activeRequests = 0;
  const server = http.createServer({ maxHeaderSize: 8192, headersTimeout: 10_000, requestTimeout: 15_000 }, (request, response) => {
    if (activeRequests >= 100) {
      response.setHeader("connection", "close");
      if (request.url?.startsWith("/v1/streams/player/")) {
        sendHtml(response, 503, callbackPage("Stream unavailable", "Return to Brick to sign in or choose a live stream."));
        return;
      }
      sendJson(response, 503, { error: "Service is busy. Try again shortly." });
      return;
    }
    activeRequests++;
    requestDeadlines.set(request, performance.now() + 15_000);
    const deadline = setTimeout(() => request.destroy(), 15_000);
    handleRequest(request, response).catch((error) => {
      if (response.destroyed || response.headersSent) return;
      response.setHeader("connection", "close");
      if (request.url?.startsWith("/v1/streams/player/")) {
        const status = error instanceof HttpError ? error.status : 500;
        sendHtml(response, status, callbackPage("Stream unavailable", "Return to Brick to sign in or choose a live stream."));
        return;
      }
      if (error instanceof HttpError) {
        sendJson(response, error.status, { error: error.message });
        return;
      }
      console.error("Presence request failed", error.code || error.name || "unknown error");
      sendJson(response, 500, { error: "Internal server error." });
    }).finally(() => {
      clearTimeout(deadline);
      requestDeadlines.delete(request);
      activeRequests--;
    });
  });
  server.maxConnections = 256;
  server.maxRequestsPerSocket = 100;
  server.keepAliveTimeout = 5_000;
  server.setTimeout(15_000, (socket) => socket.destroy());
  let streamTimer;
  let streamPollingClosed = false;
  const pollStreams = async () => {
    let delayMs = 30_000;
    try {
      if (await streams.needsPolling()) {
        await streams.poll(await streamMembers());
        delayMs = streams.nextPollDelayMs();
      }
    } catch {
      // Never log stream records, submitted URLs, or upstream request credentials.
      console.error("Background stream status refresh failed");
    } finally {
      if (!streamPollingClosed) {
        streamTimer = setTimeout(pollStreams, delayMs);
        streamTimer.unref();
      }
    }
  };
  server.on("close", () => {
    streamPollingClosed = true;
    clearTimeout(streamTimer);
    streams.close();
  });
  server.on("listening", () => {
    if (gatewayEnabled) startBotGatewayPresence();
    if (env.STREAM_BACKGROUND_ENABLED !== "false") {
      streamTimer = setTimeout(pollStreams, 0);
      streamTimer.unref();
    }
  });
  return server;

  function startBotGatewayPresence() {
    let reconnectDelayMs = GATEWAY_RECONNECT_MIN_MS;
    let reconnectTimer = null;

    const scheduleReconnect = () => {
      if (reconnectTimer) {
        return;
      }
      const delay = reconnectDelayMs;
      reconnectDelayMs = Math.min(reconnectDelayMs * 2, GATEWAY_RECONNECT_MAX_MS);
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        connectGateway();
      }, delay);
    };

    const connectGateway = async () => {
      let gatewayUrl;
      try {
        const gateway = await discordJson("/gateway/bot", `Bot ${botToken}`);
        gatewayUrl = gateway?.url;
        if (!gatewayUrl) {
          throw new Error("Discord did not return a Gateway URL.");
        }
      } catch (error) {
        console.error(`Discord Gateway lookup failed: ${error.message}`);
        scheduleReconnect();
        return;
      }

      let socket;
      let heartbeatTimer = null;
      let lastSequence = null;
      let lastHeartbeatAcked = true;

      const cleanup = () => {
        if (heartbeatTimer) {
          clearInterval(heartbeatTimer);
          heartbeatTimer = null;
        }
      };

      const reconnect = () => {
        cleanup();
        scheduleReconnect();
      };

      const sendPayload = (payload) => {
        if (socket?.readyState === WebSocket.OPEN) {
          socket.send(JSON.stringify(payload));
        }
      };

      const sendHeartbeat = () => {
        if (!lastHeartbeatAcked) {
          try {
            socket.close(4000, "missed heartbeat ack");
          } catch {
            reconnect();
          }
          return;
        }

        lastHeartbeatAcked = false;
        sendPayload({ op: 1, d: lastSequence });
      };

      try {
        socket = new WebSocket(`${gatewayUrl}/?v=10&encoding=json`);
      } catch (error) {
        console.error(`Discord Gateway connection failed: ${error.message}`);
        scheduleReconnect();
        return;
      }

      socket.addEventListener("open", () => {
        reconnectDelayMs = GATEWAY_RECONNECT_MIN_MS;
      });

      socket.addEventListener("message", (event) => {
        let payload;
        try {
          payload = JSON.parse(event.data.toString());
        } catch {
          return;
        }

        if (typeof payload.s === "number") {
          lastSequence = payload.s;
        }

        if (payload.op === 10) {
          const heartbeatInterval = Number(payload.d?.heartbeat_interval);
          if (Number.isFinite(heartbeatInterval) && heartbeatInterval > 0) {
            setTimeout(sendHeartbeat, Math.floor(Math.random() * heartbeatInterval));
            heartbeatTimer = setInterval(sendHeartbeat, heartbeatInterval);
          }
          sendPayload({
            op: 2,
            d: {
              token: botToken,
              intents: GATEWAY_INTENTS,
              properties: {
                os: process.platform,
                browser: "brick-presence",
                device: "brick-presence",
              },
              presence: {
                since: null,
                activities: [],
                status: "online",
                afk: false,
              },
            },
          });
        } else if (payload.op === 11) {
          lastHeartbeatAcked = true;
        } else if (payload.op === 1) {
          sendPayload({ op: 1, d: lastSequence });
        } else if (payload.op === 7 || payload.op === 9) {
          socket.close(4000, "discord requested reconnect");
        } else if (payload.op === 0 && payload.t === "READY") {
          console.log(`Discord Gateway ready as ${payload.d?.user?.username || "bot"}`);
        }
      });

      socket.addEventListener("close", reconnect);
      socket.addEventListener("error", (event) => {
        console.error(`Discord Gateway socket error: ${event.message || "unknown error"}`);
      });
    };

    connectGateway();
  }

}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const server = await createPresenceServer();
  const port = Number(process.env.PORT || 8080);
  server.listen(port, () => console.log(`Brick presence API listening on ${port}`));
}
