import assert from 'node:assert/strict';
import { test } from 'node:test';
import { once } from 'node:events';
import { createTimestampQueue, createTimestampClient } from './timestamp_queue.mjs';

const token = 'a1'.repeat(32);
async function fixture(t) {
  const calls = [];
  const job = { id: 'ab'.repeat(32), lease: '12345678-1234-1234-1234-123456789abc', key: { provider: 'youtube' } };
  const server = createTimestampQueue({ token, library: {
    claim() { calls.push('claim'); return job; },
    finishLease(...args) { calls.push(args); return true; },
  } });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  t.after(async () => { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); });
  const url = `http://127.0.0.1:${server.address().port}/`;
  return { url, calls, job, client: createTimestampClient({ url, token }) };
}

test('private worker queue authenticates before leasing work and exposes no API data routes', async t => {
  const f = await fixture(t);
  for (const auth of ['', `Bearer ${'b2'.repeat(32)}`]) {
    assert.equal((await fetch(f.url + 'claim', { method: 'POST', headers: { authorization: auth } })).status, 401);
  }
  assert.deepEqual(f.calls, []);
  assert.equal((await fetch(f.url + 'v1/roster', { headers: { authorization: `Bearer ${token}` } })).status, 404);
  assert.deepEqual(await f.client.claim(), f.job);
  assert.deepEqual(f.calls, ['claim']);
});

test('worker results carry only lease identity and bounded numeric output', async t => {
  const f = await fixture(t);
  const result = { unixSeconds: 1700000000, videoSeconds: 12, uncertaintySeconds: 0.1 };
  assert.equal(await f.client.finish(f.job, result), true);
  assert.deepEqual(f.calls, [[f.job.id, f.job.lease, result]]);
  for (const body of [
    { id: f.job.id, lease: f.job.lease, result, key: { provider: 'twitch' } },
    { id: f.job.id, lease: f.job.lease, result: { command: 'untrusted' } },
    { id: f.job.id, lease: f.job.lease, result: 'x'.repeat(5000) },
  ]) {
    const response = await fetch(f.url + 'finish', { method: 'POST', headers: { authorization: `Bearer ${token}` }, body: JSON.stringify(body) });
    assert([400, 413].includes(response.status));
  }
  assert.equal(f.calls.length, 1);
});

test('worker client restricts its credential to a private queue and rejects redirects', async () => {
  for (const url of ['https://example.com/', 'http://api.evil/', 'http://token@api/', 'file:///data']) {
    assert.throws(() => createTimestampClient({ url, token }));
  }
  const client = createTimestampClient({ url: 'http://api:8081/', token, fetch: async (url, options) => {
    assert.equal(url.href, 'http://api:8081/claim');
    assert.equal(options.redirect, 'error');
    return new Response('{"job":null}');
  } });
  assert.equal(await client.claim(), null);
});
