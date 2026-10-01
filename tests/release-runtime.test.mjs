import test from 'node:test';
import assert from 'node:assert/strict';
import { verifyRuntimeRelease } from '../scripts/check-release-runtime.mjs';
const workflow = 'pkg-config --atleast-version=2.54.0 webkit2gtk-4.1';
function fixture() {
  const version = '2.54.0', tag = `runtime-webkitgtk-${version}-ubuntu24.04-amd64-r1`;
  const fileName = `brick-webkitgtk-${version}-ubuntu24.04-amd64.tar.xz`;
  const lock = { schema: 1, version, distribution: 'ubuntu-24.04', architecture: 'amd64', tag, fileName,
    url: `https://github.com/IsogiE/Brick-Releases/releases/download/${tag}/${fileName}`, sha256: 'a'.repeat(64), bytes: 123,
    source: {url: `https://webkitgtk.org/releases/webkitgtk-${version}.tar.xz`, sha256: 'b'.repeat(64)}, dependencies: ['libc6:amd64'] };
  const release = {tag_name: tag, draft: false, immutable: true, assets: [{name: fileName, state: 'uploaded',
    digest: `sha256:${lock.sha256}`, size: lock.bytes, browser_download_url: lock.url}]};
  return {lock, release};
}
test('the exact immutable prebuilt runtime passes before compilation', () => {
  const {lock, release} = fixture(); assert.equal(verifyRuntimeRelease(lock, release, workflow).available, true);
});
for (const [name, mutate] of [
  ['old runtime version', f => {f.lock.version = '2.52.6';}],
  ['mutable release', f => {f.release.immutable = false;}],
  ['unpublished release', f => {f.release.draft = true;}],
  ['missing artifact', f => {f.release.assets = [];}],
  ['different bytes', f => {f.release.assets[0].digest = 'sha256:' + 'c'.repeat(64);}],
  ['different size', f => {f.release.assets[0].size++;}],
  ['untrusted download host', f => {f.lock.url = 'https://example.com/runtime';}],
  ['wrong architecture', f => {f.lock.architecture = 'arm64';}],
  ['unsafe dependency argument', f => {f.lock.dependencies = ['--allow-unauthenticated'];}],
]) test(name + ' blocks expensive build work', () => {
  const f = fixture(); mutate(f); assert.throws(() => verifyRuntimeRelease(f.lock, f.release, workflow));
});
