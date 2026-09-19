// Version monitoring reads metadata only; it never executes downloaded content.
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
import assert from 'node:assert/strict';

async function text(url) {
  const response = await fetch(url, { redirect: 'error', signal: AbortSignal.timeout(20_000) });
  if (!response.ok) throw new Error(`Runtime release metadata unavailable: ${response.status}`);
  return readBoundedMetadata(response.body);
}
export async function readBoundedMetadata(body) {
  const parts = [];
  let size = 0;
  for await (const part of body) {
    size += part.byteLength;
    if (size > 2 * 1024 * 1024) throw new Error('Runtime metadata exceeded its size limit.');
    parts.push(Buffer.from(part));
  }
  // Decode once: arbitrary network chunks can split an advisory's UTF-8 text.
  return Buffer.concat(parts).toString('utf8');
}
function compare(a, b) {
  const x = a.split('.').map(Number), y = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] - y[i];
  return 0;
}

export function checkRuntimeMetadata({ workflow, releases, review, advisories, advisory, now = Date.now() }) {
  const minimum = workflow.match(/--atleast-version=(\d+\.\d+\.\d+) webkit2gtk/)?.[1];
  assert(minimum, 'Missing reviewed WebKit minimum version');
  const versions = [...new Set([...releases.matchAll(/webkitgtk-(2\.\d+\.\d+)\.tar\.xz/g)].map(m => m[1]))]
    .filter(v => Number(v.split('.')[1]) % 2 === 0).sort((a, b) => compare(b, a));
  const upstream = versions[0];
  assert(upstream, 'WebKit release metadata changed format; review it manually');
  if (compare(upstream, minimum) <= 0) return { minimum, upstream, deferred: false };

  // A new feature series can precede its Ubuntu security package. A review is
  // specific to that first release, the tested floor, the advisory bytes, and
  // at most seven days. It cannot accept an unseen release or maintenance fix.
  assert(review?.schema === 1 && review.minimumVersion === minimum && review.upstreamVersion === upstream,
    `Review new WebKitGTK ${upstream}; Brick AppImage minimum is ${minimum}`);
  assert(/^2\.\d+\.0$/.test(upstream) && Number(upstream.split('.')[1]) > Number(minimum.split('.')[1]),
    'Only an explicitly reviewed first feature release may be deferred');
  const maintenance = versions.find(v => v.split('.').slice(0, 2).join('.') === minimum.split('.').slice(0, 2).join('.'));
  assert(maintenance && compare(maintenance, minimum) <= 0, `Review WebKit maintenance update ${maintenance}`);
  const reviewedAt = Date.parse(review.reviewedAt), expiresAt = Date.parse(review.expiresAt);
  assert(Number.isFinite(now) && Number.isFinite(reviewedAt) && Number.isFinite(expiresAt)
    && expiresAt > reviewedAt && expiresAt - reviewedAt <= 7 * 86_400_000
    && now >= reviewedAt && now < expiresAt, 'WebKit feature-release review expired or has invalid dates');
  assert(typeof review.reason === 'string' && review.reason.length >= 40, 'Missing runtime review rationale');
  const latestAdvisory = [...new Set((advisories || '').match(/WSA-\d{4}-\d{4}/g) || [])].sort().at(-1);
  assert(latestAdvisory && latestAdvisory === review.latestAdvisory, 'Review new or unreadable WebKit security advisory');
  assert(typeof advisory === 'string' && /^[a-f0-9]{64}$/.test(review.advisorySha256 || '')
    && createHash('sha256').update(advisory).digest('hex') === review.advisorySha256,
    'Reviewed WebKit security advisory changed');
  return { minimum, upstream, deferred: true, expiresAt: review.expiresAt, latestAdvisory };
}

async function main() {
  const workflow = readFileSync(new URL('../.github/workflows/release.yml', import.meta.url), 'utf8');
  const review = JSON.parse(readFileSync(new URL('../security/webkit-feature-review.json', import.meta.url), 'utf8'));
  assert(/^WSA-\d{4}-\d{4}$/.test(review.latestAdvisory), 'Invalid reviewed advisory ID');
  const [releases, advisories, advisory] = await Promise.all([
    text('https://webkitgtk.org/releases/'), text('https://webkitgtk.org/security.html'),
    text(`https://webkitgtk.org/security/${review.latestAdvisory}.html`),
  ]);
  console.log(JSON.stringify(checkRuntimeMetadata({ workflow, releases, review, advisories, advisory })));
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
