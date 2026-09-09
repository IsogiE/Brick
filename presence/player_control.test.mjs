import assert from "node:assert/strict";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import { createHash } from "node:crypto";
import { youtubeControls, twitchControls, playerScriptPolicy } from "./player_control.mjs";

function youtube({ paused = false, start = 19800.125, live = false } = {}) {
  const calls = []; const timers = []; let events; let status = paused ? 2 : 1; let seconds = start;
  const player = {
    mute: () => calls.push("mute"),
    loadVideoById: value => { seconds = value.startSeconds; status = 3; calls.push(["load", value.startSeconds]); },
    seekTo: (value, load) => { seconds = value; calls.push(["seek", value, load]); },
    playVideo: () => { status = 1; calls.push("play"); },
    pauseVideo: () => { status = 2; calls.push("pause"); },
    getCurrentTime: () => seconds, getPlayerState: () => status,
  };
  const window = {};
  runInNewContext(youtubeControls, {
    window, URL, Number, setTimeout: callback => timers.push(callback),
    document: { getElementById: () => ({
      src: `https://www.youtube.com/embed/abcDEF_12-3?start=${Math.floor(start)}&autoplay=${paused ? 0 : 1}`,
      dataset: live ? {} : { start: String(start) },
    }) },
    YT: { Player: class {
      constructor(id, options) { assert.equal(id, "media"); events = options.events; return player; }
    } },
  });
  window.onYouTubeIframeAPIReady();
  return { media: window.brickMedia, calls, ready: () => events.onReady({ target: player }),
    timers: () => { for (const callback of timers.splice(0)) callback(); },
    status: value => { status = value; events.onStateChange({ data: value }); } };
}

function twitch({ paused = false, deferredSeek = false, deferredPause = false, pauseStatusLag = false } = {}) {
  const calls = []; const listeners = new Map(); let options; let seconds = 95.125; let isPaused = true; let ended = false;
  const seeks = [];
  const pauses = [];
  const emit = event => {
    if (event === "pause" && !pauseStatusLag) isPaused = true;
    if (event === "play" || event === "playing") isPaused = false;
    if (event === "ended") ended = true;
    for (const callback of listeners.get(event) || []) callback();
    if (event === "pause" && pauseStatusLag) isPaused = true;
  };
  const player = {
    addEventListener: (event, callback) => { listeners.set(event, [...(listeners.get(event) || []), callback]); },
    setMuted: value => calls.push(["mute", value]),
    seek: value => { seconds = value; calls.push(["seek", value]); if (deferredSeek) seeks.push("seek"); else emit("seek"); },
    play: () => { calls.push("play"); if (isPaused) emit("play"); },
    pause: () => { calls.push("pause"); if (!isPaused) { if (deferredPause) pauses.push("pause"); else emit("pause"); } },
    getCurrentTime: () => seconds, isPaused: () => isPaused, getEnded: () => ended,
  };
  class Player {
    static READY = "ready"; static PLAY = "play"; static SEEK = "seek";
    static PAUSE = "pause"; static ENDED = "ended"; static PLAYING = "playing";
    constructor(id, args) { assert.equal(id, "media"); options = args; return player; }
  }
  const window = {};
  runInNewContext(twitchControls, {
    window, URL, Number, Twitch: { Player },
    document: { getElementById: () => ({ dataset: {
      src: `https://player.twitch.tv/?video=v123&parent=brick.example&time=95.125s&autoplay=${!paused}`,
    } }) },
  });
  return { media: window.brickMedia, calls, options, emit, ready: () => emit("ready"),
    deliverSeeks: () => { for (const event of seeks.splice(0)) emit(event); },
    deliverPauses: () => { for (const event of pauses.splice(0)) emit(event); },
    advance: value => { seconds += value; } };
}

test("YouTube keeps milliseconds and distinguishes paused seeks from resumed seeks", () => {
  const f = youtube();
  assert.equal(f.media.state().ready, false);
  f.ready();
  assert.deepEqual(f.calls, ["mute", ["load", 19800.125]]);
  f.status(1);
  f.media.seek(19900.875, false);
  assert.equal(f.media.state().seconds, 19900.875);
  assert.equal(f.media.state().playing, false);
  f.media.seek(19901.25, true);
  assert.equal(f.media.state().seconds, 19901.25);
  assert.equal(f.media.state().playing, true);
  f.status(3);
  assert.equal(f.media.state().buffering, true);
  assert.equal(f.media.state().playing, false);
});

test("YouTube live streams keep the live player instead of loading a replay position", () => {
  const f = youtube({ live: true });
  f.ready(); f.timers();
  assert.deepEqual(f.calls, ["mute", "play"]);
});

test("YouTube paused entry decodes a muted frame before acknowledging the precise paused target", () => {
  const f = youtube({ paused: true });
  f.media.seek(120.625, false);
  f.ready();
  assert.equal(f.media.state().seconds, 120.625);
  assert.equal(f.media.state().playing, false);
  assert.equal(f.media.state().buffering, true);
  assert.equal(f.calls[0], "mute");
  f.timers();
  f.media.seek(130.875, false);
  f.status(1); // Provider actually started decoding the requested frame.
  assert.equal(f.media.state().buffering, true);
  f.status(2); // Pause acknowledged: precise seek is now safe while paused.
  assert.equal(f.media.state().buffering, false);
  assert.equal(f.media.state().seconds, 130.875);
  assert.equal(f.media.state().playing, false);
  assert.deepEqual(f.calls.at(-1), ["seek", 130.875, true]);
  f.media.play();
  assert.equal(f.media.state().playing, true);
  const initial = youtube();
  initial.media.pause(); initial.ready();
  assert.equal(initial.media.state().buffering, true);
  initial.status(1); initial.status(2);
  assert.equal(initial.media.state().playing, false);
  const count = initial.calls.length;
  initial.timers();
  assert.equal(initial.calls.length, count, "finished preparation must not receive a stale retry");
});

test("Twitch buffers until playback starts and preserves precise paused seek targets", () => {
  const f = twitch();
  assert.equal(f.options.video, "v123"); assert.equal(f.options.parent[0], "brick.example");
  f.media.seek(100.375, true); assert.equal(f.calls.length, 0);
  f.ready();
  assert.deepEqual(f.calls, [["mute", true], ["seek", 100.375], "play"]);
  assert.equal(f.media.state().buffering, true);
  assert.equal(f.media.state().playing, false);
  f.emit("playing"); assert.equal(f.media.state().playing, true);
  f.media.seek(130.875, false);
  assert.equal(f.media.state().seconds, 130.875);
  assert.equal(f.media.state().playing, false);
  assert.equal(f.media.state().buffering, false);
  f.media.seek(150.25, true); f.emit("playing");
  assert.equal(f.media.state().playing, true);
});

test("Twitch keeps affirmative playback across startup and playing seeks without a second PLAYING event", () => {
  for (const deferredSeek of [false, true]) {
    const f = twitch({ deferredSeek });
    f.ready();
    f.advance(20);
    assert.equal(f.media.state().playing, false, "SDK clock movement does not establish playback");
    assert.equal(f.media.state().buffering, true);
    f.emit("playing");
    f.deliverSeeks();
    assert.equal(f.media.state().playing, true);
    assert.equal(f.media.state().buffering, false);
    f.media.seek(200.625, true);
    f.deliverSeeks();
    assert.equal(f.media.state().seconds, 200.625);
    assert.equal(f.media.state().playing, true);
    assert.equal(f.media.state().buffering, false);
    f.emit("pause");
    assert.equal(f.media.state().playing, false);
    f.media.seek(300.875, true);
    f.deliverSeeks();
    f.advance(10);
    assert.equal(f.media.state().playing, false, "unpausing still awaits affirmative PLAYING");
    assert.equal(f.media.state().buffering, true);
    f.emit("playing");
    assert.equal(f.media.state().playing, true);
    f.emit("ended");
    assert.equal(f.media.state().playing, false);
    assert.equal(f.media.state().buffering, false);
  }
});

test("Twitch paused entry decodes a muted frame then seeks with its paused intent", () => {
  const f = twitch({ paused: true });
  assert.equal(f.options.autoplay, false);
  f.ready();
  assert.equal(f.media.state().seconds, 95.125);
  assert.equal(f.media.state().playing, false);
  assert.equal(f.media.state().buffering, true);
  assert.deepEqual(f.calls[0], ["mute", true]);
  f.emit("playing");
  assert.equal(f.media.state().buffering, false);
  assert.equal(f.media.state().playing, false);
  assert.deepEqual(f.calls.at(-1), ["seek", 95.125]);
  const queued = twitch();
  queued.media.pause(); queued.ready();
  queued.emit("playing");
  assert.equal(queued.media.state().playing, false);
});

test("Twitch seeks an already paused video without redundant pause commands", () => {
  const f = twitch({ paused: true });
  f.ready(); f.emit("playing");
  const before = f.calls.length;
  f.media.seek(12.375, false);
  assert.deepEqual(f.calls.slice(before), [["seek", 12.375]]);
  assert.equal(f.media.state().seconds, 12.375);
  assert.equal(f.media.state().playing, false);
  assert.equal(f.media.state().buffering, false);
});

test("Twitch waits for asynchronous pause before seeking and applies the newest target", () => {
  const f = twitch({ deferredPause: true });
  f.ready(); f.emit("playing");
  const before = f.calls.length;
  f.media.seek(12.375, false);
  f.media.seek(20.625, false);
  f.media.pause();
  assert.deepEqual(f.calls.slice(before), ["pause"]);
  assert.equal(f.media.state().buffering, true);
  assert.equal(f.media.state().playing, false);
  f.deliverPauses();
  assert.deepEqual(f.calls.slice(before), ["pause", ["seek", 20.625]]);
  assert.equal(f.media.state().seconds, 20.625);
  assert.equal(f.media.state().buffering, false);
});

test("Twitch resume during an outstanding pause preserves the requested seek", () => {
  const f = twitch({ deferredPause: true });
  f.ready(); f.emit("playing");
  const before = f.calls.length;
  f.media.seek(12.375, false);
  f.media.play();
  assert.deepEqual(f.calls.slice(before), ["pause"]);
  f.deliverPauses();
  assert.deepEqual(f.calls.slice(before), ["pause", ["seek", 12.375], "play"]);
  f.emit("playing");
  assert.equal(f.media.state().seconds, 12.375);
  assert.equal(f.media.state().playing, true);
});

test("Twitch PAUSE acknowledgement can precede the SDK's cached paused status", () => {
  const f = twitch({ deferredPause: true, pauseStatusLag: true });
  f.ready(); f.emit("playing");
  const before = f.calls.length;
  f.media.seek(12.375, false);
  f.deliverPauses();
  assert.deepEqual(f.calls.slice(before), ["pause", ["seek", 12.375]]);
  assert.equal(f.media.state().seconds, 12.375);
  assert.equal(f.media.state().buffering, false);
});

test("both provider controls reject invalid seek values and resume flags before changing state", () => {
  for (const create of [youtube, twitch]) {
    const f = create(); f.ready();
    const count = f.calls.length;
    const seconds = f.media.state().seconds;
    for (const invalid of [-1, NaN, Infinity, -Infinity, 604800.001, "100", null]) f.media.seek(invalid, true);
    for (const invalid of [0, 1, "false", null]) f.media.seek(100, invalid);
    assert.equal(f.calls.length, count);
    assert.equal(f.media.state().seconds, seconds);
  }
});

test("player script policy authorizes the exact controls without arbitrary inline scripts", () => {
  for (const script of [youtubeControls, twitchControls]) assert.ok(playerScriptPolicy.includes(`'sha256-${createHash("sha256").update(script).digest("base64")}'`));
  assert.doesNotMatch(playerScriptPolicy, /unsafe-inline|unsafe-eval|https:\/\*|https:\/\/\*|script-src \*/);
});
