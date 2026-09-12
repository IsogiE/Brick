// One-way migration for clients that still trust the old shared app/addon key.
import { createPrivateKey, createPublicKey, sign, verify } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';

const repo = 'IsogiE/Brick-Releases';
const version = '0.5.3';
const gh = args => execFileSync('gh', args, { encoding: 'utf8', maxBuffer: 2 * 1024 * 1024 });
if (process.env.GITHUB_REPOSITORY !== 'IsogiE/Brick' || process.env.GITHUB_REF !== 'refs/heads/main') throw new Error('The bridge can only be published from Brick main.');
const release = JSON.parse(gh(['api', `repos/${repo}/releases/tags/v${version}`]));
if (!release.immutable || release.draft) throw new Error('The locally authorized immutable bridge release must already exist.');
mkdirSync('.release/legacy-bridge', { recursive: true });
gh(['release', 'download', `v${version}`, '--repo', repo, '--pattern', 'app-manifest.json*', '--dir', '.release/legacy-bridge']);
const bytes = readFileSync('.release/legacy-bridge/app-manifest.json');
const signature = Buffer.from(readFileSync('.release/legacy-bridge/app-manifest.json.sig', 'utf8').trim(), 'base64');
const publicBytes = Buffer.from(readFileSync(new URL('../security/app-update-public-key.b64', import.meta.url), 'utf8').trim(), 'base64');
const publicKey = createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), publicBytes]), format: 'der', type: 'spki' });
if (!verify(null, bytes, publicKey, signature)) throw new Error('The bridge is missing its independent local app signature.');
const manifest = JSON.parse(bytes);
if (manifest.packageId !== 'Brick' || manifest.schema !== 1 || manifest.version !== version || manifest.commit !== process.env.GITHUB_SHA) throw new Error('The bridge must match this exact reviewed main commit.');
if (Date.parse(manifest.expiresAt) <= Date.now()) throw new Error('Bridge authorization has expired.');
for (const artifact of manifest.artifacts) {
  if (!release.assets.some(asset => asset.name === artifact.fileName && asset.digest === `sha256:${artifact.sha256}` && asset.size === artifact.size)) throw new Error('An immutable bridge artifact differs from its signed manifest.');
}
const legacyKey = createPrivateKey({ key: Buffer.from(process.env.BRICK_ADDON_PRIVATE_KEY_B64 || '', 'base64'), format: 'der', type: 'pkcs8' });
const expected = Buffer.from('IbgAlzBy8TBsJ318uGkTq9PlBjFOuhgzqBhw4ZypPac=', 'base64');
if (!createPublicKey(legacyKey).export({ format: 'der', type: 'spki' }).subarray(-32).equals(expected)) throw new Error('Unexpected legacy signing key.');
writeFileSync('.release/legacy-bridge/app-manifest.json.sig', `${sign(null, bytes, legacyKey).toString('base64')}\n`);
gh(['release', 'upload', 'app-feed', '.release/legacy-bridge/app-manifest.json', '.release/legacy-bridge/app-manifest.json.sig', '--repo', repo, '--clobber']);
console.log('Published the fixed 0.5.3 migration. Retire BRICK_ADDON_PRIVATE_KEY_B64 immediately after verifying the legacy feed.');
