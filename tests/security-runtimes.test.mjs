import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { checkRuntimeMetadata, readBoundedMetadata } from '../scripts/check-security-runtimes.mjs';
const advisory = 'Reviewed advisory fixture';
const fixture = () => ({
  workflow: 'pkg-config --atleast-version=2.52.6 webkit2gtk-4.1',
  releases: 'webkitgtk-2.52.6.tar.xz webkitgtk-2.54.0.tar.xz webkitgtk-2.55.1.tar.xz',
  advisory, advisories: 'WSA-2026-0004 WSA-2026-0005', now: Date.parse('2026-09-19T12:00:00Z'),
  review: { schema: 1, minimumVersion: '2.52.6', upstreamVersion: '2.54.0',
    reviewedAt: '2026-09-19T00:00:00Z', expiresAt: '2026-09-26T00:00:00Z',
    latestAdvisory: 'WSA-2026-0005', advisorySha256: createHash('sha256').update(advisory).digest('hex'),
    reason: 'The tested distribution runtime includes all fixes in the reviewed advisory.' },
});
test('latest installed stable runtime needs no deferral', () => {
  const f=fixture(); f.workflow='pkg-config --atleast-version=2.54.0 webkit2gtk-4.1'; f.review=null;
  assert.equal(checkRuntimeMetadata(f).deferred,false);
});
test('review accepts only the exact first feature release within its window', () => {
  assert.equal(checkRuntimeMetadata(fixture()).deferred,true);
});
for (const version of ['2.54.1','2.56.0','2.52.7']) test(`unreviewed release ${version} blocks`, () => {
  const f=fixture(); f.releases+=` webkitgtk-${version}.tar.xz`; assert.throws(()=>checkRuntimeMetadata(f));
});
for (const mutation of [
  f=>{f.review=null;}, f=>{f.workflow='pkg-config --atleast-version=2.52.5 webkit2gtk-4.1';},
  f=>{f.releases='malformed';}, f=>{f.advisories='malformed';}, f=>{f.advisories+=' WSA-2026-0006';},
  f=>{f.advisory+=' changed';}, f=>{f.review.advisorySha256='';}, f=>{f.now=Date.parse(f.review.expiresAt);},
  f=>{f.now=Date.parse(f.review.reviewedAt)-1;}, f=>{f.review.expiresAt='2026-09-27T00:00:00Z';},
  f=>{f.review.reviewedAt='invalid';}, f=>{f.review.reason='';},
]) test(`invalid or stale review fails closed: ${mutation.toString()}`, () => {
  const f=fixture(); mutation(f); assert.throws(()=>checkRuntimeMetadata(f));
});
test('a patch release cannot be approved as a feature deferral', () => {
  const f=fixture(); f.releases+=' webkitgtk-2.54.1.tar.xz'; f.review.upstreamVersion='2.54.1';
  assert.throws(()=>checkRuntimeMetadata(f),/first feature release/);
});

test('metadata hashes are independent of UTF-8 network chunk boundaries', async () => {
  const value='Security advisory: 杉山 壮太'; const bytes=Buffer.from(value);
  async function* body() { for (const byte of bytes) yield Uint8Array.of(byte); }
  assert.equal(await readBoundedMetadata(body()),value);
});
test('metadata body size is bounded in bytes', async () => {
  async function* body() { yield Buffer.alloc(2*1024*1024); yield Buffer.from('x'); }
  await assert.rejects(readBoundedMetadata(body()),/size limit/);
});
