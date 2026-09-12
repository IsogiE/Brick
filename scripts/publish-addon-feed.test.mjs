import assert from 'node:assert/strict';
import test from 'node:test';
import { isSameSource, ensureFeedRelease } from './publish-addon-feed.mjs';

const source = {
  provider: 'github-packager', repo: 'IsogiE/AdvanceRaidTools',
  commit: 'caa83a49a97619a893184d6b3bca14ed8155cdc3',
  version: 'v1.7.11-1-gcaa83a4', releaseType: 'alpha',
  packageFile: 'AdvanceRaidTools-v1.7.11-1-gcaa83a4.zip',
  packagerCommit: 'pinned-packager', packagerScriptSha256: 'pinned-script'
};
function manifest() {
  return {
    schema: 1, packageId: 'AdvanceRaidTools', commit: source.commit,
    version: source.version, source: { ...source },
    artifact: { size: 1234, sha256: 'a'.repeat(64) }
  };
}

test('scheduled repackaging preserves the existing package despite different ZIP bytes', () => {
  const existing = manifest();
  assert.equal(isSameSource(existing, source), true);
  existing.artifact.sha256 = 'b'.repeat(64);
  existing.artifact.size = 1250;
  assert.equal(isSameSource(existing, source), true);
});
test('new addon commits publish even if the display version stays the same', () => {
  assert.equal(isSameSource(manifest(), { ...source, commit: 'new-commit' }), false);
});
test('tagging the same commit as a release publishes the release package', () => {
  assert.equal(isSameSource(manifest(), { ...source, version: 'v1.7.12', releaseType: 'release' }), false);
});
test('deliberate packager changes publish a new package', () => {
  for (const key of ['packagerCommit', 'packagerScriptSha256']) {
    assert.equal(isSameSource(manifest(), { ...source, [key]: 'changed' }), false);
  }
});
test('missing or incomplete feed metadata cannot suppress publication', () => {
  for (const existing of [null, {}, { ...manifest(), artifact: {} }, { ...manifest(), source: {} }]) {
    assert.equal(isSameSource(existing, source), false);
  }
});
test('a package from another repository or schema is not considered current', () => {
  assert.equal(isSameSource(manifest(), { ...source, repo: 'another/repo' }), false);
  assert.equal(isSameSource({ ...manifest(), schema: 2 }, source), false);
});


test('publisher only reads channel metadata and never republishes a mutable release', () => {
  const calls = [];
  ensureFeedRelease('fixture', (args) => {
    calls.push(args);
    return JSON.stringify({ tag_name: 'addon-feed-v3', draft: false, immutable: false });
  });
  assert.deepEqual(calls, [['api', 'repos/IsogiE/Brick-Releases/releases/tags/addon-feed-v3']]);
});

test('missing, draft or immutable channels fail before any asset mutation', () => {
  for (const state of [null, {}, { draft: true, immutable: false }, { draft: false, immutable: true }]) {
    const calls = [];
    assert.throws(() => ensureFeedRelease('fixture', (args) => {
      calls.push(args);
      if (!state) throw new Error('HTTP 404');
      return JSON.stringify({ tag_name: 'addon-feed-v3', ...state });
    }));
    assert.equal(calls.length, 1);
    assert.equal(calls[0][0], 'api');
  }
});
