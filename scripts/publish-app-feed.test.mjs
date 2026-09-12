import { test } from 'node:test';
import assert from 'node:assert/strict';
import { generateKeyPairSync, verify } from 'node:crypto';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { buildManifest, findReleaseArtifacts, releaseVersion, signManifest } from './publish-app-feed.mjs';

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

test('release identity rejects branch names, partial commits, and version injection', () => {
  for (const version of ['v0.5.3', '0.5.3/evil', '01.5.3', '0.5.3-beta', 'main']) assert.throws(() => releaseVersion(version));
  assert.equal(releaseVersion('0.5.3'), '0.5.3');
  assert.throws(() => buildManifest({ version: '0.5.3', commit: 'main', runId: '123', artifacts: [] }));
  assert.throws(() => buildManifest({ version: '0.5.3', commit: 'a'.repeat(40), runId: '--help', artifacts: [] }));
});
