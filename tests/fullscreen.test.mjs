import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const script = await readFile(new URL('../src/stream_player/fullscreen.js', import.meta.url), 'utf8');
function page(origin = 'https://brick.example', nested = false) {
  const listeners = new Map();
  const native = [];
  const sent = [];
  const events = [];
  const tasks = [];
  const documentListeners = new Map();
  const dispatch = event => {
    events.push(event.type);
    for (const callback of documentListeners.get(event.type) || []) callback(event);
  };
  let now = 1000;
  class Element {
    isConnected = true;
    values = new Map();
    style = {
      getPropertyValue: name => this.values.get(name)?.[0] || '',
      getPropertyPriority: name => this.values.get(name)?.[1] || '',
      setProperty: (name, value, priority = '') => this.values.set(name, [value, priority]),
      removeProperty: name => this.values.delete(name),
    };
    dispatchEvent(event) { dispatch(event); }
  }
  const frame = new Element();
  frame.contentWindow = { postMessage: (data, origin) => sent.push({ data, origin }) };
  const document = {
    querySelectorAll: () => [frame], dispatchEvent: dispatch,
    addEventListener: (type, callback) => {
      if (!documentListeners.has(type)) documentListeners.set(type, new Set());
      documentListeners.get(type).add(callback);
    },
    removeEventListener: (type, callback) => documentListeners.get(type)?.delete(callback),
  };
  const parent = { postMessage: (data, origin) => sent.push({ data, origin }) };
  const context = vm.createContext({
    setTimeout: callback => tasks.push(callback),
    Date: { now: () => now }, Element, Event: class { constructor(type) { this.type = type; } }, document,
    location: { origin }, navigator: { userActivation: { isActive: true } },
    addEventListener: (kind, callback) => listeners.set(kind, callback),
    webkit: { messageHandlers: { brickFullscreen: { postMessage: value => native.push(value) } } },
  });
  context.window = context;
  context.top = nested ? parent : context;
  context.parent = nested ? parent : context;
  vm.runInContext(script, context);
  return { context, document, frame, parent, native, sent, events, Element,
    receive: (data, origin = 'https://player.twitch.tv', source = frame.contentWindow) => listeners.get('message')?.({ data, origin, source }),
    key: key => listeners.get('keydown')?.({ key }),
    click: isTrusted => listeners.get('click')?.({ isTrusted }),
    advance: ms => { now += ms; },
    flush: () => { while (tasks.length) tasks.shift()(); },
    unload: () => listeners.get('pagehide')?.(),
  };
}

test('repeated fullscreen entry and exit restore provider state and inline styles', async () => {
  const p = page();
  const video = new p.Element();
  video.style.setProperty('width', '75%', 'important');
  video.style.setProperty('color', 'red');
  const before = [...video.values];
  for (const exit of [() => p.document.exitFullscreen(), () => p.key('Escape'), () => p.unload()]) {
    await video.requestFullscreen();
    assert.equal(p.document.fullscreenElement, video);
    assert.equal(p.document.webkitIsFullScreen, true);
    assert.equal(p.document.fullscreen, true);
    assert.equal(video.style.getPropertyValue('width'), '100vw');
    await exit();
    assert.equal(p.document.fullscreenElement, null);
    assert.equal(p.document.fullscreen, false);
    assert.deepEqual([...video.values], before);
  }
  assert.deepEqual(p.native, ['enter', 'exit', 'enter', 'exit', 'enter', 'exit']);
});

test('provider controls can use WebKit aliases without native DOM fullscreen', async () => {
  const p = page('https://player.twitch.tv', true);
  const video = new p.Element();
  assert.equal(p.document.fullscreenEnabled, true);
  await video.webkitRequestFullScreen();
  assert.equal(p.document.webkitFullscreenElement, video);
  assert.equal(p.sent[0].data, 'brick-fullscreen-enter');
  p.receive('brick-fullscreen-exit', 'https://brick.example', p.parent);
  assert.equal(p.document.webkitCurrentFullScreenElement, null);
  assert.equal(p.sent[1].data, 'brick-fullscreen-leave');
  assert.deepEqual(p.native, []);
});

test('wrapper accepts only an official provider in its own iframe and exits that frame', async () => {
  const p = page();
  p.receive('brick-fullscreen-enter', 'https://attacker.example');
  p.receive('brick-fullscreen-enter', 'https://player.twitch.tv', {});
  p.receive({ origin: 'https://player.twitch.tv', command: 'enter' });
  assert.deepEqual(p.native, []);
  p.receive('brick-fullscreen-enter');
  p.receive('brick-fullscreen-enter');
  assert.equal(p.document.fullscreenElement, p.frame);
  assert.deepEqual(p.native, ['enter']);
  await p.document.exitFullscreen();
  assert.deepEqual(p.sent, [{ data: 'brick-fullscreen-exit', origin: 'https://player.twitch.tv' }]);
  assert.equal(p.document.fullscreenElement, null);
  p.receive('brick-fullscreen-leave');
  assert.deepEqual(p.native, ['enter', 'exit']);
});

test('fullscreen requires a connected element and active user input', async () => {
  const p = page();
  const video = new p.Element();
  p.context.navigator.userActivation.isActive = false;
  await assert.rejects(video.requestFullscreen(), /user gesture/);
  p.context.navigator.userActivation.isActive = true;
  video.isConnected = false;
  await assert.rejects(video.requestFullscreen(), /user gesture/);
  assert.deepEqual(p.native, []);
});

test('unrelated subframes do not receive a replacement fullscreen API', () => {
  const p = page('https://attacker.example', true);
  assert.equal(p.Element.prototype.requestFullscreen, undefined);
});

test('Twitch can expand after consuming a trusted click, but synthetic and stale clicks fail', async () => {
  const p = page('https://player.twitch.tv', true);
  const video = new p.Element();
  p.context.navigator.userActivation.isActive = false;
  p.click(false);
  await assert.rejects(video.requestFullscreen(), /user gesture/);
  p.click(true);
  await video.requestFullscreen();
  await p.document.exitFullscreen();
  p.advance(1001);
  await assert.rejects(video.requestFullscreen(), /user gesture/);
});

test('fullscreen events arrive after the request returns so providers can register callbacks', async () => {
  const p = page();
  const video = new p.Element();
  await video.requestFullscreen();
  assert.deepEqual(p.events, []);
  p.flush();
  assert.deepEqual(p.events, ['fullscreenchange', 'webkitfullscreenchange']);
});

test('providers can detect fullscreen events when native WebKit fullscreen is disabled', async () => {
  const p = page('https://player.twitch.tv', true);
  assert.ok('onfullscreenchange' in p.document);
  assert.ok('onwebkitfullscreenchange' in p.document);
  const states = [];
  p.document.onfullscreenchange = () => states.push(!!p.document.fullscreenElement);
  const video = new p.Element();
  await video.requestFullscreen();
  p.flush();
  await p.document.exitFullscreen();
  p.flush();
  assert.deepEqual(states, [true, false]);
  p.document.onfullscreenchange = null;
  await video.requestFullscreen();
  p.flush();
  assert.deepEqual(states, [true, false]);
});
