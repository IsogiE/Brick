import assert from "node:assert/strict";
import { test } from "node:test";
import { createReplayService, createRecordingReplayService, createLogsHandoff, videoDuration, youtubeDuration } from "./stream_review.mjs";
import { streamPlayerPage } from "./streams.mjs";

test("late discovery uses the original broadcast and only its exact archive", async () => {
  const startedAt = "2026-09-08T12:00:00Z";
  let clock = Date.parse(startedAt) + 5.5 * 3600_000;
  let calls = 0;
  const state = { status: "live", startedAt, streamId: "123", twitchUserId: "42" };
  const replay = createReplayService({ now: () => clock, twitchRequest: async () => {
    calls++;
    return { data: [
      { id: "900", stream_id: "122", user_id: "42", type: "archive", duration: "6h" },
      { id: "901", stream_id: "123", user_id: "43", type: "archive", duration: "6h" },
      { id: "902", stream_id: "123", user_id: "42", type: "highlight", duration: "6h" },
      { id: "903", stream_id: "123", user_id: "42", type: "archive", duration: "5h29m40s" },
    ] };
  } });
  const results = await Promise.all(Array.from({ length: 20 }, () => replay({ provider: "twitch" }, state)));
  assert.equal(calls, 1);
  assert.equal(results[0].videoId, "903");
  assert.equal(Date.parse(results[0].startedAt), Date.parse(startedAt));
  assert.equal(results[0].availableSeconds, 19780);
  clock += 30_000;
  await replay({ provider: "twitch" }, state);
  assert.equal(calls, 2);
  await assert.rejects(replay({ provider: "twitch" }, { ...state, streamId: "124" }), /not made/);
});

test("missing archives and upstream failures are cached without inventing a playable link", async () => {
  let calls = 0;
  const replay = createReplayService({ now: () => 1_700_001_000_000, twitchRequest: async () => { calls++; throw new Error("secret upstream URL"); } });
  const state = { status: "live", startedAt: "2023-11-14T22:13:20Z", streamId: "123", twitchUserId: "42" };
  for (let i = 0; i < 3; i++) await assert.rejects(replay({ provider: "twitch" }, state), error => error.status === 502 && !error.message.includes("secret"));
  assert.equal(calls, 1);
  for (const bad of [{}, { ...state, status: "unknown" }, { ...state, startedAt: "bad" }, { ...state, streamId: "../bad" }]) {
    await assert.rejects(replay({ provider: "twitch" }, bad));
  }
});

test("YouTube replay retains the broadcast's original start when added after raid starts", async () => {
  const startedAt = "2026-09-08T12:00:00Z";
  const replay = createReplayService({ now: () => Date.parse(startedAt) + 5.5 * 3600_000, twitchRequest: () => assert.fail() });
  const result = await replay({ provider: "youtube", channelId: "abcDEF_12-3" }, { status: "live", startedAt });
  assert.equal(Date.parse(result.startedAt), Date.parse(startedAt));
  assert.equal(result.videoId, "abcDEF_12-3");
  assert.equal(result.availableSeconds, 19770);
});

test("duration parsing rejects malformed, empty and unbounded provider values", () => {
  assert.equal(videoDuration("5h30m2s"), 19802);
  assert.equal(videoDuration("120s"), 120);
  for (const value of [null, "", "0s", "-1s", "1.5h", "200h", "1hsecret", "999999999999999h"]) assert.equal(videoDuration(value), null);
});

test("PKCE handoffs are bounded, expire, and cannot be replayed or mixed with errors", () => {
  let clock = 1000;
  const handoff = createLogsHandoff({ now: () => clock });
  const state = "a".repeat(64);
  assert.equal(handoff.take(state), null);
  handoff.receive(new URLSearchParams({ state, code: "private-code" }));
  assert.deepEqual(handoff.take(state), { code: "private-code" });
  assert.equal(handoff.take(state), null);
  assert.throws(() => handoff.receive(new URLSearchParams({ state, code: "replayed" })), /already received/);
  clock += 180_000;
  handoff.receive(new URLSearchParams({ state, error: "<script>secret</script>", code: "ignored" }));
  assert.deepEqual(handoff.take(state), { error: "Warcraft Logs sign-in was cancelled." });
  for (const params of [new URLSearchParams({ state: "short", code: "x" }), new URLSearchParams(`state=${state}&state=${state}&code=x`), new URLSearchParams({ state: "b".repeat(64), code: "bad\ncode" })]) assert.throws(() => handoff.receive(params));
});

test("replay embeds address the VOD and selected timestamp instead of the live channel", () => {
  const origin = new URL("https://brick.example.com");
  const twitch = streamPlayerPage({ provider: "twitch", channelId: "alice" }, origin, { videoId: "12345", seconds: 19800 });
  assert.match(twitch, /video=v12345/); assert.match(twitch, /time=19800s/); assert.doesNotMatch(twitch, /channel=alice/);
  const youtube = streamPlayerPage({ provider: "youtube", channelId: "abcDEF_12-3" }, origin, { videoId: "abcDEF_12-3", seconds: 19800 });
  assert.match(youtube, /embed\/abcDEF_12-3/); assert.match(youtube, /start=19800/);
  const pausedYoutube = streamPlayerPage({ provider: "youtube", channelId: "abcDEF_12-3" }, origin, { videoId: "abcDEF_12-3", seconds: 19800.875, paused: true });
  assert.match(pausedYoutube, /autoplay=0/);
  assert.match(pausedYoutube, /start=19800(?:&|\")/);
  assert.match(pausedYoutube, /data-start="19800.875"/);
  const pausedTwitch = streamPlayerPage({ provider: "twitch", channelId: "alice" }, origin, { videoId: "12345", seconds: 19800.875, paused: true });
  assert.match(pausedTwitch, /autoplay=false/); assert.match(pausedTwitch, /time=19800.875s/);
});

test("saved YouTube uses the exact video's media duration after the channel goes offline", async () => {
  let calls = 0;
  const record = { provider: "youtube", id: "abcDEF_12-3", startedAt: "2026-09-08T12:00:00Z" };
  const replay = createRecordingReplayService({ now: () => Date.parse("2026-09-09T12:00:00Z"),
    twitchRequest: () => assert.fail(), youtubeRequest: async id => {
      calls++; assert.equal(id, record.id);
      return { items: [{ id, status: { embeddable: true, privacyStatus: "public" }, contentDetails: { duration: "PT3H9M37.483S" },
        liveStreamingDetails: { actualStartTime: record.startedAt, actualEndTime: "2026-09-08T16:00:00Z" } }] };
    } });
  const results = await Promise.all(Array.from({ length: 12 }, () => replay(record)));
  assert.equal(calls, 1);
  assert.equal(results[0].availableSeconds, 11377);
  assert.equal(results[0].videoId, record.id);
  assert.equal(Date.parse(results[0].startedAt), Date.parse(record.startedAt));
  for (const invalid of [null, "PT", "P", "PT0S", "PT999999H", "PT1Ssecret", "PT-1S"]) assert.equal(youtubeDuration(invalid), null);
  assert.equal(youtubeDuration("P1DT1H"), 90000);
});

test("saved Twitch validates broadcast ownership and never treats archive creation as broadcast start", async () => {
  const record = { provider: "twitch", id: "900", broadcastId: "123", twitchUserId: "42", startedAt: "2026-09-08T08:00:00Z" };
  const replay = createRecordingReplayService({ now: () => Date.parse("2026-09-09T12:00:00Z"),
    youtubeRequest: () => assert.fail(), twitchRequest: async route => {
      assert.equal(route, "/videos?id=900");
      return { data: [{ id: "900", stream_id: "123", user_id: "42", type: "archive", duration: "12h40m43s", created_at: "2026-09-08T08:00:05Z" }] };
    } });
  assert.equal((await replay(record)).startedAt, "2026-09-08T08:00:00.000Z");
  assert.equal((await replay(record)).availableSeconds, 45643);
  await assert.rejects(replay({ ...record, broadcastId: "124" }), { status: 410 });
  await assert.rejects(replay({ ...record, twitchUserId: "43" }), { status: 410 });
  await assert.rejects(replay({ ...record, startedAt: undefined }), { status: 409 });
});

test("saved replay rejects missing, private and disabled videos without exposing provider errors", async () => {
  for (const items of [[], [{ id: "abcDEF_12-3" }], [{ id: "abcDEF_12-3", status: { privacyStatus: "private" } }], [{ id: "abcDEF_12-3", status: { embeddable: false } }]]) {
    const replay = createRecordingReplayService({ youtubeRequest: async () => ({ items }) });
    await assert.rejects(replay({ provider: "youtube", id: "abcDEF_12-3" }), { status: 410 });
  }
  const replay = createRecordingReplayService({ youtubeRequest: async () => { throw new Error("private key in upstream URL"); } });
  await assert.rejects(replay({ provider: "youtube", id: "abcDEF_12-3" }), error => error.status === 502 && !error.message.includes("private"));
});

test("saved recording metadata limits concurrent provider work and coalesces duplicate requests", async () => {
  const releases = [];
  let calls = 0;
  const replay = createRecordingReplayService({ youtubeRequest: () => {
    calls++;
    return new Promise(resolve => releases.push(() => resolve({ items: [] })));
  } });
  const records = Array.from({ length: 4 }, (_, index) => ({ provider: "youtube", id: String(index).padStart(11, "0") }));
  const requests = records.slice(0, 3).map(record => assert.rejects(replay(record), { status: 410 }));
  requests.push(assert.rejects(replay(records[0]), { status: 410 }));
  await assert.rejects(replay(records[3]), { status: 503 });
  assert.equal(calls, 3);
  releases.forEach(release => release());
  await Promise.all(requests);
  const next = assert.rejects(replay(records[3]), { status: 410 });
  releases[3]();
  await next;
});
