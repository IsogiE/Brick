import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';

const captureSource = await readFile(new URL('../src/stream_preference_capture.js', import.meta.url), 'utf8');
const relaySource = await readFile(new URL('../src/stream_preference_relay.js', import.meta.url), 'utf8');
const key = 'content-classification-labels-acknowledged';
const wrapper = 'https://brick.example/v1/streams/player/1/twitch';
const origin = 'https://brick.example';
const twitch = 'https://player.twitch.tv';
const nonce = 'test-only-nonce';

function capture(remembered = {}, providerOrigin = twitch) {
  const context = vm.createContext({ messages: [], providerOrigin });
  vm.runInContext(`
    class Storage {
      constructor() { this.data = new Map(); }
      getItem(key) { return this.data.get(String(key)) ?? null; }
      setItem(key, value) { this.data.set(String(key), String(value)); }
      removeItem(key) { this.data.delete(String(key)); }
      clear() { this.data.clear(); }
    }
    globalThis.Storage = Storage;
    globalThis.window = globalThis;
    globalThis.localStorage = new Storage();
    globalThis.location = { origin: providerOrigin };
    globalThis.parent = { postMessage(body, origin) { messages.push({body, origin}); } };
    globalThis.top = parent;
    globalThis.addEventListener = () => {};
  `, context);
  const script = captureSource
    .replace('__BRICK_WRAPPER_ORIGIN__', JSON.stringify(origin))
    .replace('__BRICK_ACKNOWLEDGEMENTS__', JSON.stringify(remembered));
  vm.runInContext(script, context);
  return {
    context,
    read: () => vm.runInContext(`localStorage.getItem(${JSON.stringify(key)})`, context),
    write: value => vm.runInContext(`localStorage.setItem(${JSON.stringify(key)}, ${JSON.stringify(JSON.stringify(value))})`, context),
  };
}

function relay() {
  const frame = {};
  const native = [];
  const listeners = new Map();
  const context = vm.createContext({
    location: { href: wrapper },
    document: { querySelector: () => ({ contentWindow: frame }) },
    addEventListener: (name, fn) => listeners.set(name, fn),
    ipc: { postMessage: body => native.push(JSON.parse(body)) },
  });
  vm.runInContext('globalThis.window = globalThis; globalThis.top = globalThis;', context);
  vm.runInContext(relaySource
    .replace('__BRICK_WRAPPER_URL__', JSON.stringify(wrapper))
    .replace('__BRICK_NONCE__', JSON.stringify(nonce)), context);
  return { frame, native, receive: event => listeners.get('message')(event) };
}

test('captures a real storage change and restores precisely those unexpired choices on reopening', () => {
  const first = capture();
  assert.equal(first.read(), null);
  assert.equal(first.context.messages.length, 0);
  const expiry = Date.now() + 60_000;
  first.write({ loggedIn: { 'provider-account': ['Gambling'] }, loggedOut: { Gambling: expiry } });
  const message = first.context.messages[0];
  assert.equal(message.origin, origin);
  assert.deepEqual(JSON.parse(message.body), { kind: 'brick-twitch-consent', acknowledgements: { Gambling: expiry } });
  const protectedWrapper = relay();
  protectedWrapper.receive({ origin: twitch, source: protectedWrapper.frame, data: message.body });
  assert.deepEqual(protectedWrapper.native, [{ nonce, acknowledgements: { Gambling: expiry } }]);
  const reopened = capture(protectedWrapper.native[0].acknowledgements);
  assert.deepEqual(JSON.parse(reopened.read()), { loggedIn: {}, loggedOut: { Gambling: expiry } });
  assert.equal(reopened.context.messages.length, 0, 'restoring does not fabricate or renew an acknowledgement');
});

test('captures no cookies, accounts, arbitrary keys, expired choices or oversized browser values', () => {
  const player = capture();
  const now = Date.now();
  player.write({ loggedIn: { account: ['secret'] }, loggedOut: {
    Gambling: now - 1,
    MatureGame: now + 2_592_100_000,
    'auth-token': 'secret',
    ViolentGraphic: now + 60_000,
  } });
  assert.deepEqual(JSON.parse(player.context.messages[0].body).acknowledgements, { ViolentGraphic: now + 60_000 });
  vm.runInContext('localStorage.setItem("auth-token", "secret")', player.context);
  player.write({ loggedOut: {}, other: 'x'.repeat(4097) });
  assert.equal(player.context.messages.length, 1);
  vm.runInContext(`localStorage.removeItem(${JSON.stringify(key)})`, player.context);
  assert.deepEqual(JSON.parse(player.context.messages[1].body).acknowledgements, {});
  player.write({ loggedIn: {}, loggedOut: { Gambling: now + 60_000 } });
  vm.runInContext('localStorage.clear()', player.context);
  assert.deepEqual(JSON.parse(player.context.messages[3].body).acknowledgements, {});
});

test('relay trusts browser supplied origin and its direct iframe identity, not message claims', () => {
  const player = relay();
  const data = JSON.stringify({ kind: 'brick-twitch-consent', acknowledgements: {} });
  for (const event of [
    { origin: 'https://attacker.example', source: player.frame, data },
    { origin: twitch, source: {}, data },
    { origin: twitch, source: player.frame, data: 'x'.repeat(4097) },
    { origin: twitch, source: player.frame, data: { origin: twitch, kind: 'brick-twitch-consent' } },
    { origin: twitch, source: player.frame, data: '{invalid' },
    { origin: twitch, source: player.frame, data: JSON.stringify({ kind: 'execute-command' }) },
  ]) player.receive(event);
  assert.equal(player.native.length, 0);
  player.receive({ origin: twitch, source: player.frame, data });
  assert.deepEqual(player.native, [{ nonce, acknowledgements: {} }]);
});

test('capture stays inactive outside the exact Twitch player origin', () => {
  const player = capture({ Gambling: Date.now() + 60_000 }, 'https://www.youtube.com');
  assert.equal(player.read(), null);
  player.write({ loggedIn: {}, loggedOut: { Gambling: Date.now() + 60_000 } });
  assert.equal(player.context.messages.length, 0);
});
