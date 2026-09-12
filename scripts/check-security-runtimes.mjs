// Version monitoring reads metadata only; it never executes downloaded content.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';

async function text(url) {
  const response = await fetch(url, { redirect: 'error', signal: AbortSignal.timeout(20_000) });
  if (!response.ok) throw new Error(`Runtime release metadata unavailable: ${response.status}`);
  let value = '';
  for await (const part of response.body) {
    value += Buffer.from(part).toString('utf8');
    if (Buffer.byteLength(value) > 2 * 1024 * 1024) throw new Error('Runtime metadata exceeded its size limit.');
  }
  return value;
}
function newer(a, b) {
  const x = a.split('.').map(Number), y = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i];
  return false;
}
const dockerfile = readFileSync(new URL('../presence/Dockerfile.timestamp', import.meta.url), 'utf8');
const ffmpeg = dockerfile.match(/^ARG FFMPEG_VERSION=(\d+\.\d+\.\d+)$/m)?.[1];
assert(ffmpeg, 'Missing reviewed FFmpeg version');
const ffmpegPage = await text('https://ffmpeg.org/download.html');
const latestFfmpeg = ffmpegPage.match(/Download Source Code ffmpeg-(\d+\.\d+\.\d+)\.tar/)?.[1]
  || [...ffmpegPage.matchAll(/ffmpeg-(\d+\.\d+\.\d+)\.tar\.xz/g)].map(m => m[1]).sort((a,b) => newer(a,b) ? -1 : newer(b,a) ? 1 : 0)[0];
assert(latestFfmpeg, 'FFmpeg release metadata changed format; review it manually');
assert(!newer(latestFfmpeg, ffmpeg), `Review new FFmpeg ${latestFfmpeg}; worker currently builds ${ffmpeg}`);
const workflow = readFileSync(new URL('../.github/workflows/release.yml', import.meta.url), 'utf8');
const webkit = workflow.match(/--atleast-version=(\d+\.\d+\.\d+) webkit2gtk/)?.[1];
assert(webkit, 'Missing reviewed WebKit minimum version');
const releases = await text('https://webkitgtk.org/releases/');
const stable = [...releases.matchAll(/webkitgtk-(2\.\d+\.\d+)\.tar\.xz/g)]
  .map(m => m[1]).filter(v => Number(v.split('.')[1]) % 2 === 0)
  .sort((a,b) => newer(a,b) ? -1 : newer(b,a) ? 1 : 0)[0];
assert(stable, 'WebKit release metadata changed format; review it manually');
assert(!newer(stable, webkit), `Review new WebKitGTK ${stable}; Brick AppImage minimum is ${webkit}`);
console.log(`Reviewed runtime versions remain current: FFmpeg ${ffmpeg}; WebKitGTK ${webkit}`);
