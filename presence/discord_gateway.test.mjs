import assert from 'node:assert/strict';
import { test } from 'node:test';
import { startDiscordGateway } from './discord_gateway.mjs';

async function fixture(t, url = 'wss://gateway.discord.gg') {
  t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  t.mock.method(Math, 'random', () => 0.5);
  const sockets = [], logs = [];
  class Socket extends EventTarget {
    static OPEN = 1;
    readyState = 1;
    sent = [];
    constructor(address) { super(); this.address = address; sockets.push(this); }
    send(value) { this.sent.push(JSON.parse(value)); }
    close() { this.readyState = 3; this.dispatchEvent(new Event('close')); }
    message(value) { this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(value) })); }
  }
  const stop = startDiscordGateway({ lookup: async () => ({ url }), token: 'test-only-bot', WebSocket: Socket,
    logger: { log: value => logs.push(value), error: value => logs.push(value) } });
  t.after(stop);
  await Promise.resolve();
  return { sockets, logs, stop };
}

test('gateway identification uses only the exact TLS Discord endpoint', async t => {
  const f = await fixture(t);
  const socket = f.sockets[0];
  assert.equal(socket.address, 'wss://gateway.discord.gg/?v=10&encoding=json');
  socket.message({ op: 10, d: { heartbeat_interval: 41_250 } });
  assert.equal(socket.sent.length, 1);
  assert.equal(socket.sent[0].d.token, 'test-only-bot');
  t.mock.timers.tick(20_625);
  assert.equal(socket.sent[1].op, 1);
  socket.message({ op: 11 });
  t.mock.timers.tick(41_250);
  assert.equal(socket.sent[2].op, 1);
  f.stop();
  t.mock.timers.tick(1_000_000);
  assert.equal(socket.sent.length, 3);
  assert.equal(f.sockets.length, 1);
});

test('gateway rejects redirected credential destinations and non-TLS URLs', async t => {
  for (const url of ['ws://gateway.discord.gg', 'wss://gateway.discord.gg.evil.example', 'wss://evil.example/gateway.discord.gg',
    'wss://gateway.discord.gg@evil.example', 'wss://user@gateway.discord.gg', 'wss://gateway.discord.gg/?redirect=evil',
    'wss://gateway.discord.gg:8443', 'wss://127.0.0.1']) {
    await t.test(url, async t => {
      const f = await fixture(t, url);
      assert.equal(f.sockets.length, 0);
      assert.deepEqual(f.logs, ['Discord Gateway lookup rejected']);
    });
  }
});

test('malformed and duplicate Hello messages cannot create timers or identify twice', async t => {
  for (const interval of [null, 0, -1, 1, 999, 120_001, 2 ** 32, '41250', 1000.5]) {
    await t.test(String(interval), async t => {
      const f = await fixture(t);
      const socket = f.sockets[0];
      socket.message({ op: 10, d: { heartbeat_interval: interval } });
      assert.equal(socket.readyState, 3);
      assert.deepEqual(socket.sent, []);
    });
  }
  await t.test('duplicate Hello and queued heartbeat cleanup', async t => {
    const f = await fixture(t);
    const socket = f.sockets[0];
    socket.message({ op: 10, d: { heartbeat_interval: 41_250 } });
    socket.message({ op: 10, d: { heartbeat_interval: 41_250 } });
    t.mock.timers.tick(30_000);
    assert.equal(socket.sent.length, 1);
    assert.equal(socket.readyState, 3);
  });
});

test('gateway logs never include untrusted usernames or error payloads', async t => {
  const f = await fixture(t);
  f.sockets[0].message({ op: 0, t: 'READY', d: { user: { username: '\nFORGED LOG\u001b[2J' } } });
  const error = new Event('error');
  error.message = '\nFORGED ERROR';
  f.sockets[0].dispatchEvent(error);
  assert.deepEqual(f.logs, ['Discord Gateway ready', 'Discord Gateway socket error']);
});
