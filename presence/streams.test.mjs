import assert from "node:assert/strict";
import { test } from "node:test";
import { once } from "node:events";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createPresenceServer } from "./server.mjs";
import { createStreamService, parsePlayerOrigin, parseStreamUrl } from "./streams.mjs";

const json = (body, status = 200) => new Response(JSON.stringify(body), { status });
const providerEnv = { TWITCH_CLIENT_ID: "test-client", TWITCH_CLIENT_SECRET: "test-secret", YOUTUBE_API_KEY: "test-key" };
const videoId = "abcDEF_12-3";
const members = [
  { user: { id: "11", username: "Alice" }, roles: ["raider"] },
  { user: { id: "22", username: "Bob" }, roles: ["officer"] },
  { user: { id: "33", username: "Removed" }, roles: [] },
];

async function fixture(t, upstream = () => { throw new Error("Unexpected provider request"); }, extra = {}, timing = {}) {
  const data = await mkdtemp(path.join(os.tmpdir(), "brick-stream-test-"));
  let clock = 1_700_000_000_000;
  let calls = 0;
  let userChecks = 0;
  let currentMembers = members;
  let discordFailure = null;
  const env = {
    DATA_DIR: data, DISCORD_BOT_TOKEN: "test-bot", DISCORD_GUILD_ID: "guild",
    DISCORD_OFFICER_ROLE_ID: "officer", DISCORD_RAIDER_ROLE_ID: "raider",
    DISCORD_GATEWAY_ENABLED: "false", STREAM_BACKGROUND_ENABLED: "false", BRICK_PLAYER_ORIGIN: "https://brick.example.com", ...extra,
  };
  const fetchUpstream = (url, options) => {
    calls++;
    assert.equal(options.redirect, "error");
    if (!url.startsWith("https://discord.com/")) return upstream(url, options);
    if (url.endsWith("/users/@me") || url.endsWith("/member")) userChecks++;
    if (discordFailure) return json({}, discordFailure);
    const token = options.headers.authorization?.split(" ")[1];
    if (token === "invalid") return json({}, 401);
    const id = token === "bob-token" ? "22" : token === "special-token" ? "341518802208423957" : token === "removed-token" ? "33" : "11";
    const member = currentMembers.find(member => member.user.id === id);
    if (url.endsWith("/users/@me")) return json(member?.user || { id, username: "Removed" });
    if (url.endsWith("/member")) return member ? json(member) : json({}, 403);
    if (url.includes("/members?")) return json(currentMembers);
    throw new Error("Unexpected Discord request");
  };
  let server;
  let base;
  const close = async () => {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  };
  const start = async () => {
    // Most existing cases exercise the scheduled refresh in isolation. Initial
    // submission checks have separate integration coverage with the real default.
    server = await createPresenceServer({ env, fetch: fetchUpstream, now: () => clock, firstStreamCheckWaitMs: 0, ...timing });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    base = `http://127.0.0.1:${server.address().port}`;
  };
  await start();
  t.after(async () => { await close(); await rm(data, { recursive: true, force: true }); });
  const request = (route = "/v1/streams", options = {}) => fetch(base + route, options);
  const authorized = (route = "/v1/streams", options = {}, token = "alice-token") => request(route, {
    ...options, headers: { authorization: `Bearer ${token}`, ...options.headers },
  });
  return {
    data, request, authorized, calls: () => calls, userChecks: () => userChecks, advance: milliseconds => { clock += milliseconds; },
    setMembers: value => { currentMembers = value; }, failDiscord: status => { discordFailure = status; },
    restart: async () => { await close(); await start(); },
    save: (url, token) => authorized("/v1/streams/me", { method: "PUT", body: JSON.stringify({ url }) }, token),
    list: async token => (await authorized("/v1/streams", {}, token)).json(),
  };
}

function twitchLive(url, options) {
  if (url === "https://id.twitch.tv/oauth2/token") {
    assert.equal(options.body.get("client_secret"), "test-secret");
    return json({ access_token: "test-app-token", expires_in: 3600 });
  }
  const query = new URL(url).searchParams;
  assert.equal(query.get("first"), "100");
  assert.equal(options.headers.authorization, "Bearer test-app-token");
  return json({ data: query.getAll("user_login").map(user_login => ({ user_login, type: "live", title: "Raid night", viewer_count: 12 })) });
}

async function quotaFixture(t, count, upstream) {
  const dataDir = await mkdtemp(path.join(os.tmpdir(), "brick-stream-quota-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  const users = {};
  const eligible = [];
  for (let index = 1; index <= count; index++) {
    const userId = String(index);
    users[userId] = { youtube: { url: `https://youtu.be/${userId.padStart(11, "0")}` } };
    eligible.push({ userId, name: `Member ${index}` });
  }
  await writeFile(path.join(dataDir, "streams.json"), JSON.stringify({ version: 2, users }));
  let clock = 1_700_000_000_000;
  let requests = 0;
  const service = await createStreamService({ dataDir, env: providerEnv, now: () => clock, firstCheckWaitMs: 0,
    fetch: async (url, options) => {
      requests++;
      return upstream(new URL(url), options);
    } });
  t.after(() => service.close());
  return {
    advance: milliseconds => { clock += milliseconds; }, requests: () => requests,
    snapshot: () => service.snapshot({ id: "1", name: "Member 1" }, eligible),
    save: url => service.save({ id: "1", name: "Member 1" }, url),
  };
}

async function manualFixture(t, upstream, users = {}) {
  const dataDir = await mkdtemp(path.join(os.tmpdir(), "brick-stream-manual-"));
  await writeFile(path.join(dataDir, "streams.json"), JSON.stringify({ version: 2, users }));
  let clock = 1_700_000_000_000;
  const service = await createStreamService({ dataDir, env: providerEnv, now: () => clock, fetch: upstream });
  t.after(async () => { service.close(); await rm(dataDir, { recursive: true, force: true }); });
  const eligible = Array.from({ length: 1000 }, (_, index) => ({ userId: String(index + 1), name: `Member ${index + 1}` }));
  return { dataDir, service, advance: milliseconds => { clock += milliseconds; },
    save: (url, id = "1", options) => service.save({ id, name: `Member ${id}` }, url, options),
    snapshot: () => service.snapshot({ id: "1", name: "Member 1" }, eligible) };
}

const liveYoutubeBatch = url => json({ items: url.searchParams.get("id").split(",").map(id => ({
  id, snippet: { title: "Live raid", liveBroadcastContent: "live" }, status: { embeddable: true },
  liveStreamingDetails: {},
})) });

test("private-log setup and replay reads require current Discord eligibility; PKCE codes stay isolated", async t => {
  const f = await fixture(t, twitchLive, { WARCRAFTLOGS_CLIENT_ID: "public-pkce-id", ROSTER_CACHE_SECONDS: "1" });
  assert.equal((await f.request("/v1/streams/review/config")).status, 401);
  assert.deepEqual(await (await f.authorized("/v1/streams/review/config")).json(), { clientId: "public-pkce-id", guildId: 580482, userId: "11" });
  const state = "a".repeat(64);
  assert.equal((await f.request(`/warcraftlogs/callback?state=${state}&code=private-code`)).status, 200);
  assert.equal((await f.request(`/v1/streams/review/callback?state=${state}`)).status, 401);
  assert.equal((await f.request(`/v1/auth/callback?state=${state}`)).status, 202, "Discord cannot consume a WCL callback");
  assert.deepEqual(await (await f.authorized(`/v1/streams/review/callback?state=${state}`)).json(), { code: "private-code" });
  assert.equal((await f.authorized(`/v1/streams/review/callback?state=${state}`)).status, 202);
  f.setMembers(members.filter(m => m.user.id !== "11"));
  await new Promise(resolve => setTimeout(resolve, 1050));
  assert.equal((await f.authorized("/v1/streams/review/config")).status, 403);
});

test("replay endpoints and embedded seeking cannot cross broadcast identities or seek beyond availability", async t => {
  const startedAt = new Date(1_700_000_000_000 - 6 * 3600_000).toISOString();
  const f = await fixture(t, (url, options) => {
    if (url === "https://id.twitch.tv/oauth2/token") return json({ access_token: "app-token", expires_in: 3600 });
    if (url.includes("/helix/streams?")) return json({ data: [{ user_login: "alice", type: "live", id: "123", user_id: "42", started_at: startedAt }] });
    if (url.includes("/helix/videos?")) return json({ data: [{ id: "456", stream_id: "123", user_id: "42", type: "archive", duration: "5h59m" }] });
    throw new Error("Unexpected provider request");
  }, providerEnv);
  await f.save("https://twitch.tv/alice");
  const replay = await (await f.authorized("/v1/streams/review/11/twitch")).json();
  assert.equal(replay.broadcastId, "123"); assert.equal(replay.videoId, "456"); assert.equal(Date.parse(replay.startedAt), Date.parse(startedAt));
  assert.equal((await f.request("/v1/streams/review/11/twitch")).status, 401);
  assert.equal((await f.authorized("/v1/streams/review/33/twitch")).status, 404);
  const valid = await f.authorized("/v1/streams/player/11/twitch?at=19800&broadcast=123");
  assert.equal(valid.status, 200); assert.match(await valid.text(), /video=v456.*time=19800s/);
  const paused = await f.authorized("/v1/streams/player/11/twitch?at=19800.875&broadcast=123&paused=1");
  assert.equal(paused.status, 200);
  const html = await paused.text();
  assert.match(html, /autoplay=false/); assert.match(html, /time=19800.875s/);
  for (const query of ["at=19800&broadcast=122", "at=21600&broadcast=123", "at=-1&broadcast=123", "at=2&at=3&broadcast=123", "at=2&broadcast=123&broadcast=123", "at=2&broadcast=123&extra=x", "broadcast=123", "paused=1", "at=1&paused=1", "at=1&broadcast=123&paused=0", "at=1&broadcast=123&paused=true", "at=1&broadcast=123&paused=1&paused=1", "at=1.0001&broadcast=123", "at=1.&broadcast=123", "at=.5&broadcast=123", "at=1e2&broadcast=123", "at=NaN&broadcast=123", "at=604800.001&broadcast=123"]) {
    assert.ok((await f.authorized(`/v1/streams/player/11/twitch?${query}`)).status >= 400, query);
  }
});

test("large YouTube directories reduce quota use while verified entries never outlive the freshness limit", async t => {
  const small = await quotaFixture(t, 200, liveYoutubeBatch);
  assert.equal((await small.snapshot()).streams.length, 200);
  assert.equal(small.requests(), 4);
  small.advance(30_000);
  assert.equal((await small.snapshot()).streams.length, 200);
  assert.equal(small.requests(), 4, "fresh provider cache avoids an unnecessary batch round");
  small.advance(30_000);
  assert.equal((await small.snapshot()).streams.length, 200);
  assert.equal(small.requests(), 8);

  const large = await quotaFixture(t, 1000, liveYoutubeBatch);
  assert.equal((await large.snapshot()).streams.length, 1000);
  assert.equal(large.requests(), 20);
  large.advance(90_000);
  const expired = await large.snapshot();
  assert.equal(expired.streams.length, 0);
  assert.equal(expired.ownStreams[0].status, "unknown");
  assert.equal(large.requests(), 20, "quota cadence is not bypassed by an expired display cache");
  large.advance(150_000);
  assert.equal((await large.snapshot()).streams.length, 1000);
  assert.equal(large.requests(), 40);
});

test("YouTube quota and rate limits withdraw stale playback and back off without false offline results", async t => {
  let failure = null;
  const f = await quotaFixture(t, 1, url => failure
    ? new Response("{}", { status: failure, headers: { "retry-after": "120" } }) : liveYoutubeBatch(url));
  assert.equal((await f.snapshot()).streams.length, 1);
  failure = 403;
  f.advance(30_000);
  assert.equal((await f.snapshot()).ownStreams[0].status, "unknown");
  assert.equal(f.requests(), 2);
  failure = null;
  for (let index = 0; index < 29; index++) {
    f.advance(30_000);
    assert.equal((await f.snapshot()).streams.length, 0);
  }
  assert.equal(f.requests(), 2);
  f.advance(30_000);
  assert.equal((await f.snapshot()).streams.length, 1);
  failure = 429;
  f.advance(30_000);
  assert.equal((await f.snapshot()).ownStreams[0].status, "unknown");
  const limited = f.requests();
  failure = null;
  f.advance(90_000);
  assert.equal((await f.snapshot()).ownStreams[0].status, "unknown");
  assert.equal(f.requests(), limited);
  f.advance(30_000);
  assert.equal((await f.snapshot()).streams.length, 1);
});

test("new YouTube links wait for the shared quota cadence without becoming failed checks", async t => {
  const f = await quotaFixture(t, 200, liveYoutubeBatch);
  assert.equal((await f.snapshot()).streams.length, 200);
  assert.equal((await f.save(`https://youtu.be/${videoId}`)).ownStream.status, "checking");
  f.advance(30_000);
  const waiting = await f.snapshot();
  assert.equal(waiting.ownStream.status, "checking");
  assert.equal(waiting.streams.length, 199);
  assert.equal(waiting.unverifiedCount, 0);
  assert.equal(f.requests(), 4, "saving cannot bypass the shared quota cadence");
  f.advance(30_000);
  assert.equal((await f.snapshot()).ownStream.status, "live");
  assert.equal(f.requests(), 9, "the replaced live broadcast remains a recording target");
});

test("new YouTube links report an existing provider backoff instead of checking indefinitely", async t => {
  let failure = false;
  const f = await quotaFixture(t, 1, url => failure ? json({}, 403) : liveYoutubeBatch(url));
  await f.snapshot();
  failure = true;
  f.advance(30_000);
  assert.equal((await f.snapshot()).ownStream.status, "unknown");
  assert.equal((await f.save(`https://youtu.be/${videoId}`)).ownStream.status, "unknown");
  f.advance(30_000);
  assert.equal((await f.snapshot()).ownStream.status, "unknown");
  assert.equal(f.requests(), 2, "a replacement link cannot bypass provider backoff");
});

test("stream URLs are canonical provider IDs and reject arbitrary hosts, credentials, paths and schemes", () => {
  assert.deepEqual(parseStreamUrl(" https://m.twitch.tv/Some_Channel/?ref=share "), {
    provider: "twitch", channelId: "some_channel", url: "https://www.twitch.tv/some_channel",
  });
  for (const url of [`https://youtu.be/${videoId}?si=tracking`, `https://youtube.com/live/${videoId}`, `https://www.youtube.com/watch?v=${videoId}&t=2`]) {
    assert.deepEqual(parseStreamUrl(url), { provider: "youtube", channelId: videoId, url: `https://www.youtube.com/watch?v=${videoId}` });
  }
  for (const url of [null, "channel", "http://twitch.tv/user", "https://twitch.tv.evil.test/user", "https://twitch.tv@evil.test/user", "https://user:pass@twitch.tv/user", "https://twitch.tv:444/user", "https://127.0.0.1/live", "https://twitch.tv/videos/123", "https://twitch.tv/directory", "https://twitch.tv/a\\b", "https://youtube.com/@channel/live", `https://youtube.com/watch?v=${videoId}&v=${videoId}`, "https://youtube.com/watch?v=short", "file:///etc/passwd", `https://youtube.com/embed/${videoId}`, "https://twitch.tv/a%2fb"]) {
    assert.throws(() => parseStreamUrl(url), { status: 400 }, String(url));
  }
});

test("all stream endpoints reject anonymous, invalid and forbidden users before registry or provider access", async t => {
  const f = await fixture(t, twitchLive, providerEnv);
  for (const [route, method] of [["/v1/streams", "GET"], ["/v1/streams/me", "PUT"], ["/v1/streams/me", "DELETE"], ["/v1/streams/player/11", "GET"]]) {
    const response = await f.request(route, { method, ...(method === "PUT" ? { body: "invalid JSON" } : {}) });
    assert.equal(response.status, 401);
    assert.equal(response.headers.get("cache-control"), "no-store");
    const body = await response.text();
    assert(!body.includes("twitch.tv"));
    assert(!body.includes("Alice"));
  }
  assert.equal(f.calls(), 0);
  await assert.rejects(stat(path.join(f.data, "streams.json")), { code: "ENOENT" });
  assert.equal((await f.authorized("/v1/streams", {}, "invalid")).status, 401);
  assert.equal((await f.save("https://twitch.tv/alice", "removed-token")).status, 403);
  assert.equal(f.calls(), 4);
  await assert.rejects(stat(path.join(f.data, "streams.json")), { code: "ENOENT" });
});

test("members can only update their own stream; registrations survive restart with restrictive permissions and no secrets", async t => {
  const f = await fixture(t);
  assert.equal((await f.save("https://twitch.tv/alice")).status, 200);
  assert.equal((await f.authorized("/v1/streams/me", { method: "PUT", body: JSON.stringify({ userId: "11", url: `https://youtu.be/${videoId}` }) }, "bob-token")).status, 200);
  let list = await f.list();
  assert.deepEqual(list.providers, { twitch: false, youtube: false });
  assert.equal(list.ownStream.channelId, "alice");
  assert.equal(list.ownStream.status, "unknown");
  assert.deepEqual(list.streams, []);
  assert.equal((await f.list("bob-token")).ownStream.channelId, videoId);
  const saved = await readFile(path.join(f.data, "streams.json"), "utf8");
  if (process.platform !== "win32") assert.equal((await stat(path.join(f.data, "streams.json"))).mode & 0o777, 0o600);
  for (const secret of ["alice-token", "bob-token", "test-bot"]) assert(!saved.includes(secret));
  await f.restart();
  assert.equal((await f.list()).ownStream.channelId, "alice");
  assert.equal((await f.authorized("/v1/streams/me", { method: "DELETE" }, "bob-token")).status, 200);
  assert.equal((await f.list("bob-token")).ownStream, null);
  assert.equal((await f.list()).ownStream.channelId, "alice");
  assert.equal((await f.authorized("/v1/streams/11", { method: "DELETE" }, "bob-token")).status, 404);
  const anonymous = await f.request();
  assert.equal(anonymous.status, 401);
  assert(!(await anonymous.text()).includes("alice"));
});

test("concurrent member submissions persist without losing either registration", async t => {
  const f = await fixture(t);
  const responses = await Promise.all([
    f.save("https://twitch.tv/alice"), f.save(`https://youtu.be/${videoId}`, "bob-token"),
  ]);
  assert(responses.every(response => response.status === 200));
  await f.restart();
  assert.equal((await f.list()).ownStream.channelId, "alice");
  assert.equal((await f.list("bob-token")).ownStream.channelId, videoId);
});

test("one player can keep both platforms, replace either independently and select either authenticated player", async t => {
  const f = await fixture(t, (url, options) => url.includes("googleapis")
    ? json({ items: [{ id: videoId, snippet: { title: "Dual stream", liveBroadcastContent: "live" }, status: { embeddable: true }, liveStreamingDetails: {} }] })
    : twitchLive(url, options), providerEnv);
  await f.save("https://twitch.tv/alice");
  await f.save(`https://youtu.be/${videoId}`);
  let snapshot = await f.list();
  assert.equal(snapshot.ownStreams.length, 2);
  assert.equal(snapshot.streams.length, 2);
  assert(snapshot.streams.every(stream => stream.userId === "11"));
  for (const provider of ["twitch", "youtube"]) {
    const response = await f.authorized(`/v1/streams/player/11/${provider}`);
    assert.equal(response.status, 200);
    assert((await response.text()).includes(provider === "twitch" ? "player.twitch.tv" : "www.youtube.com/embed"));
  }
  await f.save("https://twitch.tv/alice_new");
  snapshot = await f.list();
  assert.equal(snapshot.ownStreams.find(stream => stream.provider === "twitch").channelId, "alice_new");
  assert.equal(snapshot.ownStreams.find(stream => stream.provider === "youtube").channelId, videoId);
  await f.restart();
  assert.equal((await f.list()).ownStreams.length, 2);
  assert.equal((await f.authorized("/v1/streams/me", { method: "DELETE", body: JSON.stringify({ provider: "youtube" }) })).status, 200);
  snapshot = await f.list();
  assert.deepEqual(snapshot.ownStreams.map(stream => stream.provider), ["twitch"]);
  assert.equal((await f.authorized("/v1/streams/me", { method: "DELETE", body: JSON.stringify({ provider: "arbitrary" }) })).status, 400);
  const stored = JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8"));
  assert.equal(stored.version, 2);
  assert.equal(stored.users["11"].twitch.channelId, "alice_new");
  assert.equal(stored.users["11"].youtube, undefined);
});

test("single-platform registry migrates without dropping the existing registration", async t => {
  const f = await fixture(t);
  await writeFile(path.join(f.data, "streams.json"), JSON.stringify({ version: 1, users: { "11": { url: "https://twitch.tv/alice" } } }), { mode: 0o600 });
  await f.restart();
  assert.deepEqual((await f.list()).ownStreams.map(stream => stream.provider), ["twitch"]);
  await f.save(`https://youtu.be/${videoId}`);
  assert.equal((await f.list()).ownStreams.length, 2);
  assert.equal(JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8")).version, 2);
});

test("recordings are authenticated and contain only eligible members, surviving registration removal and restart", async t => {
  let ended = false;
  const f = await fixture(t, url => {
    assert(url.includes("googleapis"));
    return json({ items: [{ id: videoId, snippet: { title: "Guild raid", liveBroadcastContent: ended ? "none" : "live" },
      status: { embeddable: true }, liveStreamingDetails: { actualStartTime: "2023-11-14T20:00:00Z",
        ...(ended ? { actualEndTime: "2023-11-14T21:00:00Z" } : {}) } }] });
  }, providerEnv);
  for (const route of ["/v1/streams/vods", "/v1/streams/vods/11"]) assert.equal((await f.request(route)).status, 401);
  assert.equal(f.calls(), 0);
  await f.save(`https://youtu.be/${videoId}`);
  await f.list();
  ended = true;
  f.advance(30_000);
  assert.equal((await f.list()).streams.length, 0);
  let response = await f.authorized("/v1/streams/vods");
  assert.equal(response.headers.get("cache-control"), "no-store");
  let recordings = (await response.json()).vods;
  assert.equal(recordings.length, 1);
  assert.equal(recordings[0].userId, "11");
  assert.equal(recordings[0].name, "Alice");
  assert.equal(recordings[0].url, `https://www.youtube.com/watch?v=${videoId}`);
  assert.equal(recordings[0].endedAt, "2023-11-14T21:00:00.000Z");
  await f.authorized("/v1/streams/me", { method: "DELETE", body: JSON.stringify({ provider: "youtube" }) });
  await f.restart();
  response = await f.authorized("/v1/streams/vods/11");
  assert.equal((await response.json()).vods.length, 1);
  f.setMembers(members.filter(member => member.user.id !== "11"));
  await f.restart();
  response = await f.authorized("/v1/streams/vods", {}, "bob-token");
  assert.deepEqual((await response.json()).vods, []);
  assert.equal((await f.authorized("/v1/streams/vods/11", {}, "bob-token")).status, 404);
});

test("background worker discovers completed recordings after restart without any client reading streams", async t => {
  let checks = 0;
  const f = await fixture(t, url => {
    assert(url.includes("googleapis"));
    checks++;
    return json({ items: [{ id: videoId, snippet: { title: "Finished while away", liveBroadcastContent: "none" },
      status: { embeddable: true }, liveStreamingDetails: { actualStartTime: "2023-11-14T20:00:00Z", actualEndTime: "2023-11-14T21:00:00Z" } }] });
  }, { ...providerEnv, STREAM_BACKGROUND_ENABLED: "true" });
  await writeFile(path.join(f.data, "streams.json"), JSON.stringify({ version: 2, users: { "11": { youtube: { url: `https://youtu.be/${videoId}` } } } }), { mode: 0o600 });
  await f.restart();
  for (let i = 0; i < 200 && !checks; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert(checks >= 1);
  // Depending on scheduler timing, the first server may also notice the seed
  // before closing. Neither server needs a member request to collect the VOD.
  assert.equal(f.userChecks(), 0);
  const response = await f.authorized("/v1/streams/vods");
  assert.equal((await response.json()).vods[0].title, "Finished while away");
});

test("Twitch broadcast identity reaches archive discovery and delayed VODs survive unregistering", async t => {
  let live = true;
  let archiveReady = false;
  let archiveChecks = 0;
  const f = await fixture(t, (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    const query = new URL(url).searchParams;
    if (url.includes("/helix/videos")) {
      archiveChecks++;
      assert.equal(query.get("user_id"), "444");
      assert.equal(query.get("type"), "archive");
      return json({ data: archiveReady ? [{ id: "999", stream_id: "555", user_id: "444", type: "archive", title: "Archived raid" }] : [] });
    }
    return json({ data: live ? [{ user_login: "alice", type: "live", user_id: "444", id: "555", title: "Live raid", started_at: "2023-11-14T20:00:00Z" }] : [] });
  }, providerEnv);
  await f.save("https://twitch.tv/alice");
  assert.equal((await f.list()).streams.length, 1);
  await f.authorized("/v1/streams/me", { method: "DELETE", body: JSON.stringify({ provider: "twitch" }) });
  live = false;
  f.advance(30_000);
  assert.deepEqual((await f.list()).ownStreams, []);
  assert.equal(archiveChecks, 1);
  assert.deepEqual((await (await f.authorized("/v1/streams/vods")).json()).vods, []);
  await f.restart();
  archiveReady = true;
  f.advance(60_000);
  await f.list();
  assert.equal(archiveChecks, 2);
  const recordings = (await (await f.authorized("/v1/streams/vods")).json()).vods;
  assert.equal(recordings.length, 1);
  assert.equal(recordings[0].url, "https://www.twitch.tv/videos/999");
  assert.equal(recordings[0].userId, "11");
  assert.equal(recordings[0].title, "Archived raid");
});

test("Twitch live streams disappear when ended; failed checks are unknown and withdraw stale playback", async t => {
  let status = "live";
  let providerCalls = 0;
  const f = await fixture(t, (url, options) => {
    if (url.includes("/helix/")) {
      providerCalls++;
      if (status === "offline") return json({ data: [] });
      if (status === "failed") return json({}, 503);
    }
    return twitchLive(url, options);
  }, providerEnv);
  await f.save("https://twitch.tv/alice");
  let snapshot = await f.list();
  assert.equal(snapshot.streams[0].name, "Alice");
  assert.equal(snapshot.streams[0].viewerCount, 12);
  assert.equal(snapshot.refreshAfterSeconds, 30);
  assert.equal(snapshot.unverifiedCount, 0);
  await f.list();
  assert.equal(providerCalls, 1);
  status = "offline";
  f.advance(30_000);
  snapshot = await f.list();
  assert.deepEqual(snapshot.streams, []);
  assert.equal(snapshot.ownStream.status, "offline");
  assert.equal(snapshot.unverifiedCount, 0);
  status = "live";
  f.advance(30_000);
  assert.equal((await f.list()).streams.length, 1);
  status = "failed";
  f.advance(30_000);
  snapshot = await f.list();
  assert.deepEqual(snapshot.streams, []);
  assert.equal(snapshot.ownStream.status, "unknown");
  assert.equal(snapshot.unverifiedCount, 1);
  const player = await f.authorized("/v1/streams/player/11");
  assert.equal(player.status, 404);
  assert(!(await player.text()).includes("twitch.tv"));
});

test("Twitch filtered short pages may include a cursor without invalidating verified live or offline channels", async t => {
  let calls = 0;
  const f = await fixture(t, (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    calls++;
    const query = new URL(url).searchParams;
    assert.equal(query.get("first"), "100");
    assert.deepEqual(query.getAll("user_login").sort(), ["alice", "bob"]);
    return json({ data: [{ user_login: "Alice", type: "live" }], pagination: { cursor: "provider-cursor" } });
  }, providerEnv);
  await f.save("https://twitch.tv/alice");
  await f.save("https://twitch.tv/bob", "bob-token");
  const snapshot = await f.list();
  assert.equal(snapshot.ownStream.status, "live");
  assert.equal(snapshot.unverifiedCount, 0);
  assert.deepEqual(snapshot.streams.map(stream => stream.userId), ["11"]);
  assert.equal((await f.list("bob-token")).ownStream.status, "offline");
  assert.equal(calls, 1, "a fully bounded filtered query does not need extra pagination requests");
});

test("Twitch duplicate or unrelated channel rows cannot produce a false offline result", async t => {
  let rows = [{ user_login: "alice", type: "live" }, { user_login: "ALICE", type: "live" }];
  const f = await fixture(t, (url, options) => url.includes("oauth2/token") ? twitchLive(url, options)
    : json({ data: rows, pagination: { cursor: "provider-cursor" } }), providerEnv);
  await f.save("https://twitch.tv/alice");
  await f.save("https://twitch.tv/bob", "bob-token");
  for (const invalid of [rows, [{ user_login: "unrelated", type: "live" }]]) {
    rows = invalid;
    const snapshot = await f.list();
    assert.deepEqual(snapshot.streams, []);
    assert.equal(snapshot.ownStream.status, "unknown");
    assert.equal(snapshot.unverifiedCount, 2);
    f.advance(30_000);
  }
});

test("new saves remain checking until the shared refresh and do not warn or invalidate another platform", async t => {
  let providerCalls = 0;
  let failTwitch = false;
  const f = await fixture(t, (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    providerCalls++;
    if (url.includes("googleapis")) return liveYoutubeBatch(new URL(url));
    return failTwitch ? json({}, 503) : json({ data: [] });
  }, providerEnv);
  await f.list(); // The directory was refreshed just before the member submitted a link.
  for (const url of ["https://twitch.tv/alice", `https://youtu.be/${videoId}`]) {
    const saved = await (await f.save(url)).json();
    assert.equal(saved.ownStream.status, "checking");
  }
  let snapshot = await f.list();
  assert(snapshot.ownStreams.every(stream => stream.status === "checking"));
  assert.equal(snapshot.unverifiedCount, 0);
  assert.deepEqual(snapshot.streams, []);
  assert.equal(providerCalls, 0);
  assert.equal((await f.authorized("/v1/streams/player/11/twitch")).status, 404);
  f.advance(30_000);
  snapshot = await f.list();
  assert.equal(snapshot.ownStreams.find(stream => stream.provider === "twitch").status, "offline");
  assert.equal(snapshot.ownStreams.find(stream => stream.provider === "youtube").status, "live");
  assert.equal(snapshot.unverifiedCount, 0);
  assert.equal(providerCalls, 2);
  for (const channel of ["alice_next", "alice_again"]) {
    const saved = await (await f.save(`https://twitch.tv/${channel}`)).json();
    assert.equal(saved.ownStream.status, "checking");
    snapshot = await f.list();
    assert.equal(snapshot.unverifiedCount, 0);
    assert.equal(snapshot.streams.length, 1);
    assert.equal(snapshot.streams[0].provider, "youtube");
  }
  assert.equal(providerCalls, 2, "rapid saves must not force additional API calls");
  failTwitch = true;
  f.advance(30_000);
  snapshot = await f.list();
  assert.equal(snapshot.ownStreams.find(stream => stream.provider === "twitch").status, "unknown");
  assert.equal(snapshot.ownStreams.find(stream => stream.provider === "youtube").status, "live");
  assert.equal(snapshot.unverifiedCount, 1, "an actual failed attempt still surfaces a warning");
  assert.equal(providerCalls, 4);
});

test("manual saves check only their target immediately and leave the normal 30-second round unchanged", async t => {
  const queries = [];
  const f = await fixture(t, (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    const parsed = new URL(url);
    queries.push({ provider: url.includes("googleapis") ? "youtube" : "twitch",
      ids: parsed.searchParams.get("id")?.split(",") || parsed.searchParams.getAll("user_login") });
    return url.includes("googleapis") ? liveYoutubeBatch(parsed) : twitchLive(url, options);
  }, providerEnv, { firstStreamCheckWaitMs: 5_000 });
  await f.list();
  f.advance(10_000);
  assert.equal((await (await f.save("https://twitch.tv/alice")).json()).ownStream.status, "live");
  f.advance(10_000);
  const saved = await (await f.save(`https://youtu.be/${videoId}`)).json();
  assert(saved.ownStreams.every(stream => stream.status === "live"));
  assert.deepEqual(queries, [{ provider: "twitch", ids: ["alice"] }, { provider: "youtube", ids: [videoId] }]);
  assert.equal((await f.list()).streams.length, 2);
  assert.equal(queries.length, 2);
  f.advance(10_000);
  assert.equal((await f.list()).streams.length, 2);
  assert.equal(queries.length, 4, "manual saves do not postpone the original normal deadline");
});

test("slow mixed-provider rounds keep the next normal refresh aligned with fast Twitch freshness", async t => {
  let releaseYoutube;
  let twitchCalls = 0;
  const f = await manualFixture(t, (url, options) => {
    if (url.includes("id.twitch.tv")) return twitchLive(url, options);
    if (url.includes("api.twitch.tv")) {
      twitchCalls++;
      return twitchLive(url, options);
    }
    if (releaseYoutube) return liveYoutubeBatch(new URL(url));
    return new Promise(resolve => { releaseYoutube = () => resolve(liveYoutubeBatch(new URL(url))); });
  }, { "1": { twitch: { url: "https://twitch.tv/alice" }, youtube: { url: `https://youtu.be/${videoId}` } } });
  const first = f.snapshot();
  for (let i = 0; i < 100 && !releaseYoutube; i++) await new Promise(resolve => setTimeout(resolve, 1));
  assert(releaseYoutube);
  await new Promise(resolve => setImmediate(resolve));
  f.advance(10_000);
  releaseYoutube();
  assert.equal((await first).streams.length, 2);
  assert.equal(f.service.nextPollDelayMs(), 20_000, "provider latency cannot postpone the regular deadline");
  f.advance(19_999);
  assert.equal((await f.snapshot()).streams.length, 2);
  assert.equal(twitchCalls, 1);
  assert.equal(f.service.nextPollDelayMs(), 1_000, "worker scheduling stays bounded near the deadline");
  f.advance(1);
  const next = await f.snapshot();
  assert.equal(twitchCalls, 2, "the next round begins before a verified Twitch status gets a skipped tick");
  assert.equal(next.streams.length, 2);
  assert.equal(next.unverifiedCount, 0);
  assert.equal(f.service.nextPollDelayMs(), 30_000);
});

test("manual YouTube replacements preserve other members' fresh cached broadcasts", async t => {
  const queries = [];
  const f = await fixture(t, url => {
    const parsed = new URL(url);
    queries.push(parsed.searchParams.get("id"));
    return liveYoutubeBatch(parsed);
  }, providerEnv, { firstStreamCheckWaitMs: 5_000 });
  await f.save(`https://youtu.be/${videoId}`);
  await f.save("https://youtu.be/abcDEF_12-4", "bob-token");
  await f.save("https://youtu.be/abcDEF_12-5");
  const snapshot = await f.list();
  assert.deepEqual(snapshot.streams.map(stream => stream.channelId).sort(), ["abcDEF_12-4", "abcDEF_12-5"]);
  assert.deepEqual(queries, [videoId, "abcDEF_12-4", "abcDEF_12-5"]);
});

test("concurrent first checks deduplicate canonical targets and keep every submitting recording owner", async t => {
  let release;
  let calls = 0;
  const f = await fixture(t, url => {
    calls++;
    return new Promise(resolve => { release = () => resolve(liveYoutubeBatch(new URL(url))); });
  }, providerEnv, { firstStreamCheckWaitMs: 5_000 });
  const alice = f.save(`https://youtu.be/${videoId}`);
  for (let i = 0; i < 200 && !release; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert(release);
  const bob = f.save(`https://www.youtube.com/watch?v=${videoId}&feature=share`, "bob-token");
  for (let i = 0; i < 200; i++) {
    const registry = JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8"));
    if (registry.users["22"]) break;
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  await new Promise(resolve => setTimeout(resolve, 10));
  release();
  for (const response of await Promise.all([alice, bob])) {
    assert.equal(response.status, 200);
    assert.equal((await response.json()).ownStream.status, "live");
  }
  assert.equal(calls, 1);
  const recordings = JSON.parse(await readFile(path.join(f.data, "stream-vods.json"), "utf8"));
  assert.deepEqual(recordings.active[0].owners.sort(), ["11", "22"]);
  assert.equal((await (await f.save(`https://youtube.com/live/${videoId}`)).json()).ownStream.status, "live");
  assert.equal(calls, 1, "a fresh canonical target reuses its verified cache");
});

test("a persisted save succeeds after its provider check deadline", { timeout: 10_000 }, async t => {
  const provider = Promise.withResolvers();
  const f = await fixture(t, async (url, options) => {
    if (!url.includes("oauth2/token")) await provider.promise;
    return twitchLive(url, options);
  }, providerEnv, { firstStreamCheckWaitMs: 120 });
  try {
    const saved = await f.save("https://twitch.tv/alice");
    assert.equal(saved.status, 200);
    assert.equal((await saved.json()).ownStream.status, "checking");
    assert.equal(JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8")).users["11"].twitch.channelId, "alice");
  } finally {
    provider.resolve();
  }
});

test("a save finishing after concurrent removal reports no registration", { timeout: 10_000 }, async t => {
  const replacementStarted = Promise.withResolvers();
  const provider = Promise.withResolvers();
  const f = await fixture(t, async (url, options) => {
    if (new URL(url).searchParams.getAll("user_login").includes("alice_next")) {
      replacementStarted.resolve();
      await provider.promise;
    }
    return twitchLive(url, options);
  }, providerEnv, { firstStreamCheckWaitMs: 5_000 });
  const saved = await f.save("https://twitch.tv/alice");
  assert.equal(saved.status, 200);
  assert.equal((await saved.json()).ownStream.status, "live");
  let saveFinished = false;
  const waiting = f.save("https://twitch.tv/alice_next").then(response => {
    saveFinished = true;
    return response;
  });
  try {
    await replacementStarted.promise;
    const registry = JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8"));
    assert.equal(registry.users["11"].twitch.channelId, "alice_next");
    assert.equal(saveFinished, false, "the replacement check must still be pending before removal");
    assert.equal((await f.authorized("/v1/streams/me", { method: "DELETE" })).status, 200);
    assert.equal(JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8")).users["11"], undefined);
    assert.equal(saveFinished, false, "removal must finish before the replacement save returns");
  } finally {
    provider.resolve();
  }
  const response = await waiting;
  assert.equal(response.status, 200);
  const removed = await response.json();
  assert.equal(removed.ownStream, null);
  assert.deepEqual(removed.ownStreams, []);
  const current = await f.list();
  assert.equal(current.ownStream, null);
  assert.deepEqual(current.ownStreams, []);
});

test("a save finishing after a replacement reports the current registration instead of the old URL", async t => {
  let release;
  let calls = 0;
  const f = await fixture(t, (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    if (calls++ === 0) return new Promise(resolve => { release = () => resolve(twitchLive(url, options)); });
    return twitchLive(url, options);
  }, providerEnv, { firstStreamCheckWaitMs: 5_000 });
  const previous = f.save("https://twitch.tv/alice");
  for (let i = 0; i < 200 && !release; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert(release);
  const replacement = f.save("https://twitch.tv/alice_next");
  for (let i = 0; i < 200; i++) {
    const registry = JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8"));
    if (registry.users["11"].twitch.channelId === "alice_next") break;
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  release();
  for (const response of await Promise.all([previous, replacement])) {
    assert.equal(response.status, 200);
    const body = await response.json();
    assert.equal(body.ownStream.channelId, "alice_next");
    assert.deepEqual(body.ownStreams.map(stream => stream.channelId), ["alice_next"]);
  }
  assert.equal((await f.list()).ownStream.status, "live");
});

test("manual YouTube checks respect the same provider backoff without failing a persisted submission", async t => {
  let calls = 0;
  const f = await fixture(t, () => { calls++; return json({}, 403); }, providerEnv, { firstStreamCheckWaitMs: 5_000 });
  assert.equal((await (await f.save(`https://youtu.be/${videoId}`)).json()).ownStream.status, "unknown");
  const response = await f.save("https://youtu.be/abcDEF_12-4");
  assert.equal(response.status, 200);
  assert.equal((await response.json()).ownStream.status, "unknown");
  assert.equal(calls, 1, "a new URL cannot bypass an API backoff");
});

test("manual queues stay bounded and share the three-worker limit without losing the normal round's cache", async t => {
  const users = Object.fromEntries(Array.from({ length: 201 }, (_, index) => [String(index + 1), {
    twitch: { url: `https://twitch.tv/member_${index + 1}` },
  }]));
  let active = 0;
  let peak = 0;
  let normalFinished = 0;
  let manualCalls = 0;
  const releases = [];
  const f = await manualFixture(t, async (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    const manual = new URL(url).searchParams.getAll("user_login").every(id => id.startsWith("manual_"));
    active++;
    peak = Math.max(peak, active);
    if (manual) {
      manualCalls++;
      assert.equal(normalFinished, 3, "manual work starts after the existing normal round");
    } else {
      await new Promise(resolve => { releases.push(resolve); });
      normalFinished++;
    }
    const result = twitchLive(url, options);
    active--;
    return result;
  }, users);
  const normal = f.snapshot();
  for (let i = 0; i < 200 && releases.length < 3; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert.equal(releases.length, 3);
  let completed = 0;
  const saves = Array.from({ length: 150 }, (_, index) => f.save(`https://twitch.tv/manual_${index}`, String(202 + index)).then(result => { completed++; return result; }));
  for (let i = 0; i < 600 && completed < 22; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert.equal(completed, 22, "only the overflow beyond 128 queued targets falls back immediately");
  assert.equal(manualCalls, 0);
  for (const release of releases) release();
  await normal;
  const saved = await Promise.all(saves);
  assert.equal(saved.filter(result => result.ownStream.status === "live").length, 128);
  assert.equal(saved.filter(result => result.ownStream.status === "checking").length, 22);
  assert.equal(manualCalls, 2, "the bounded queue is coalesced into 100-channel provider batches");
  assert.equal(peak, 3);
  assert.equal((await f.snapshot()).streams.length, 329, "targeted cache merges retain every existing live member");
});

test("manual YouTube quota is bounded separately and cannot consume the normal refresh allowance", async t => {
  let calls = 0;
  const f = await manualFixture(t, async url => {
    assert.equal(new URL(url).hostname, "www.googleapis.com");
    calls++;
    return json({ items: [] });
  });
  const url = index => `https://youtu.be/${String(index).padStart(11, "0")}`;
  for (let index = 0; index < 256; index++) {
    assert.equal((await f.save(url(index))).ownStream.status, "offline");
  }
  assert.equal(calls, 256);
  assert.equal((await f.save(url(256))).ownStream.status, "checking");
  assert.equal(calls, 256);
  f.advance(30_000);
  assert.equal((await f.snapshot()).ownStream.status, "offline");
  assert.equal(calls, 257, "normal quota remains available after the manual allowance is exhausted");
  assert.equal((await f.save(url(257))).ownStream.status, "checking");
  f.advance(86_400_000);
  assert.equal((await f.save(url(258))).ownStream.status, "offline");
  assert.equal(calls, 258, "manual allowance returns after its rolling 24-hour window");
});

test("a manual first check does not query unrelated pending Twitch recording targets", async t => {
  const requests = [];
  const f = await manualFixture(t, async url => {
    requests.push(new URL(url).hostname);
    return liveYoutubeBatch(new URL(url));
  });
  await writeFile(path.join(f.dataDir, "stream-vods.json"), JSON.stringify({ version: 1, history: [], active: [{
    provider: "twitch", channelId: "other_channel", owners: ["2"], title: "", live: false,
    lastSeenAt: 1_700_000_000_000, nextAttemptAt: 0, endedAt: "2023-11-14T22:13:20.000Z",
    streamId: "123", twitchUserId: "456",
  }] }));
  assert.equal((await f.save(`https://youtu.be/${videoId}`)).ownStream.status, "live");
  assert.deepEqual(requests, ["www.googleapis.com"]);
});

test("YouTube actual end wins over live snippet; upcoming, missing, and nonembeddable videos are never listed", async t => {
  let video = { id: videoId, snippet: { title: "My live raid", liveBroadcastContent: "live" }, status: { embeddable: true }, liveStreamingDetails: { concurrentViewers: "123", actualStartTime: "2023-11-14T20:00:00Z" } };
  let fail = false;
  const f = await fixture(t, url => {
    assert.equal(new URL(url).hostname, "www.googleapis.com");
    assert.equal(new URL(url).searchParams.get("part"), "snippet,status,liveStreamingDetails");
    return fail ? json({}, 403) : json({ items: video ? [video] : [] });
  }, providerEnv);
  await f.save(`https://youtu.be/${videoId}`);
  assert.equal((await f.list()).streams[0].viewerCount, 123);
  assert.equal((await f.list()).ownStreams[0].broadcastState, "live");
  assert.equal((await f.list()).streams[0].replayStartMs, Date.parse("2023-11-14T20:00:00Z"));
  assert.equal((await f.list()).streams[0].replayEndMs, 1_700_000_000_000 - 30_000);
  video.liveStreamingDetails.actualEndTime = "2023-11-14T21:00:00Z";
  f.advance(30_000);
  assert.equal((await f.list()).ownStream.status, "offline");
  assert.equal((await f.list()).ownStreams[0].broadcastState, "ended");
  assert.equal((await f.list()).ownStreams[0].replayEndMs, Date.parse("2023-11-14T21:00:00Z"));
  delete video.liveStreamingDetails.actualEndTime;
  video.status.embeddable = false;
  f.advance(30_000);
  assert.equal((await f.list()).streams.length, 0);
  video.snippet.liveBroadcastContent = "upcoming";
  f.advance(30_000);
  assert.equal((await f.list()).ownStream.status, "offline");
  assert.equal((await f.list()).ownStreams[0].broadcastState, "upcoming");
  video = null;
  f.advance(30_000);
  assert.equal((await f.list()).ownStream.status, "offline");
  fail = true;
  f.advance(30_000);
  assert.equal((await f.list()).ownStream.status, "unknown");
  assert.equal((await f.list()).ownStreams[0].broadcastState, undefined);
  assert.equal((await f.list()).ownStreams[0].replayStartMs, undefined);
  assert.equal((await f.list()).ownStreams[0].replayEndMs, undefined);
  fail = false;
  const privateId = "abcDEF_12-4";
  video = { id: privateId, snippet: { title: "Private recording", liveBroadcastContent: "none" },
    status: { embeddable: true, privacyStatus: "private" }, liveStreamingDetails: { actualEndTime: "2023-11-14T21:00:00Z" } };
  await f.save(`https://youtu.be/${privateId}`);
  f.advance(30_000);
  assert.equal((await f.list()).ownStreams[0].status, "unknown");
  assert.equal((await f.list()).ownStreams[0].broadcastState, undefined);
  assert(!(await (await f.authorized("/v1/streams/vods")).json()).vods.some(vod => vod.id === privateId));
});

test("authenticated players use only official iframes, trusted parent and safe referrer/CSP; no credentials in HTML", async t => {
  const f = await fixture(t, (url, options) => url.includes("googleapis")
    ? json({ items: [{ id: videoId, snippet: { title: '<script>alert("x")</script>', liveBroadcastContent: "live" }, status: { embeddable: true }, liveStreamingDetails: {} }] })
    : twitchLive(url, options), providerEnv);
  await f.save("https://twitch.tv/alice");
  await f.save(`https://youtu.be/${videoId}`, "bob-token");
  for (const [id, provider] of [["11", "player.twitch.tv"], ["22", "www.youtube.com/embed/"]]) {
    const response = await f.authorized(`/v1/streams/player/${id}`, { headers: { host: "evil.example", "x-forwarded-host": "evil.example" } });
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.equal(response.headers.get("referrer-policy"), "strict-origin-when-cross-origin");
    assert(response.headers.get("content-security-policy").includes("frame-ancestors 'none'"));
    const html = await response.text();
    assert(html.includes(provider));
    if (provider.includes("youtube")) {
      assert.match(html, /<iframe[^>]+allow="[^"]*; clipboard-write"/);
      assert(!html.includes("clipboard-read"));
    }
    const source = id === '11' ? html.match(/<div id="media" data-src="([^"]+)"/) : html.match(/<iframe\b[^>]*\bsrc="([^"]+)"/);
    assert(source, 'the trusted media element must exist');
    const iframe = new URL(source[1].replaceAll('&amp;', '&'));
    assert.equal(iframe.protocol, 'https:');
    assert.equal(iframe.hostname, id === '11' ? 'player.twitch.tv' : 'www.youtube.com');
    if (id === '11') assert.equal(iframe.searchParams.get('parent'), 'brick.example.com');
    else assert.equal(new URL(iframe.searchParams.get('origin')).origin, 'https://brick.example.com');
    for (const secret of ["alice-token", "test-bot", "test-secret", "test-app-token", "test-key", "evil.example", '<script>alert("x")</script>']) assert(!html.includes(secret));
  }
  const anonymous = await f.request("/v1/streams/player/11?token=alice-token");
  assert.equal(anonymous.status, 401);
  assert(!(await anonymous.text()).includes("alice"));
});

test("concurrent directory reads share one provider refresh", async t => {
  let calls = 0;
  let release;
  const f = await fixture(t, (url, options) => {
    if (url.includes("/helix/")) {
      calls++;
      return new Promise(resolve => { release = () => resolve(twitchLive(url, options)); });
    }
    return twitchLive(url, options);
  }, providerEnv);
  await f.save("https://twitch.tv/alice");
  const pending = Array.from({ length: 8 }, () => f.list());
  for (let i = 0; i < 200 && !release; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert(release);
  await new Promise(resolve => setTimeout(resolve, 25));
  assert.equal(calls, 1);
  release();
  const results = await Promise.all(pending);
  assert(results.every(snapshot => snapshot.streams.length === 1));
});

test("a save during an in-flight refresh stays checking until its own target is checked", async t => {
  let calls = 0;
  let release;
  const f = await fixture(t, (url, options) => {
    if (url.includes("/helix/") && calls++ === 0) {
      return new Promise(resolve => { release = () => resolve(twitchLive(url, options)); });
    }
    return twitchLive(url, options);
  }, providerEnv);
  await f.save("https://twitch.tv/alice");
  const pending = f.list();
  for (let i = 0; i < 200 && !release; i++) await new Promise(resolve => setTimeout(resolve, 5));
  assert(release);
  const saved = await (await f.save("https://twitch.tv/alice_next")).json();
  assert.equal(saved.ownStream.status, "checking");
  release();
  const snapshot = await pending;
  assert.equal(snapshot.ownStream.channelId, "alice_next");
  assert.equal(snapshot.ownStream.status, "checking");
  assert.equal(snapshot.unverifiedCount, 0);
  assert.deepEqual(snapshot.streams, []);
  assert.equal(calls, 1);
  f.advance(30_000);
  assert.equal((await f.list()).ownStream.status, "live");
  assert.equal(calls, 2);
});

test("stream checks never use stale Discord authentication or stale guild membership", async t => {
  const f = await fixture(t, twitchLive, { ...providerEnv, REQUESTER_CACHE_SECONDS: "1", ROSTER_CACHE_SECONDS: "1" });
  await f.save("https://twitch.tv/alice");
  assert.equal((await f.list()).streams.length, 1);
  await new Promise(resolve => setTimeout(resolve, 1050));
  f.failDiscord(429);
  const expired = await f.authorized();
  assert.equal(expired.status, 429);
  const body = await expired.text();
  assert(!body.includes("alice"));
  assert(!body.includes("Raid night"));
});

test("a member removed from the guild directory cannot read streams through a cached valid token", async t => {
  const f = await fixture(t, twitchLive, { ...providerEnv, ROSTER_CACHE_SECONDS: "1" });
  await f.save("https://twitch.tv/alice");
  await f.list();
  f.setMembers(members.filter(member => member.user.id !== "11"));
  await new Promise(resolve => setTimeout(resolve, 1050));
  assert.equal((await f.authorized()).status, 403);
  assert.equal((await f.authorized("/v1/streams/me", { method: "PUT", body: "invalid JSON" })).status, 403);
  assert.equal((await f.authorized("/v1/streams/me", { method: "DELETE" })).status, 403);
  const registry = JSON.parse(await readFile(path.join(f.data, "streams.json"), "utf8"));
  assert.equal(registry.users["11"].twitch.channelId, "alice");
  assert.deepEqual((await f.list("bob-token")).streams, []);
});

test("a failed roster refresh does not expose streams through the legacy stale-roster fallback", async t => {
  const f = await fixture(t, twitchLive, { ...providerEnv, ROSTER_CACHE_SECONDS: "1" });
  await f.save("https://twitch.tv/alice");
  assert.equal((await f.list()).streams.length, 1);
  await new Promise(resolve => setTimeout(resolve, 1050));
  f.failDiscord(503);
  const response = await f.authorized();
  assert.equal(response.status, 503);
  const body = await response.text();
  assert(!body.includes("alice"));
  assert(!body.includes("Raid night"));
});

test("provider batches use Twitch first=100, YouTube groups of 50 and at most three simultaneous requests", async t => {
  const dataDir = await mkdtemp(path.join(os.tmpdir(), "brick-stream-batch-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  const users = {};
  const eligible = [];
  for (let i = 0; i < 202; i++) {
    const userId = String(i + 1);
    users[userId] = { url: i < 101 ? `https://twitch.tv/channel_${i}` : `https://youtu.be/${String(i).padStart(11, "0")}` };
    eligible.push({ userId, name: `User ${i}` });
  }
  await writeFile(path.join(dataDir, "streams.json"), JSON.stringify({ version: 1, users }), { mode: 0o600 });
  let active = 0;
  let peak = 0;
  let twitchBatches = 0;
  let youtubeBatches = 0;
  const service = await createStreamService({ dataDir, env: providerEnv, fetch: async (url, options) => {
    if (url.includes("oauth2/token")) return twitchLive(url, options);
    active++;
    peak = Math.max(peak, active);
    await new Promise(resolve => setTimeout(resolve, 5));
    active--;
    const query = new URL(url).searchParams;
    if (url.includes("helix")) {
      twitchBatches++;
      assert(query.getAll("user_login").length <= 100);
      return twitchLive(url, options);
    }
    youtubeBatches++;
    assert(query.get("id").split(",").length <= 50);
    return json({ items: [] });
  } });
  const snapshot = await service.snapshot({ id: "1", name: "User 0" }, eligible);
  assert.equal(snapshot.streams.length, 101);
  assert.equal(twitchBatches, 2);
  assert.equal(youtubeBatches, 3);
  assert.equal(peak, 3);
});

test("player origins are configured HTTPS origins with loopback HTTP for local testing", () => {
  assert.equal(parsePlayerOrigin("https://brick.example.com").hostname, "brick.example.com");
  assert.equal(parsePlayerOrigin("http://127.0.0.1:8080").origin, "http://127.0.0.1:8080");
  assert.equal(parsePlayerOrigin(undefined), null);
  for (const origin of ["http://brick.example.com", "https://user:pass@brick.example.com", "https://brick.example.com/path", "https://brick.example.com/?key=x", "javascript:alert(1)"]) assert.throws(() => parsePlayerOrigin(origin));
});

async function seedRecording(f) {
  await writeFile(path.join(f.data, "stream-vods.json"), JSON.stringify({ version: 1, active: [], history: [{
    userId: "11", provider: "youtube", id: videoId, title: "Saved raid", startedAt: "2023-11-14T20:00:00Z", endedAt: "2023-11-14T21:00:00Z",
  }] }), { mode: 0o600 });
}

test("saved recording review and playback work without any live registration and retain authentication", async t => {
  let providerCalls = 0;
  const f = await fixture(t, url => {
    providerCalls++;
    const request = new URL(url);
    assert.equal(request.origin, "https://www.googleapis.com");
    assert.equal(request.searchParams.get("id"), videoId);
    assert(request.searchParams.get("part").includes("contentDetails"));
    return json({ items: [{ id: videoId, status: { embeddable: true, privacyStatus: "public" }, contentDetails: { duration: "PT59M55S" },
      liveStreamingDetails: { actualStartTime: "2023-11-14T20:00:00Z", actualEndTime: "2023-11-14T21:00:00Z" } }] });
  }, providerEnv);
  await seedRecording(f);
  const review = `/v1/streams/vods/11/youtube/${videoId}/review`;
  const player = `/v1/streams/player/11/youtube?recording=${videoId}&at=123.875&broadcast=${videoId}&paused=1`;
  for (const route of [review, player]) assert.equal((await f.request(route)).status, 401);
  assert.equal(providerCalls, 0);
  const metadata = await f.authorized(review);
  assert.equal(metadata.status, 200);
  assert.equal((await metadata.json()).availableSeconds, 3595);
  assert.equal((await f.list()).streams.length, 0);
  const response = await f.authorized(player);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  const html = await response.text();
  assert.match(html, /data-start="123.875"/);
  assert.match(html, /autoplay=0/);
  assert(!html.includes("Saved raid"));
  assert(!html.includes("alice-token"));
  assert.equal(providerCalls, 1);
  assert.equal((await f.authorized(review.replace("/11/", "/33/"))).status, 404);
  assert.equal((await f.authorized(player.replace("at=123.875", "at=3595"))).status, 409);
  assert.equal((await f.authorized(player + "&recording=../../secret")).status, 400);
  assert.equal((await f.authorized(player.replace("at=123.875", "at=NaN"))).status, 400);
  assert.equal(providerCalls, 1);
});

test("only current officers and the designated eligible user may remove saved recordings", async t => {
  const f = await fixture(t, () => assert.fail("Deleting must not contact providers"), { ROSTER_CACHE_SECONDS: "1" });
  await seedRecording(f);
  const route = `/v1/streams/vods/youtube/${videoId}`;
  assert.equal((await f.request(route, { method: "DELETE" })).status, 401);
  assert.equal((await (await f.authorized("/v1/streams/vods")).json()).canDeleteRecordings, false);
  assert.equal((await f.authorized(route, { method: "DELETE", headers: { "x-user-id": "341518802208423957" } })).status, 403);
  assert.equal((await (await f.authorized("/v1/streams/vods", {}, "bob-token")).json()).canDeleteRecordings, true);
  // Downgrade an officer while the same OAuth token is cached.
  f.setMembers(members.map(member => member.user.id === "22" ? { ...member, roles: ["raider"] } : member));
  f.advance(2000);
  await new Promise(resolve => setTimeout(resolve, 1050));
  assert.equal((await f.authorized(route, { method: "DELETE" }, "bob-token")).status, 403);
  assert.equal((await (await f.authorized("/v1/streams/vods")).json()).vods.length, 1);
  f.setMembers([...members, { user: { id: "341518802208423957", username: "Authorized reviewer" }, roles: ["raider"] }]);
  f.advance(2000);
  await new Promise(resolve => setTimeout(resolve, 1050));
  assert.equal((await (await f.authorized("/v1/streams/vods", {}, "special-token")).json()).canDeleteRecordings, true);
  assert.equal((await f.authorized(route, { method: "DELETE" }, "special-token")).status, 200);
  await f.restart();
  assert.deepEqual((await (await f.authorized("/v1/streams/vods")).json()).vods, []);
  assert.equal((await f.authorized(`/v1/streams/vods/11/youtube/${videoId}/review`)).status, 404);
});

test("officer removal succeeds and a designated user without guild access cannot delete", async t => {
  const f = await fixture(t);
  await seedRecording(f);
  const route = `/v1/streams/vods/youtube/${videoId}`;
  assert.equal((await f.authorized(route, { method: "DELETE" }, "special-token")).status, 403);
  assert.equal((await f.authorized(route, { method: "DELETE" }, "bob-token")).status, 200);
  assert.deepEqual((await (await f.authorized("/v1/streams/vods")).json()).vods, []);
});

test("shared raid timestamps require guild authorization and a visible VOD before storage", async t => {
  const f = await fixture(t);
  await seedRecording(f);
  const key = { readerVersion: 1, provider: 'youtube', videoId, broadcastId: videoId, report: 'abcdefghABCDEFGH', pullId: 1, encounter: 3429, difficulty: 4,
    startMs: Date.parse('2023-11-14T20:05:00Z'), endMs: Date.parse('2023-11-14T20:11:00Z'), recordingStartMs: Date.parse('2023-11-14T20:00:00Z') };
  const alignment = { unixSeconds: key.startMs / 1000, videoSeconds: 300.12, uncertaintySeconds: 0.1 };
  const lookup = '/v1/streams/review/sync/lookup';
  const upload = '/v1/streams/review/sync/observations';
  const request = body => ({ method: 'POST', body: JSON.stringify(body) });
  assert.equal((await f.request(lookup, request({ keys: [key] }))).status, 401);
  assert.equal((await f.request(upload, request({ key, alignment }))).status, 401);
  assert.equal((await f.authorized(upload, request({ key, alignment }), 'removed-token')).status, 403);
  const first = await f.authorized(upload, request({ key, alignment }));
  assert.equal(first.status, 200); assert.equal((await first.json()).verified, false);
  assert.equal((await (await f.authorized(upload, request({ key, alignment }))).json()).confirmations, 1);
  assert.equal((await (await f.authorized(upload, request({ key, alignment }), 'bob-token')).json()).verified, true);
  const result = await (await f.authorized(lookup, request({ keys: [key] }))).json();
  assert.equal(result.results[0].alignment.verified, true);
  assert.equal(result.results[0].alignment.videoSeconds, alignment.videoSeconds);
  assert.equal((await f.authorized(lookup, request({ keys: [{ ...key, videoId: 'zbcDEF_12-3' }] }))).status, 404);
  assert.equal((await f.authorized(upload, request({ key: { ...key, difficulty: 10 }, alignment }))).status, 400);
  f.setMembers([members[1]]); await f.restart();
  assert.equal((await f.authorized(lookup, request({ keys: [key] }), 'bob-token')).status, 404);
});

test("verified YouTube media clocks reach live and archived review HTTP responses", async t => {
  const { createReplaySyncLibrary } = await import('./replay_sync.mjs');
  for (const live of [true, false]) {
    const f = await fixture(t, () => json({ items: [{ id: videoId,
      snippet: { title: 'Raid', liveBroadcastContent: live ? 'live' : 'none' },
      status: { embeddable: true, privacyStatus: 'public' }, contentDetails: { duration: 'PT59M55S' },
      liveStreamingDetails: { actualStartTime: '2023-11-14T20:00:00Z',
        ...(!live ? { actualEndTime: '2023-11-14T21:00:00Z' } : {}) } }] }), providerEnv);
    if (live) { await f.save(`https://youtu.be/${videoId}`); await f.list(); }
    else await seedRecording(f);
    const route = live ? '/v1/streams/review/11/youtube' : `/v1/streams/vods/11/youtube/${videoId}/review`;
    const original = await (await f.authorized(route)).json();
    const recordingStartMs = Date.parse(original.startedAt);
    const key = { readerVersion: 1, provider: 'youtube', videoId, broadcastId: videoId,
      report: 'abcdefghABCDEFGH', pullId: 1, encounter: 3492, difficulty: 5,
      recordingStartMs, startMs: recordingStartMs + 3000_000, endMs: recordingStartMs + 3200_000 };
    const library = createReplaySyncLibrary({ dataDir: f.data, now: () => 1_700_000_000_000 });
    try {
      library.enqueue([key]);
      assert.equal(library.finish(library.claim(), { videoSeconds: 2729.182,
        unixSeconds: Math.floor(key.startMs/1000), uncertaintySeconds: .125 }), true);
    } finally { library.close(); }
    const corrected = await (await f.authorized(route)).json();
    assert.equal(Date.parse(corrected.startedAt), recordingStartMs + 270818);
    assert.equal(corrected.availableSeconds, live ? original.availableSeconds - 271 : original.availableSeconds);
    const adjusted = { ...key, recordingStartMs: Date.parse(corrected.startedAt) };
    const response = await f.authorized('/v1/streams/review/sync/lookup', {
      method: 'POST', body: JSON.stringify({ keys: [adjusted] }) });
    assert.equal(response.status, 200);
    const alignment = (await response.json()).results[0].alignment;
    assert.equal(alignment.verified, true);
    assert.ok(Math.abs(alignment.videoSeconds - (adjusted.startMs-adjusted.recordingStartMs)/1000) < .001);
  }
});
