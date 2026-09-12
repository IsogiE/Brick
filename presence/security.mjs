import { isIP } from "node:net";

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

export function bearerToken(authorization) {
  // Reject oversized input before parsing. Splitting the fixed scheme from a
  // bounded token avoids overlapping regex quantifiers on hostile whitespace.
  if (typeof authorization !== 'string' || authorization.length > 2055
      || authorization.slice(0, 7).toLowerCase() !== 'bearer ') return null;
  const token = authorization.slice(7);
  return token.length > 0 && token.length <= 2048 && /^[\x21-\x7e]+$/.test(token) ? token : null;
}

// Bounded token buckets: a flood of new addresses cannot evict existing budgets.
export class RateLimit {
  constructor(burst, perSecond, clientBurst, clientPerSecond, now = Date.now) {
    this.now = now;
    this.global = { tokens: burst, at: now() };
    this.clients = new Map();
    Object.assign(this, { burst, perSecond, clientBurst, clientPerSecond });
  }

  take(address) {
    const now = this.now();
    const refill = (bucket, burst, rate) => {
      bucket.tokens = Math.min(burst, bucket.tokens + Math.max(0, now - bucket.at) * rate / 1000);
      bucket.at = now;
    };
    refill(this.global, this.burst, this.perSecond);
    if (this.global.tokens < 1) throw new HttpError(429, "Too many requests. Try again shortly.");
    let client = this.clients.get(address);
    if (!client) {
      if (this.clients.size >= 2048) {
        for (const [key, entry] of this.clients) {
          if (now - entry.at >= this.clientBurst / this.clientPerSecond * 1000) this.clients.delete(key);
        }
        if (this.clients.size >= 2048) throw new HttpError(429, "Too many requests. Try again shortly.");
      }
      client = { tokens: this.clientBurst, at: now };
      this.clients.set(address, client);
    }
    refill(client, this.clientBurst, this.clientPerSecond);
    if (client.tokens < 1) throw new HttpError(429, "Too many requests. Try again shortly.");
    this.global.tokens--;
    client.tokens--;
  }
}

export function clientAddress(request, trustProxy) {
  // Enabled only on the private Docker network. Caddy overwrites this header.
  const forwarded = request.headers["x-brick-client-ip"];
  let ip = trustProxy && typeof forwarded === "string" && isIP(forwarded)
    ? forwarded : request.socket.remoteAddress || "unknown";
  if (ip.startsWith("::ffff:") && isIP(ip.slice(7)) === 4) ip = ip.slice(7);
  if (isIP(ip) === 6) {
    // Group IPv6 /64s so address rotation within a subnet cannot reset limits.
    const normalized = new URL(`http://[${ip}]/`).hostname.slice(1, -1);
    const [left, right = ""] = normalized.split("::");
    const head = left ? left.split(":") : [];
    const tail = right ? right.split(":") : [];
    ip = [...head, ...Array(8 - head.length - tail.length).fill("0"), ...tail].slice(0, 4).join(":");
  }
  return ip;
}

export async function readBounded(response, maxBytes) {
  if (Number(response.headers.get("content-length")) > maxBytes) {
    await response.body?.cancel();
    throw new HttpError(502, "Discord response is too large.");
  }
  const chunks = [];
  let size = 0;
  for await (const chunk of response.body || []) {
    size += chunk.length;
    if (size > maxBytes) throw new HttpError(502, "Discord response is too large.");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks).toString("utf8");
}
