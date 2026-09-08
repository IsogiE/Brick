import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { createVodService } from "./stream_vods.mjs";

const youtubeId = "abcDEF12345";
const startedAt = "2026-09-08T12:00:00.000Z";
const endedAt = "2026-09-08T14:00:00.000Z";
const liveYoutube = { provider: "youtube", channelId: youtubeId, owners: ["11"], status: "live", title: "Raid night", startedAt };
const liveTwitch = { provider: "twitch", channelId: "raider", owners: ["11"], status: "live", title: "Raid night", startedAt, streamId: "stream-1", twitchUserId: "123" };

async function fixture(t, provider = async () => ({ data: [] })) {
  const dataDir = await mkdtemp(path.join(os.tmpdir(), "brick-vods-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  let clock = Date.parse(endedAt);
  let calls = 0;
  const options = { dataDir, now: () => clock, twitchRequest: async (...args) => { calls++; return provider(...args); } };
  let service = createVodService(options);
  return {
    get service() { return service; }, dataDir, calls: () => calls,
    advance: milliseconds => { clock += milliseconds; },
    restart: () => { service = createVodService(options); },
  };
}

test("constructing VOD storage is lazy and empty reads have no provider or disk writes", async t => {
  const f = await fixture(t);
  assert.equal(f.calls(), 0);
  await assert.rejects(stat(path.join(f.dataDir, "stream-vods.json")), { code: "ENOENT" });
  assert.deepEqual(await f.service.list(), []);
  assert.deepEqual(await f.service.targets(), []);
  await assert.rejects(stat(path.join(f.dataDir, "stream-vods.json")), { code: "ENOENT" });
});

test("YouTube keeps tracking a removed registration and records only a verified end", async t => {
  const f = await fixture(t);
  await f.service.observe([liveYoutube]);
  f.restart();
  assert.deepEqual(await f.service.targets(), [{ provider: "youtube", channelId: youtubeId }]);
  await f.service.observe([{ ...liveYoutube, owners: [], status: "unknown" }]);
  await f.service.observe([{ ...liveYoutube, owners: [], status: "offline" }]);
  assert.deepEqual(await f.service.list(), []);
  await f.service.observe([{ ...liveYoutube, owners: [], status: "offline", endedAt }]);
  assert.deepEqual(await f.service.list(), [{
    userId: "11", provider: "youtube", id: youtubeId,
    url: `https://www.youtube.com/watch?v=${youtubeId}`, title: "Raid night", startedAt, endedAt,
  }]);
  assert.deepEqual(await f.service.targets(), []);
  await f.service.observe([{ ...liveYoutube, status: "offline", endedAt }]);
  f.restart();
  assert.equal((await f.service.list()).length, 1);
  if (process.platform !== "win32") {
    assert.equal((await stat(path.join(f.dataDir, "stream-vods.json"))).mode & 0o777, 0o600);
  }
});

test("a YouTube broadcast completed while Brick was away is saved for every registered owner", async t => {
  const f = await fixture(t);
  await f.service.observe([{ ...liveYoutube, status: "offline", owners: ["11", "22", "11", "invalid"], endedAt }]);
  assert.deepEqual((await f.service.list()).map(entry => entry.userId), ["11", "22"]);
  const copies = await f.service.list();
  copies[0].title = "changed";
  assert.equal((await f.service.list())[0].title, "Raid night");
});

test("Twitch requires exact broadcast archive, retries delayed availability, and rebuilds canonical URL", async t => {
  let available = false;
  const f = await fixture(t, async (request, signal) => {
    assert.equal(request, "/videos?user_id=123&type=archive&first=100");
    assert(signal instanceof AbortSignal);
    return { data: [
      { id: "987", stream_id: "other-stream", user_id: "123", type: "archive" },
      { id: "988", stream_id: "stream-1", user_id: "999", type: "archive" },
      { id: "989", stream_id: "stream-1", user_id: "123", type: "highlight" },
      ...(available ? [{ id: "990", stream_id: "stream-1", user_id: "123", type: "archive", title: "Saved raid", url: "https://evil.example" }] : []),
    ] };
  });
  await f.service.observe([liveTwitch]);
  await f.service.observe([{ provider: "twitch", channelId: "raider", status: "unknown", owners: [] }]);
  assert.equal(f.calls(), 0);
  await f.service.observe([{ provider: "twitch", channelId: "raider", status: "offline", owners: [] }]);
  assert.equal(f.calls(), 1);
  assert.deepEqual(await f.service.list(), []);
  f.restart();
  assert.equal((await f.service.targets()).length, 1);
  await f.service.observe([]);
  assert.equal(f.calls(), 1);
  available = true;
  f.advance(60_000);
  await f.service.observe([]);
  const [vod] = await f.service.list();
  assert.equal(vod.id, "990");
  assert.equal(vod.url, "https://www.twitch.tv/videos/990");
  assert.equal(vod.userId, "11");
  assert.equal(vod.title, "Saved raid");
  assert.deepEqual(await f.service.targets(), []);
});

test("Twitch rapid restart preserves previous broadcast while tracking the new stream", async t => {
  const f = await fixture(t, async () => ({ data: [{ id: "990", stream_id: "stream-1", user_id: "123", type: "archive" }] }));
  await f.service.observe([liveTwitch]);
  await f.service.observe([{ ...liveTwitch, streamId: "stream-2", owners: ["22"] }]);
  assert.equal((await f.service.list())[0].userId, "11");
  assert.equal((await f.service.targets()).length, 1);
});

test("provider failures preserve pending Twitch history until its bounded retry window ends", async t => {
  const f = await fixture(t, async () => { throw new Error("provider temporarily unavailable"); });
  await f.service.observe([liveTwitch]);
  await f.service.observe([{ ...liveTwitch, status: "offline" }]);
  assert.equal((await f.service.targets()).length, 1);
  f.advance(49 * 60 * 60 * 1000);
  await f.service.observe([]);
  assert.deepEqual(await f.service.targets(), []);
  assert.deepEqual(await f.service.list(), []);
});

test("both platforms and concurrent observations retain every archive without duplicates", async t => {
  const f = await fixture(t, async () => ({ data: [{ id: "990", stream_id: "stream-1", user_id: "123", type: "archive" }] }));
  await f.service.observe([liveTwitch]);
  await Promise.all([
    f.service.observe([{ ...liveTwitch, status: "offline" }]),
    f.service.observe([{ ...liveYoutube, status: "offline", endedAt }]),
    f.service.observe([{ ...liveYoutube, status: "offline", endedAt }]),
  ]);
  assert.equal((await f.service.list()).length, 2);
  f.restart();
  assert.equal((await f.service.list()).length, 2);
});

test("invalid persisted history is rejected and unavailable provider metadata cannot create a VOD", async t => {
  const f = await fixture(t);
  await f.service.observe([{ ...liveTwitch, twitchUserId: "../../secret" }, { ...liveYoutube, status: "unknown", endedAt }]);
  assert.deepEqual(await f.service.list(), []);
  assert.equal(f.calls(), 0);
  await writeFile(path.join(f.dataDir, "stream-vods.json"), JSON.stringify({ version: 1, active: [], history: [{ userId: "11", provider: "youtube", id: "../secret", title: "Invalid", endedAt }] }));
  f.restart();
  await assert.rejects(f.service.list(), /Invalid stream archive/);
  assert.equal((await readFile(path.join(f.dataDir, "stream-vods.json"), "utf8")).includes("../secret"), true);
});

test("oversized owner association growth preserves readable archive storage and in-memory state", async t => {
  const f = await fixture(t);
  const owners = Array.from({ length: 1000 }, (_, index) => String(index).padStart(20, "1"));
  const active = Array.from({ length: 1500 }, (_, index) => ({
    provider: "youtube", channelId: String(index).padStart(11, "0"), owners: owners.slice(0, 400),
    title: "", live: true, lastSeenAt: Date.parse(endedAt), nextAttemptAt: 0,
  }));
  const history = [{
    userId: "11", provider: "youtube", id: youtubeId, title: "Saved raid", startedAt, endedAt,
    url: `https://www.youtube.com/watch?v=${youtubeId}`,
  }];
  const original = `${JSON.stringify({ version: 1, active, history })}\n`;
  const archivePath = path.join(f.dataDir, "stream-vods.json");
  await writeFile(archivePath, original);
  f.restart();
  assert.deepEqual(await f.service.list(), history);
  await assert.rejects(f.service.observe(active.map(session => ({
    provider: session.provider, channelId: session.channelId, status: "live", owners,
  }))), /storage is full/);
  assert.equal(await readFile(archivePath, "utf8"), original);
  assert.deepEqual(await f.service.list(), history);
  f.restart();
  assert.deepEqual(await f.service.list(), history);
  assert.equal((await f.service.targets()).length, active.length);
});
