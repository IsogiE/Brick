import assert from "node:assert/strict";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import { createHash } from "node:crypto";
import { youtubeControls, twitchControls, playerScriptPolicy } from "./player_control.mjs";

function youtube({ viewport = null, paused = false, start = 19800.125, live = false } = {}) {
  const calls = []; const timers = []; let events; let status = paused ? 2 : 1; let seconds = start;
  const player = {
    mute: () => calls.push("mute"),
    loadVideoById: value => { seconds = value.startSeconds; status = 3; calls.push(["load", value.startSeconds]); },
    seekTo: (value, load) => { seconds = value; calls.push(["seek", value, load]); },
    playVideo: () => { status = 1; calls.push("play"); },
    pauseVideo: () => { status = 2; calls.push("pause"); },
    getCurrentTime: () => seconds, getPlayerState: () => status,
  };
  let sourceHeight=0;
  const frame={style:{width:"",height:"",transform:"",transformOrigin:""},src:`https://www.youtube.com/embed/abcDEF_12-3?start=${Math.floor(start)}&autoplay=${paused ? 0 : 1}`,dataset:live?{}:{start:String(start)}};
  const window = {innerWidth:viewport?.[0],innerHeight:viewport?.[1],brickReplaySourceHeight:()=>sourceHeight};
  runInNewContext(youtubeControls, {
    window, URL, Number, setTimeout: callback => timers.push(callback),
    document: {getElementById:()=>frame},
    YT: { Player: class {
      constructor(id, options) { assert.equal(id, "media"); events = options.events; return player; }
    } },
  });
  window.onYouTubeIframeAPIReady();
  return { frame, sourceHeight:value=>sourceHeight=value, media: window.brickMedia, calls, ready: () => events.onReady({ target: player }),
    timers: () => { for (const callback of timers.splice(0)) callback(); },
    status: value => { status = value; events.onStateChange({ data: value }); } };
}

function twitch({ viewport = null, qualities = ["auto","chunked","720p60","360p"], live = false, paused = false, deferredSeek = false, deferredPause = false, pauseStatusLag = false, dropSeekInPause = false, omitPauseState = false } = {}) {
  const calls = []; const listeners = new Map(); let options; let seconds = 95.125; let isPaused = true; let ended = false;
  let quality="auto", height=360;
  const seeks = [];
  const pauses = [];
  let insidePause = false;
  const emit = event => {
    insidePause = event === "pause";
    if (event === "pause" && !pauseStatusLag && !omitPauseState) isPaused = true;
    if (event === "play" || event === "playing") isPaused = false;
    if (event === "ended") ended = true;
    for (const callback of listeners.get(event) || []) callback();
    if (event === "pause" && pauseStatusLag && !omitPauseState) isPaused = true;
    insidePause = false;
  };
  const player = {
    addEventListener: (event, callback) => { listeners.set(event, [...(listeners.get(event) || []), callback]); },
    setMuted: value => calls.push(["mute", value]),
    seek: value => { assert.equal(live, false, "live must never seek"); calls.push(["seek", value]); if (dropSeekInPause && insidePause) return; seconds = value; if (deferredSeek) seeks.push("seek"); else emit("seek"); },
    play: () => { calls.push("play"); if (isPaused) emit("play"); },
    pause: () => { calls.push("pause"); if (!isPaused) { if (deferredPause) pauses.push("pause"); else emit("pause"); } },
    getQuality: () => quality,
    setQuality: value => { quality=value; calls.push(["quality",value]); },
    getQualities: () => qualities,
    getPlaybackStats: () => ({videoResolution:`1920x${height}`,bufferSize:10,fps:60}),
    getCurrentTime: () => { assert.equal(live, false, "live must not read VOD time"); return seconds; }, isPaused: () => isPaused, getEnded: () => ended,
  };
  class Player {
    static PLAYBACK_BLOCKED = "blocked"; static READY = "ready"; static PLAY = "play"; static SEEK = "seek";
    static PAUSE = "pause"; static ENDED = "ended"; static PLAYING = "playing";
    constructor(id, args) { assert.equal(id, "media"); options = args; return player; }
  }
  const container={style:{},dataset:{src:`https://player.twitch.tv/?${live ? "channel=vspeed" : "video=v123"}&parent=brick.example&time=${live ? 0 : 95.125}s&autoplay=${!paused}`}};
  let resized;
  const window = {innerWidth:viewport?.[0],innerHeight:viewport?.[1],addEventListener:(_,callback)=>resized=callback};
  runInNewContext(twitchControls, {
    window, URL, Number, Twitch: { Player },
    document: {getElementById:()=>container},
  });
  return { container, resize:(w,h)=>{window.innerWidth=w;window.innerHeight=h;resized();}, media: window.brickMedia, calls, options, emit, qualityHeight:value=>height=value, ready: () => emit("ready"),
    deliverSeeks: () => { for (const event of seeks.splice(0)) emit(event); },
    deliverPauses: () => { for (const event of pauses.splice(0)) emit(event); },
    pausedSnapshot: () => { isPaused = true; },
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
  assert.deepEqual(f.calls, [["mute", true], "play"]);
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
  assert.deepEqual(f.calls.slice(before), ["pause"], "PAUSE callback must not issue a seek");
  f.media.state();
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
  assert.deepEqual(f.calls.slice(before), ["pause"]);
  f.media.state();
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
  assert.deepEqual(f.calls.slice(before), ["pause"]);
  f.media.state();
  assert.deepEqual(f.calls.slice(before), ["pause", ["seek", 12.375]]);
  assert.equal(f.media.state().seconds, 12.375);
  assert.equal(f.media.state().buffering, false);
});

test("Twitch forward seek waits outside the provider PAUSE transition", () => {
  const f = twitch({ deferredPause: true, dropSeekInPause: true });
  f.ready(); f.emit("playing");
  const before = f.calls.length;
  f.media.seek(30087.125, false);
  f.deliverPauses();
  assert.deepEqual(f.calls.slice(before), ["pause"]);
  assert.equal(f.media.state().seconds, 30087.125);
  assert.equal(f.media.state().buffering, false);
  for (let i = 0; i < 10; i++) f.media.state();
  assert.deepEqual(f.calls.slice(before), ["pause", ["seek", 30087.125]], "polling flushes the seek once");
});

test("Twitch missed PAUSE event cannot strand later native play or seek", () => {
  const f = twitch({ deferredPause: true });
  f.ready(); f.emit("playing");
  const before = f.calls.length;
  f.media.seek(30087.125, false);
  f.media.seek(30123.375, false);
  f.media.play();
  f.pausedSnapshot(); // Fresh SDK state, but no PAUSE callback was delivered.
  f.media.state();
  assert.deepEqual(f.calls.slice(before), ["pause", ["seek", 30123.375], "play"]);
  f.emit("playing");
  assert.equal(f.media.state().playing, true);
  assert.equal(f.media.state().seconds, 30123.375);
});

test("Twitch exposes PAUSE intent before cached paused state without claiming settlement", () => {
  const f = twitch({ omitPauseState: true });
  f.ready(); f.emit("playing");
  f.emit("pause");
  const pending = f.media.state();
  assert.equal(pending.pause_intent, true);
  assert.equal(pending.playing, false);
  assert.equal(pending.buffering, true, "event intent must not fake a settled SDK pause");
  f.pausedSnapshot();
  assert.equal(f.media.state().pause_intent, true);
  assert.equal(f.media.state().buffering, false);
  f.emit("play");
  assert.equal(f.media.state().pause_intent, false);
  f.emit("pause"); f.emit("playing");
  assert.equal(f.media.state().pause_intent, false);
});

test("Twitch native commands clear an earlier provider pause intent", () => {
  for (const command of [media => media.seek(20.125, false), media => media.pause(), media => media.play()]) {
    const f = twitch();
    f.ready(); f.emit("playing"); f.emit("pause");
    assert.equal(f.media.state().pause_intent, true);
    command(f.media);
    assert.equal(f.media.state().pause_intent, false);
  }
});

test("Twitch suppresses native PAUSE events without masking later provider gestures", () => {
  const f = twitch();
  f.ready(); f.emit("playing");
  f.media.pause();
  assert.equal(f.media.state().pause_intent, false, "native Pause is not a provider gesture");
  f.media.pause(); // Already paused: the SDK emits no second PAUSE.
  f.emit("play"); f.emit("playing"); f.emit("pause");
  assert.equal(f.media.state().pause_intent, true, "redundant native Pause must not swallow the next real gesture");
  const priming = twitch({ paused: true });
  priming.ready(); priming.emit("playing");
  assert.equal(priming.media.state().pause_intent, false, "initial muted frame decoding pauses internally");
});

test("Twitch retains native pause provenance until its delayed event after a Play submission", () => {
  const f = twitch({ deferredPause: true });
  f.ready(); f.emit("playing");
  f.media.pause();
  f.media.play(); // SDK has not announced PLAY yet; the prior PAUSE is still ours.
  f.deliverPauses();
  assert.equal(f.media.state().pause_intent, false);
  f.emit("play"); f.emit("playing"); f.emit("pause");
  assert.equal(f.media.state().pause_intent, true, "a user Pause after actual playback supersedes native Play");
});

test("Twitch distinguishes provider Play during paused seek from native playback", () => {
  const f = twitch({ paused: true });
  f.ready();
  assert.equal(f.media.state().play_intent, false, "muted initial decoding is not a user action");
  f.emit("playing");
  f.media.seek(120.625, false);
  f.emit("play");
  assert.equal(f.media.state().play_intent, true);
  assert.equal(f.media.state().playing, false, "intent must not claim decoded playback");
  assert.equal(f.media.state().buffering, true);
  f.emit("playing");
  assert.equal(f.media.state().play_intent, true, "retain the gesture until the native poll sees it");
  assert.equal(f.media.state().playing, true);
  f.emit("pause");
  assert.equal(f.media.state().play_intent, false);
  f.media.play(); f.emit("playing");
  assert.equal(f.media.state().play_intent, false, "native Play is not a user gesture");
  f.media.seek(150.875, true);
  assert.equal(f.media.state().play_intent, false);
});

test("Twitch clears provider Play intent on replacement native commands and end", () => {
  for (const command of [media => media.seek(20.125, false), media => media.pause(), media => media.play()]) {
    const f = twitch({ paused: true });
    f.ready(); f.emit("playing"); f.emit("play");
    assert.equal(f.media.state().play_intent, true);
    command(f.media);
    assert.equal(f.media.state().play_intent, false);
  }
  const f = twitch({ paused: true });
  f.ready(); f.emit("playing"); f.emit("play"); f.emit("ended");
  assert.equal(f.media.state().play_intent, false);
  const pendingPause = twitch({ deferredPause: true });
  pendingPause.ready(); pendingPause.emit("playing");
  pendingPause.media.seek(20.125, false);
  pendingPause.media.play();
  pendingPause.deliverPauses(); pendingPause.media.state();
  assert.equal(pendingPause.media.state().play_intent, false, "a queued native Play remains native");
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


test("Twitch live uses the SDK without VOD timing calls", () => {
  const f = twitch({live:true});
  assert.equal(f.options.channel, "vspeed");
  assert.equal(f.options.video, undefined);
  assert.equal(f.options.time, undefined);
  f.ready(); f.emit("blocked");
  assert.equal(f.media.state().blocked, true);
  f.media.seek(100, true);
  f.emit("playing");
  assert.equal(f.media.state().playing, true);
  assert.equal(f.media.state().seconds, 0);
  f.media.pause(); f.media.play(); f.emit("playing");
  assert.equal(f.media.state().blocked, false);
});

test("Twitch blocked startup waits for real playback and preserves a paused target", () => {
  for (const paused of [false, true]) {
    const f = twitch({paused, deferredPause:true});
    f.ready(); f.emit("blocked"); f.pausedSnapshot();
    const before = f.calls.length;
    f.media.seek(111.625, !paused);
    for (let n=0;n<5;n++) f.media.state();
    assert.equal(f.calls.length, before, "no seek/play loop while provider needs interaction");
    f.emit("pause");
    assert.equal(f.media.state().pause_intent, false);
    f.emit("play"); f.advance(2);
    assert.equal(f.media.state().blocked, true);
    assert.equal(f.media.state().playing, false);
    f.emit("playing");
    assert.equal(f.media.state().blocked, false);
    if (paused) {
      assert.equal(f.media.state().buffering, true);
      f.deliverPauses();
      assert.equal(f.media.state().buffering, false);
      assert.equal(f.media.state().playing, false);
      assert.equal(f.media.state().seconds,111.625);
    } else assert.equal(f.media.state().playing,true);
  }
});


test("Twitch marker scan waits for decoded source quality and restores the viewer quality",()=>{
  const f=twitch();f.ready();f.emit("playing");
  f.media.prepareSync(true);
  assert.equal(f.media.state().sync_ready,false);
  assert.deepEqual(f.calls.at(-1),["quality","chunked"]);
  const count=f.calls.length;f.media.state();assert.equal(f.calls.length,count);
  f.qualityHeight(1080);assert.equal(f.media.state().sync_ready,true);
  f.media.prepareSync(false);assert.deepEqual(f.calls.at(-1),["quality","auto"]);
  assert.equal(f.media.state().sync_ready,true);
});

test("Twitch startup does not seek before decoded playback, including a newer blocked target",()=>{
  const f=twitch();f.ready();f.emit("blocked");f.media.seek(234.625,true);
  assert.equal(f.calls.some(c=>Array.isArray(c)&&c[0]==="seek"),false);
  f.emit("playing");assert.deepEqual(f.calls.at(-1),["seek",234.625]);
  f.media.seek(240.0,true);f.emit("blocked");f.media.seek(250.5,false);f.emit("playing");
  assert.equal(f.media.state().playing,false);
  assert.equal(f.media.state().seconds,250.5);
});


test("Twitch source selection accepts current SDK objects and requests their group",()=>{
  const f=twitch({qualities:[{name:"Auto",group:"auto"},{name:"480p",group:"480p30"},{name:"1080p60",group:"1080p60"}]});
  f.ready();f.emit("playing");f.media.prepareSync(true);assert.equal(f.media.state().sync_ready,false);
  assert.deepEqual(f.calls.at(-1),["quality","1080p60"]);
  f.qualityHeight(1080);assert.equal(f.media.state().sync_ready,true);
  f.media.prepareSync(false);assert.deepEqual(f.calls.at(-1),["quality","auto"]);
});


test("compact Twitch panes keep the entire supported provider viewport visible",()=>{
  const f=twitch({viewport:[300,180]});
  assert.equal(f.container.style.width,"500px");assert.equal(f.container.style.height,"300px");assert.equal(f.container.style.transform,"scale(0.6)");
  f.resize(900,500);assert.equal(f.container.style.width,"900px");assert.equal(f.container.style.height,"500px");assert.equal(f.container.style.transform,"");
});


test("YouTube scan expands its internal viewport and waits for readable decoded pixels",()=>{
  const f=youtube({viewport:[800,450]});f.ready();f.status(1);f.media.prepareSync(true);
  assert.equal(f.frame.style.width,"1920px");assert.equal(f.frame.style.height,"1080px");
  assert.equal(f.media.state().sync_ready,false);f.sourceHeight(1080);assert.equal(f.media.state().sync_ready,true);
  f.media.prepareSync(false);assert.equal(f.frame.style.width,"");assert.equal(f.frame.style.transform,"");
});

test("native Play can retry a blocked provider without pretending playback resumed", () => {
  const f = twitch(); f.ready(); f.emit("blocked");
  const before = f.calls.length;
  f.media.play();
  assert.deepEqual(f.calls.slice(before), ["play"]);
  assert.equal(f.media.state().blocked, true);
  assert.equal(f.media.state().playing, false);
  for (let n=0;n<10;n++) f.media.state();
  assert.equal(f.calls.length, before+1);
  f.emit("playing");assert.equal(f.media.state().blocked, false);
});

test("Play recovers a blocked in-flight pause and retains its requested VOD position", () => {
  const f=twitch({deferredPause:true});f.ready();f.emit("playing");
  f.media.seek(130.625,false);f.emit("blocked");
  const before=f.calls.length;f.media.play();
  assert.deepEqual(f.calls.slice(before),["play"]);
  f.emit("playing");
  assert.equal(f.media.state().blocked,false);
  assert.equal(f.media.state().playing,true);
  assert.equal(f.media.state().seconds,130.625);
});
