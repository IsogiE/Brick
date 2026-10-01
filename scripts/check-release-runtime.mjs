import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
import { readBoundedMetadata } from './check-security-runtimes.mjs';

const repository = 'IsogiE/Brick-Releases';
export function validateRuntimeLock(lock, workflow) {
  const minimum = workflow.match(/--atleast-version=(\d+\.\d+\.\d+) webkit2gtk-4\.1/)?.[1];
  assert(minimum && lock?.schema === 1 && lock.version === minimum, 'Runtime lock must match the reviewed release minimum');
  assert(lock.distribution === 'ubuntu-24.04' && lock.architecture === 'amd64', 'Unexpected runtime build target');
  assert(lock.tag === `runtime-webkitgtk-${lock.version}-ubuntu24.04-amd64-r1`, 'Unexpected runtime release tag');
  assert(lock.fileName === `brick-webkitgtk-${lock.version}-ubuntu24.04-amd64.tar.xz`, 'Unexpected runtime filename');
  assert(lock.url === `https://github.com/${repository}/releases/download/${lock.tag}/${lock.fileName}`, 'Unexpected runtime download URL');
  assert(/^[a-f0-9]{64}$/.test(lock.sha256 || ''), 'Missing pinned runtime digest');
  assert(Number.isSafeInteger(lock.bytes) && lock.bytes > 0 && lock.bytes <= 256 * 1024 * 1024, 'Invalid runtime archive size');
  assert(lock.source?.url === `https://webkitgtk.org/releases/webkitgtk-${lock.version}.tar.xz`
    && /^[a-f0-9]{64}$/.test(lock.source.sha256 || ''), 'Missing upstream source identity');
  assert(Array.isArray(lock.dependencies) && lock.dependencies.length > 0 && lock.dependencies.length <= 150
    && lock.dependencies.every(name => /^[a-z0-9][a-z0-9+.-]*(?::amd64)?$/.test(name)), 'Invalid runtime dependency package names');
  return lock;
}
export function verifyRuntimeRelease(lock, release, workflow) {
  validateRuntimeLock(lock, workflow);
  assert(release?.tag_name === lock.tag && release.draft === false && release.immutable === true,
    'The pinned runtime must be published as an immutable release');
  assert(Array.isArray(release.assets), 'Invalid runtime release metadata');
  const assets = release.assets.filter(asset => asset.name === lock.fileName);
  assert(assets.length === 1, 'Pinned prebuilt runtime asset is missing or duplicated');
  const asset = assets[0];
  assert(asset.state === 'uploaded' && asset.size === lock.bytes && asset.digest === `sha256:${lock.sha256}`
    && asset.browser_download_url === lock.url, 'Published runtime identity differs from the reviewed lock');
  return { version: lock.version, sha256: lock.sha256, bytes: lock.bytes, available: true };
}
async function main() {
  const workflow = readFileSync(new URL('../.github/workflows/release.yml', import.meta.url), 'utf8');
  const lock = validateRuntimeLock(JSON.parse(readFileSync(new URL('../packaging/linux/runtime.lock.json', import.meta.url), 'utf8')), workflow);
  const headers = { Accept: 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28' };
  if (process.env.GH_TOKEN) headers.Authorization = `Bearer ${process.env.GH_TOKEN}`;
  const response = await fetch(`https://api.github.com/repos/${repository}/releases/tags/${lock.tag}`,
    { headers, redirect: 'error', signal: AbortSignal.timeout(20_000) });
  assert(response.ok, `Pinned Linux runtime is not available: HTTP ${response.status}. Resolve it before builds or signing.`);
  console.log(JSON.stringify(verifyRuntimeRelease(lock, JSON.parse(await readBoundedMetadata(response.body)), workflow)));
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();
