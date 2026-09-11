import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const origin = 'https://brick.example';
const script = (await readFile(new URL('../src/stream_player/clock.js', import.meta.url), 'utf8'))
  .replaceAll('__BRICK_WRAPPER_ORIGIN__', JSON.stringify(origin));

function provider(providerOrigin) {
  const listeners = new Map();
  const sent = [];
  let videos = [], serial = 0;
  const parent = { postMessage: data => sent.push(data) };
  const context = vm.createContext({
    location: { origin: providerOrigin }, performance: { now: () => 1000 },
    document: { querySelectorAll: () => videos },
    addEventListener: (name, callback) => listeners.set(name, callback),
  });
  context.window = context;
  context.top = context.parent = parent;
  vm.runInContext(script, context);
  return {
    videos: value => { videos = value; },
    sample() {
      listeners.get('message')({ source: parent, origin,
        data: { type: 'brick-replay-clock-request', serial: ++serial } });
      return sent.at(-1);
    },
  };
}
function video() {
  const listeners = new Set();
  return {
    currentTime: 100, paused: false, ended: false, seeking: false, readyState: 4,
    videoWidth: 1280, videoHeight: 720, playbackRate: 1,
    getBoundingClientRect: () => ({ width: 1280, height: 720 }),
    getVideoPlaybackQuality: () => ({ totalVideoFrames: 30 }),
    addEventListener: (name, callback) => { assert.equal(name, 'seeking'); listeners.add(callback); },
    removeEventListener: (name, callback) => { assert.equal(name, 'seeking'); listeners.delete(callback); },
    seek(seconds) { this.currentTime = seconds; for (const callback of listeners) callback(); },
    listenerCount: () => listeners.size,
  };
}
for (const providerOrigin of ['https://player.twitch.tv', 'https://www.youtube.com', 'https://www.youtube-nocookie.com']) {
  test(`${providerOrigin}: seeks completed between clock samples remain observable`, () => {
    const p = provider(providerOrigin), media = video();
    p.videos([media]);
    assert.equal(p.sample().seekGeneration, 0);
    media.seek(700);
    const after = p.sample();
    assert.equal(after.seconds, 700);
    assert.equal(after.seeking, false);
    assert.equal(after.seekGeneration, 1);
    for (let i = 0; i < 1000; i++) p.sample();
    assert.equal(media.listenerCount(), 1);
    assert.equal(p.sample().seekGeneration, 1);
    media.seek(50);
    assert.equal(p.sample().seekGeneration, 2);
  });
}
test('replaced or ambiguous media releases seek listeners and cannot change the active clock', () => {
  const p = provider('https://www.youtube.com'), first = video(), second = video();
  p.videos([first]); p.sample(); first.seek(120);
  p.videos([second]); assert.equal(p.sample().seekGeneration, 1);
  assert.equal(first.listenerCount(), 0);
  first.seek(500);
  assert.equal(p.sample().seekGeneration, 1);
  second.seek(300);
  assert.equal(p.sample().seekGeneration, 2);
  p.videos([first, second]); p.sample();
  assert.equal(first.listenerCount(), 0);
  assert.equal(second.listenerCount(), 0);
});

test('wrapper accepts bounded seek evidence only from its current provider and request', () => {
  const listeners = new Map(), intervals = [];
  let request;
  const frame = { src: 'https://www.youtube.com/embed/fixture',
    contentWindow: { postMessage: data => { request = data; } } };
  const context = vm.createContext({ URL,
    location: { origin }, performance: { now: () => 1000 },
    document: { querySelectorAll: () => [frame] },
    addEventListener: (name, callback) => listeners.set(name, callback),
    setInterval: (callback, ms) => intervals.push({ callback, ms }),
    brickMedia: { state: () => ({ ready: true, playing: true }) },
  });
  context.window = context; context.top = context;
  vm.runInContext(script, context);
  assert.equal(intervals.length, 1);
  assert.equal(intervals[0].ms, 100);
  const receive = (seekGeneration, source = frame.contentWindow, eventOrigin = 'https://www.youtube.com') => {
    listeners.get('message')({ source, origin: eventOrigin, data: {
      type: 'brick-replay-clock-reply', serial: request.serial, seconds: 500,
      playing: true, decoded: true, rate: 1, seekGeneration,
    } });
  };
  intervals[0].callback();
  receive(77, {});
  receive(77, frame.contentWindow, 'https://untrusted.example');
  assert.equal(context.brickPlaybackState().provider_seek_generation, undefined);
  receive(4);
  assert.equal(context.brickPlaybackState().provider_seek_generation, 4);
  for (const invalid of [-1, 0x100000000, 1.5, '4', null]) {
    intervals[0].callback(); receive(invalid);
    assert.equal(context.brickPlaybackState().provider_seek_generation, null);
  }
});
