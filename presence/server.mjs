import http from "node:http";
import path from "node:path";
import { promises as fs } from "node:fs";
import { createHash } from "node:crypto";

const DISCORD_API = "https://discord.com/api/v10";
const MAX_BODY_BYTES = 32 * 1024;
const GATEWAY_INTENTS = 1;
const GATEWAY_RECONNECT_MIN_MS = 5_000;
const GATEWAY_RECONNECT_MAX_MS = 60_000;

const port = parsePositiveInt(process.env.PORT, 8080);
const dataDir = process.env.DATA_DIR || "/data";
const heartbeatFile = path.join(dataDir, "heartbeats.json");

const guildId = requireEnv("DISCORD_GUILD_ID");
const botToken = requireEnv("DISCORD_BOT_TOKEN");
const officerRoleId = requireEnv("DISCORD_OFFICER_ROLE_ID");
const raiderRoleId = requireEnv("DISCORD_RAIDER_ROLE_ID");
const onlineWindowSeconds = parsePositiveInt(process.env.ONLINE_WINDOW_SECONDS, 180);
const rosterCacheSeconds = parsePositiveInt(process.env.ROSTER_CACHE_SECONDS, 60);
const heartbeatRetentionSeconds = parsePositiveInt(
  process.env.HEARTBEAT_RETENTION_SECONDS,
  14 * 24 * 60 * 60,
);
const requesterCacheSeconds = parsePositiveInt(process.env.REQUESTER_CACHE_SECONDS, 5 * 60);
const requesterStaleSeconds = parsePositiveInt(process.env.REQUESTER_STALE_SECONDS, 60 * 60);
const gatewayEnabled = process.env.DISCORD_GATEWAY_ENABLED?.trim().toLowerCase() !== "false";

let rosterCache = null;
const requesterCache = new Map();
const requesterInFlight = new Map();
let heartbeatWriteQueue = Promise.resolve();

function requireEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) {
    throw new Error(`${name} is required`);
  }
  return value;
}

function parsePositiveInt(value, fallback) {
  const number = Number.parseInt(value ?? "", 10);
  return Number.isFinite(number) && number > 0 ? number : fallback;
}

class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

function sendJson(response, status, payload) {
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
  });
  response.end(JSON.stringify(payload));
}

function bearerToken(request) {
  const auth = request.headers.authorization || "";
  const match = auth.match(/^Bearer\s+(.+)$/i);
  return match?.[1]?.trim() || null;
}

function requestUrl(request) {
  return new URL(request.url || "/", `http://${request.headers.host || "localhost"}`);
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
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw new HttpError(400, "Request body must be valid JSON.");
  }
}

async function discordJson(pathname, authorization) {
  const response = await fetch(`${DISCORD_API}${pathname}`, {
    headers: {
      authorization,
      "user-agent": "BrickPresence/1.0",
    },
    signal: AbortSignal.timeout(10_000),
  });

  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = { message: text };
    }
  }

  if (!response.ok) {
    const message = body?.message || `Discord returned HTTP ${response.status}`;
    const retryAfter = Number(body?.retry_after);
    const detail =
      response.status === 429 && Number.isFinite(retryAfter)
        ? `${message} Retry after ${retryAfter} seconds.`
        : message;
    const status =
      response.status === 401 || response.status === 403 || response.status === 429
        ? response.status
        : 502;
    throw new HttpError(status, `Discord request failed: ${detail}`);
  }

  return body;
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
  return (
    member?.nick ||
    member?.user?.global_name ||
    member?.user?.username ||
    user?.global_name ||
    user?.username ||
    "Unknown"
  );
}

async function verifyRequester(request) {
  const token = bearerToken(request);
  if (!token) {
    throw new HttpError(401, "Missing Discord authorization token.");
  }

  const cacheKey = tokenCacheKey(token);
  const cached = requesterCache.get(cacheKey);
  const now = Date.now();
  if (cached && cached.expiresAt > now) {
    return cached.user;
  }

  if (requesterInFlight.has(cacheKey)) {
    return requesterInFlight.get(cacheKey);
  }

  const verification = verifyRequesterFromDiscord(token, cacheKey, cached, now).finally(() => {
    requesterInFlight.delete(cacheKey);
  });
  requesterInFlight.set(cacheKey, verification);
  return verification;
}

async function verifyRequesterFromDiscord(token, cacheKey, cached, now) {
  const authorization = `Bearer ${token}`;
  let user;
  let member;
  try {
    [user, member] = await Promise.all([
      discordJson("/users/@me", authorization),
      discordJson(`/users/@me/guilds/${guildId}/member`, authorization),
    ]);
  } catch (error) {
    if (error instanceof HttpError && error.status === 429 && cached?.staleUntil > now) {
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

  const verified = {
    id: user.id,
    name: displayName(member, user),
    role,
  };
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
  const tempFile = `${heartbeatFile}.${process.pid}.${Date.now()}.tmp`;
  await fs.writeFile(tempFile, `${JSON.stringify({ version: 1, users }, null, 2)}\n`, {
    mode: 0o600,
  });
  await fs.rename(tempFile, heartbeatFile);
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

async function fetchRosterMembers() {
  if (rosterCache && rosterCache.expiresAt > Date.now()) {
    return rosterCache.members;
  }

  try {
    const members = await fetchRosterMembersFromDiscord();
    rosterCache = {
      expiresAt: Date.now() + rosterCacheSeconds * 1000,
      members,
    };
    return members;
  } catch (error) {
    if (rosterCache?.members?.length) {
      console.error(`Discord roster refresh failed; serving stale roster: ${error.message}`);
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

async function handleRequest(request, response) {
  const url = requestUrl(request);

  if (request.method === "GET" && url.pathname === "/health") {
    sendJson(response, 200, { ok: true, service: "brick-presence", time: new Date().toISOString() });
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

  sendJson(response, 404, { error: "Not found." });
}

const server = http.createServer((request, response) => {
  handleRequest(request, response).catch((error) => {
    if (error instanceof HttpError) {
      sendJson(response, error.status, { error: error.message });
      return;
    }

    console.error(error);
    sendJson(response, 500, { error: "Internal server error." });
  });
});

server.listen(port, () => {
  console.log(`Brick presence API listening on ${port}`);
  if (gatewayEnabled) {
    startBotGatewayPresence();
  }
});

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
