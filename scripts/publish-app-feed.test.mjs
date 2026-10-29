import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, verify } from 'node:crypto';
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, writeFileSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildManifest, checkReleaseRun, decodeUpdateKey, findReleaseArtifacts, readRegularFile, releaseVersion, selectVersionRelease, signManifest, verifyUploadedFiles } from './publish-app-feed.mjs';

function fixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'brick-release-policy-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  for (const name of ['brick_0.5.3_x64-setup.exe', 'brick_0.5.3_x86_64.AppImage', 'brick_0.5.3_amd64.deb', 'brick_0.5.3_x86_64.tar.gz', 'PKGBUILD']) {
    writeFileSync(join(directory, name), `fixture ${name}`);
  }
  return directory;
}

test('release artifact policy rejects extra executables, missing packages, duplicates, and links', t => {
  const directory = fixture(t);
  const artifacts = findReleaseArtifacts(directory, '0.5.3');
  assert.equal(artifacts.length, 5);
  assert.equal(artifacts.find(a => a.kind === 'nsis').os, 'windows');
  writeFileSync(join(directory, 'unreviewed.exe'), 'payload');
  assert.throws(() => findReleaseArtifacts(directory, '0.5.3'), /Unexpected/);
  rmSync(join(directory, 'unreviewed.exe'));
  mkdirSync(join(directory, 'duplicate'));
  writeFileSync(join(directory, 'duplicate/PKGBUILD'), 'duplicate');
  assert.throws(() => findReleaseArtifacts(directory, '0.5.3'), /duplicate/);
  rmSync(join(directory, 'duplicate'), { recursive: true });
  rmSync(join(directory, 'PKGBUILD'));
  assert.throws(() => findReleaseArtifacts(directory, '0.5.3'), /required/);
  if (process.platform !== 'win32') {
    symlinkSync(join(directory, 'brick_0.5.3_x64-setup.exe'), join(directory, 'PKGBUILD'));
    assert.throws(() => findReleaseArtifacts(directory, '0.5.3'), /links/);
  }
});

test('release signatures bind installer bytes, source identity, and bounded expiration', t => {
  const keys = generateKeyPairSync('ed25519');
  const wrong = generateKeyPairSync('ed25519');
  const artifacts = findReleaseArtifacts(fixture(t), '0.5.3');
  const manifest = buildManifest({ version: '0.5.3', commit: 'a'.repeat(40), runId: '1234', artifacts, now: new Date('2026-09-12T12:00:00Z') });
  assert.equal(Date.parse(manifest.expiresAt) - Date.parse(manifest.builtAt), 90 * 86400_000);
  assert.equal(manifest.artifacts.some(a => 'path' in a), false);
  assert.throws(() => signManifest(manifest, wrong.privateKey, keys.publicKey), /dedicated/);
  assert.throws(() => signManifest(manifest, keys.privateKey), /dedicated/);
  const signed = signManifest(manifest, keys.privateKey, keys.publicKey);
  assert.equal(verify(null, signed.bytes, keys.publicKey, signed.signature), true);
  const changed = Buffer.from(signed.bytes.toString().replace(artifacts[0].sha256, 'f'.repeat(64)));
  assert.equal(verify(null, changed, keys.publicKey, signed.signature), false);
  assert.equal(verify(null, signed.bytes, wrong.publicKey, signed.signature), false);
});

test('release verification snapshots cannot be replaced through the download tree', t => {
  const directory = fixture(t);
  const snapshotDirectory = mkdtempSync(join(tmpdir(), 'brick-signing-snapshot-'));
  t.after(() => rmSync(snapshotDirectory, { recursive: true, force: true }));
  const artifacts = findReleaseArtifacts(directory, '0.5.3', { snapshotDirectory });
  for (const artifact of artifacts) {
    const original = readFileSync(artifact.path);
    writeFileSync(join(directory, artifact.fileName), 'replaced after verification');
    assert.deepEqual(readFileSync(artifact.path), original);
    assert.notDeepEqual(readFileSync(join(directory, artifact.fileName)), original);
  }
  assert.throws(() => findReleaseArtifacts(directory, '0.5.3', { snapshotDirectory }), /EEXIST/);
});

test('signing file reads reject links, directories, oversized input and public key-file permissions', t => {
  const directory = fixture(t);
  const file = join(directory, 'key');
  writeFileSync(file, 'private fixture', { mode: 0o600 });
  assert.equal(readRegularFile(file, 4096, { privateFile: true }).toString(), 'private fixture');
  assert.throws(() => readRegularFile(file, 2), /bounded/);
  assert.throws(() => readRegularFile(directory, 4096), /bounded/);
  chmodSync(file, 0o644);
  assert.throws(() => readRegularFile(file, 4096, { privateFile: true }), /private/);
  symlinkSync(file, join(directory, 'key-link'));
  assert.throws(() => readRegularFile(join(directory, 'key-link'), 4096), /ELOOP/);
});

test('release identity rejects branch names, partial commits, and version injection', () => {
  for (const version of ['v0.5.3', '0.5.3/evil', '01.5.3', '0.5.3-beta', 'main']) assert.throws(() => releaseVersion(version));
  assert.equal(releaseVersion('0.5.3'), '0.5.3');
  assert.throws(() => buildManifest({ version: '0.5.3', commit: 'main', runId: '123', artifacts: [] }));
  assert.throws(() => buildManifest({ version: '0.5.3', commit: 'a'.repeat(40), runId: '--help', artifacts: [] }));
});

test('version lookup finds unpublished drafts across pages and rejects ambiguous identities', () => {
  const draft = { id: 387619036, tag_name: 'v0.5.3', draft: true, html_url: 'https://github.com/IsogiE/Brick-Releases/releases/tag/untagged-example' };
  assert.equal(selectVersionRelease([[{ id: 1, tag_name: 'v0.5.2' }], [draft]], 'v0.5.3'), draft);
  assert.equal(selectVersionRelease([[]], 'v0.5.3'), undefined);
  assert.throws(() => selectVersionRelease([draft], 'v0.5.3'), /Invalid/);
  assert.throws(() => selectVersionRelease([[draft, draft]], 'v0.5.3'), /Ambiguous/);
  assert.throws(() => selectVersionRelease([[{ ...draft, id: 'untrusted/path' }]], 'v0.5.3'), /invalid/);
});

test('uploaded-file verification rejects missing, extra and tampered signatures as well as packages', t => {
  const directory = fixture(t);
  const paths = findReleaseArtifacts(directory, '0.5.3').map(artifact => artifact.path);
  for (const name of ['app-manifest.json', 'app-manifest.json.sig', 'PKGBUILD.sig']) {
    const path = join(directory, name); writeFileSync(path, `fixture ${name}`); paths.push(path);
  }
  const assets = paths.map(path => { const bytes = readFileSync(path); return {
    name: path.split('/').at(-1), size: bytes.length, digest: `sha256:${createHash('sha256').update(bytes).digest('hex')}`,
  }; });
  verifyUploadedFiles({ assets }, paths);
  assert.throws(() => verifyUploadedFiles({ assets: assets.slice(1) }, paths), /Unexpected/);
  assert.throws(() => verifyUploadedFiles({ assets: [...assets, { name: 'extra.exe' }] }, paths), /Unexpected/);
  const changed = assets.map(asset => asset.name === 'app-manifest.json.sig' ? { ...asset, digest: `sha256:${'0'.repeat(64)}` } : asset);
  assert.throws(() => verifyUploadedFiles({ assets: changed }, paths), /snapshot/);
});

test('release runs must be the exact main Release run; only the current run may still be in progress', () => {
  const commit = 'c'.repeat(40);
  const run = { id: 42, status: 'completed', conclusion: 'success', head_sha: commit, head_branch: 'main',
    path: '.github/workflows/release.yml', head_repository: { full_name: 'IsogiE/Brick' }, event: 'workflow_run' };
  checkReleaseRun(run, commit);
  checkReleaseRun({ ...run, status: 'in_progress', conclusion: null }, commit, '42');
  assert.throws(() => checkReleaseRun({ ...run, status: 'in_progress', conclusion: null }, commit, '43'), /Release run/);
  assert.throws(() => checkReleaseRun({ ...run, status: 'in_progress', conclusion: null }, commit), /Release run/);
  for (const change of [{ conclusion: 'failure' }, { head_sha: 'd'.repeat(40) }, { head_branch: 'feature' },
    { path: '.github/workflows/ci.yml' }, { head_repository: { full_name: 'fork/Brick' } }, { event: 'pull_request' }]) {
    assert.throws(() => checkReleaseRun({ ...run, ...change }, commit), /Release run/);
  }
});

test('the update key comes from base64 PKCS#8 in the environment', () => {
  const keys = generateKeyPairSync('ed25519');
  const encoded = keys.privateKey.export({ format: 'der', type: 'pkcs8' }).toString('base64');
  assert.equal(decodeUpdateKey(`${encoded}\n`).asymmetricKeyType, 'ed25519');
  for (const value of [undefined, '', 'not base64!']) assert.throws(() => decodeUpdateKey(value), /BRICK_APP_UPDATE_KEY_B64/);
});
