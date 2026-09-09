import assert from "node:assert/strict";
import { test } from "node:test";
import { createReplayService, createLogsHandoff, videoDuration } from "./stream_review.mjs";
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
