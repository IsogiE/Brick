import assert from "node:assert/strict";
import { test } from "node:test";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createPresenceServer } from "./server.mjs";
import { clientAddress, RateLimit, readBounded } from "./security.mjs";

const state = (n) => n.toString(16).padStart(64, "0");
const json = (body, status = 200) => new Response(JSON.stringify(body), { status });
async function fixture(t, upstream, extra = {}) {
  const data = await mkdtemp(path.join(os.tmpdir(), "brick-presence-test-"));
  const server = await createPresenceServer({ env: {
    DATA_DIR: data, DISCORD_BOT_TOKEN: "test-only", DISCORD_GUILD_ID: "123",
    DISCORD_OFFICER_ROLE_ID: "456", DISCORD_RAIDER_ROLE_ID: "789",
    DISCORD_GATEWAY_ENABLED: "false", STREAM_BACKGROUND_ENABLED: "false", ...extra,
  }, fetch: upstream });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  t.after(async () => {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    await rm(data, { recursive: true, force: true });
  });
  const url = `http://127.0.0.1:${server.address().port}`;
  const request = (route, options) => fetch(url + route, options);
  const authorized = (route, token = "test-token", options = {}) => request(route, {
    ...options, headers: { authorization: `Bearer ${token}`, ...options.headers },
  });
  return { request, authorized, data };
}

function discordFixture(url, options) {
  assert.equal(options.redirect, "error");
  if (url.endsWith("/users/@me")) return json({ id: "12345", username: "Tester" });
  if (url.endsWith("/member")) return json({ roles: ["789"] });
  return json([{ user: { id: "12345", username: "Tester" }, roles: ["789"] }]);
}

test("profiles require current guild membership; officers can set roles but cannot rename others", async t => {
  let officer = true;
  const members = () => [
    { user: { id: "111", username: "Officer" }, roles: officer ? ["456"] : ["789"] },
    { user: { id: "222", username: "Raider" }, roles: ["789"] },
  ];
  const f = await fixture(t, (url, options) => {
    const id = options.headers.authorization === "Bearer officer" ? "111" : "222";
    if (url.endsWith("/users/@me")) return json({ id, username: id });
    if (url.endsWith("/member")) return json({ roles: id === "111" ? ["456"] : ["789"] });
    return json(members());
  }, { PROFILE_ENCRYPTION_KEY: "be".repeat(32), ROSTER_CACHE_SECONDS: "1" });
  const put = (route, token, body) => f.authorized(route, token, { method: "PUT", body: JSON.stringify(body) });
  assert.equal((await f.request("/v1/profile/me")).status, 401);
  assert.equal((await f.authorized("/v1/profile/me", "raider", {
    method: "PUT", headers: { "x-brick-profile-user": "111" }, body: JSON.stringify({ customName: "Wrong account" }),
  })).status, 409);
  assert.equal((await put("/v1/profile/me", "raider", { customName: "Raid Name", raidRole: "healer" })).status, 200);
  assert.equal((await put("/v1/profiles/111/role", "raider", { raidRole: "tank" })).status, 403);
  assert.equal((await put("/v1/profiles/222/role", "officer", { raidRole: "tank" })).status, 200);
  assert.equal((await put("/v1/profiles/222/role", "officer", { customName: "Impersonation", raidRole: "tank" })).status, 400);
  assert.equal((await put("/v1/profiles/333/role", "officer", { raidRole: "tank" })).status, 404);
  const own = await (await f.authorized("/v1/profile/me", "raider")).json();
  assert.deepEqual(own, { customName: "Raid Name", raidRole: "tank", available: true });
  const roster = await (await f.authorized("/v1/roster", "officer")).json();
  assert.equal(roster.raiders[0].name, "Raid Name");
  assert.equal(roster.raiders[0].raidRole, "tank");
  assert.equal(roster.canEditRoles, true);
  // The OAuth identity is still cached; the current roster must revoke its
  // officer capability independently when the role is removed.
  officer = false;
  await new Promise(resolve => setTimeout(resolve, 1050));
  assert.equal((await put("/v1/profiles/222/role", "officer", { raidRole: "dps" })).status, 403);
  const encrypted = await readFile(path.join(f.data, "profiles.enc"), "utf8");
  assert(!encrypted.includes("Raid Name"));
  assert(!encrypted.includes("officer"));
});

test("authorized heartbeat and roster persist no credentials", async t => {
  let calls = 0;
  const f = await fixture(t, (...args) => { calls++; return discordFixture(...args); });
  assert.equal((await f.request("/health")).status, 200);
  assert.equal((await f.request("/v1/roster")).status, 401);
  assert.equal(calls, 0);
  const heartbeat = await f.authorized("/v1/heartbeat", "test-token", {
    method: "POST", body: JSON.stringify({ appVersion: "test", platform: "linux" }),
  });
  assert.equal(heartbeat.status, 200);
  const roster = await f.authorized("/v1/roster");
  const body = await roster.json();
  assert.equal(roster.status, 200);
  assert.equal(body.raiders[0].online, true);
  assert.equal(calls, 3);
  const saved = await readFile(path.join(f.data, "heartbeats.json"), "utf8");
  assert(!saved.includes("test-token"));
  assert(!saved.includes("test-only"));
  for (const body of ["null", "[]", '"text"']) {
    assert.equal((await f.authorized("/v1/heartbeat", "test-token", { method: "POST", body })).status, 400);
  }
});

test("unique invalid tokens cannot cause unbounded Discord fanout", async t => {
  let calls = 0;
  const releases = [];
  const f = await fixture(t, () => { calls++; return new Promise(resolve => releases.push(resolve)); });
  let completed = 0;
  const pending = Array.from({ length: 40 }, (_, i) => f.authorized("/v1/roster", `invalid-${i}`, {
    headers: { "x-brick-client-ip": `192.0.2.${i + 1}`, "x-forwarded-for": `192.0.2.${i + 1}` },
  }).then(response => { completed++; return response; }));
  // Wait until the eight allowed verifications have reached the fake upstream.
  const deadline = Date.now() + 5000;
  while (completed < 32 && Date.now() < deadline) await new Promise(r => setTimeout(r, 5));
  assert.equal(completed, 32);
  assert.equal(calls, 16);
  for (const resolve of releases) resolve(json({}, 401));
  const statuses = await Promise.all(pending.map(async p => (await p).status));
  assert(statuses.includes(429));
  assert(statuses.includes(503));
  assert(calls <= 24);
});

test("repeated invalid token uses bounded negative cache", async t => {
  let calls = 0;
  const f = await fixture(t, () => { calls++; return json({}, 401); });
  for (let i = 0; i < 20; i++) assert.equal((await f.authorized("/v1/roster", "invalid")).status, 401);
  assert.equal(calls, 2);
});

test("Discord 429 prevents immediate upstream retries", async t => {
  let calls = 0;
  const f = await fixture(t, () => { calls++; return json({ retry_after: 60 }, 429); });
  assert.equal((await f.authorized("/v1/roster", "one")).status, 429);
  const before = calls;
  assert.equal((await f.authorized("/v1/roster", "two")).status, 429);
  assert.equal(calls, before);
});

test("concurrent cold roster requests share one refresh", async t => {
  let rosterCalls = 0;
  let release;
  const f = await fixture(t, (url, options) => {
    if (url.includes("/members?")) {
      rosterCalls++;
      return new Promise(resolve => { release = () => resolve(discordFixture(url, options)); });
    }
    return discordFixture(url, options);
  });
  await f.authorized("/v1/heartbeat", "test-token", { method: "POST", body: "{}" });
  const pending = Array.from({ length: 6 }, () => f.authorized("/v1/roster"));
  for (let i = 0; i < 100 && !release; i++) await new Promise(r => setTimeout(r, 5));
  await new Promise(r => setTimeout(r, 30));
  assert.equal(rosterCalls, 1);
  release();
  for (const response of await Promise.all(pending)) assert.equal(response.status, 200);
});

test("callback flood cannot evict or replace pending login; handoffs are single use", async t => {
  const f = await fixture(t, () => { throw new Error("Unexpected upstream request"); }, { OAUTH_HANDOFF_MAX_ENTRIES: "2" });
  const callback = (n, code) => f.request(`/discord/callback?state=${state(n)}&code=${code}`);
  assert.equal((await callback(1, "legitimate")).status, 200);
  assert.equal((await callback(1, "replacement")).status, 409);
  assert.equal((await callback(2, "second")).status, 200);
  assert.equal((await callback(3, "overflow")).status, 503);
  const first = await f.request(`/v1/auth/callback?state=${state(1)}`);
  assert.equal((await first.json()).code, "legitimate");
  assert.equal((await f.request(`/v1/auth/callback?state=${state(1)}`)).status, 202);
  assert.equal((await f.request("/discord/callback?state=short&code=x")).status, 400);
  assert.equal((await f.request(`/discord/callback?state=${state(4)}`)).status, 400);
  assert.equal((await callback(4, "valid-after-invalid")).status, 200);
});

test("expired callbacks are discarded", async t => {
  const f = await fixture(t, () => {}, { OAUTH_HANDOFF_TTL_SECONDS: "1" });
  await f.request(`/discord/callback?state=${state(1)}&code=expired`);
  await new Promise(r => setTimeout(r, 1050));
  assert.equal((await f.request(`/v1/auth/callback?state=${state(1)}`)).status, 202);
});

test("callback creation is rate limited independently of polling", async t => {
  const f = await fixture(t, () => {});
  for (let i = 1; i <= 10; i++) assert.equal((await f.request(`/discord/callback?state=${state(i)}&code=x`)).status, 200);
  assert.equal((await f.request(`/discord/callback?state=${state(11)}&code=x`)).status, 429);
  assert.equal((await f.request(`/v1/auth/callback?state=${state(1)}`)).status, 200);
});

test("untrusted proxy headers are ignored and IPv6 subnet rotation shares a limit", () => {
  const request = { socket: { remoteAddress: "192.0.2.1" }, headers: { "x-brick-client-ip": "192.0.2.2" } };
  assert.equal(clientAddress(request, false), "192.0.2.1");
  assert.equal(clientAddress(request, true), "192.0.2.2");
  request.headers["x-brick-client-ip"] = "2001:db8:1234:abcd::1";
  const first = clientAddress(request, true);
  request.headers["x-brick-client-ip"] = "2001:db8:1234:abcd:ffff:1:2:3";
  assert.equal(clientAddress(request, true), first);
});

test("global budget caps distributed requests, refills, and keeps its address map bounded", () => {
  let now = 0;
  const limit = new RateLimit(3, 1, 2, 1, () => now);
  limit.take("a"); limit.take("a");
  assert.throws(() => limit.take("a"), { status: 429 });
  limit.take("b");
  assert.throws(() => limit.take("c"), { status: 429 });
  now = 1000;
  limit.take("c");
  const many = new RateLimit(5000, 1, 2, 1, () => now);
  for (let i = 0; i < 2048; i++) many.take(`${i}`);
  assert.throws(() => many.take("new"), { status: 429 });
  assert.equal(many.clients.size, 2048);
  now += 2000;
  many.take("new");
});

test("upstream response limit handles missing and dishonest content lengths", async () => {
  for (const headers of [{}, { "content-length": "1" }, { "content-length": "1000" }]) {
    await assert.rejects(readBounded(new Response("123456789", { headers }), 8), { status: 502 });
  }
  assert.equal(await readBounded(new Response("12345678"), 8), "12345678");
});
