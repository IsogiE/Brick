import http from 'node:http';
import { timingSafeEqual } from 'node:crypto';
import { HttpError, RateLimit, readBounded } from './security.mjs';

const MAX_BYTES = 4096;
const isObject = value => value && typeof value === 'object' && !Array.isArray(value);

// This listener is bound only to the container's private worker network. Its
// credential grants lease/result access, never Discord or profile access.
export function createTimestampQueue({ library, token }) {
  if (!/^[a-f0-9]{64}$/.test(token || '')) throw new Error('Invalid timestamp worker credential');
  const expected = Buffer.from(`Bearer ${token}`);
  const requests = new RateLimit(60, 1, 30, 0.5);
  const server = http.createServer({ maxHeaderSize: 4096, headersTimeout: 5000, requestTimeout: 5000 }, async (request, response) => {
    const send = (status, value) => {
      response.writeHead(status, { 'content-type': 'application/json', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
      response.end(JSON.stringify(value));
    };
    try {
      requests.take(request.socket.remoteAddress || 'unknown');
      const actual = Buffer.from(request.headers.authorization || '');
      if (actual.length !== expected.length || !timingSafeEqual(actual, expected)) throw new HttpError(401, 'Unauthorized');
      if (request.method !== 'POST' || !['/claim', '/finish'].includes(request.url)) throw new HttpError(404, 'Not found');
      let size = 0;
      const parts = [];
      for await (const chunk of request) {
        size += chunk.length;
        if (size > MAX_BYTES) throw new HttpError(413, 'Request too large');
        parts.push(chunk);
      }
      const body = size ? JSON.parse(Buffer.concat(parts).toString('utf8')) : {};
      if (!isObject(body)) throw new HttpError(400, 'Invalid request');
      if (request.url === '/claim') {
        if (Object.keys(body).length) throw new HttpError(400, 'Invalid claim');
        send(200, { job: library.claim() });
      } else {
        if (Object.keys(body).some(key => !['id', 'lease', 'result'].includes(key))
            || !/^[a-f0-9]{64}$/.test(body.id || '') || !/^[a-f0-9-]{36}$/.test(body.lease || '')
            || (body.result !== null && (!isObject(body.result)
              || Object.keys(body.result).some(key => !['unixSeconds', 'videoSeconds', 'uncertaintySeconds', 'error'].includes(key))))) {
          throw new HttpError(400, 'Invalid result');
        }
        send(200, { measured: library.finishLease(body.id, body.lease, body.result) });
      }
    } catch (error) {
      send(error instanceof HttpError ? error.status : error instanceof SyntaxError ? 400 : 500, { error: 'Worker request rejected' });
    }
  });
  server.maxConnections = 8;
  server.maxRequestsPerSocket = 20;
  server.keepAliveTimeout = 1000;
  server.setTimeout(5000, socket => socket.destroy());
  return server;
}

export function createTimestampClient({ url, token, fetch = globalThis.fetch }) {
  const origin = new URL(url);
  if (origin.protocol !== 'http:' || origin.username || origin.password || origin.search || origin.hash
      || !['api', '127.0.0.1', 'localhost'].includes(origin.hostname) || origin.pathname !== '/') {
    throw new Error('Invalid private timestamp queue address');
  }
  if (!/^[a-f0-9]{64}$/.test(token || '')) throw new Error('Invalid timestamp worker credential');
  async function call(route, body, signal) {
    const response = await fetch(new URL(route, origin), { method: 'POST', redirect: 'error',
      headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
      body: JSON.stringify(body), signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(8000)]) : AbortSignal.timeout(8000) });
    if (!response.ok) throw new Error('Timestamp queue unavailable');
    return JSON.parse(await readBounded(response, MAX_BYTES));
  }
  return {
    async claim(signal) { return (await call('/claim', {}, signal)).job; },
    async finish(job, result, signal) { return (await call('/finish', { id: job.id, lease: job.lease, result }, signal)).measured; },
  };
}
