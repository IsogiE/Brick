import { createHash, createPrivateKey, createPublicKey, sign, verify } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { closeSync, constants, fstatSync, mkdirSync, mkdtempSync, openSync, readSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

const trustedRoot = fileURLToPath(new URL('../', import.meta.url));
let root = trustedRoot;
const sourceRepo = 'IsogiE/Brick';
const feedRepo = 'IsogiE/Brick-Releases';
const feedTag = 'app-feed-v2';
const publicKeyBytes = Buffer.from(readFileSync(new URL('../security/app-update-public-key.b64', import.meta.url), 'utf8').trim(), 'base64');
export const publicKey = createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), publicKeyBytes]), format: 'der', type: 'spki' });
const MAX_ARTIFACT_BYTES = 256 * 1024 * 1024;

// Validate and read the same open file, never a path checked earlier. A bounded
// descriptor read also rejects devices, FIFOs, links and files growing mid-read.
export function readRegularFile(path, limit, { privateFile = false } = {}) {
  if (constants.O_NOFOLLOW === undefined) throw new Error('Local signing requires a host with no-follow file opens.');
  const fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  try {
    const before = fstatSync(fd);
    if (!before.isFile() || before.size <= 0 || before.size > limit
        || (privateFile && ((before.mode & 0o077) || before.uid !== process.getuid()))) {
      throw new Error('Expected a bounded regular file with private permissions for signing keys.');
    }
    const bytes = Buffer.alloc(before.size + 1);
    let size = 0;
    while (size < bytes.length) {
      const count = readSync(fd, bytes, size, bytes.length - size, null);
      if (!count) break;
      size += count;
    }
    const after = fstatSync(fd);
    if (size !== before.size || after.size !== before.size || after.mtimeMs !== before.mtimeMs
        || after.ctimeMs !== before.ctimeMs) throw new Error('File changed while reading.');
    return bytes.subarray(0, size);
  } finally { closeSync(fd); }
}

export function releaseVersion(value) {
  if (typeof value !== 'string' || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value)) throw new Error('An exact stable SemVer version is required.');
  return value;
}

export function findReleaseArtifacts(directory, version, { snapshotDirectory } = {}) {
  releaseVersion(version);
  const expected = new Map([
    [`brick_${version}_x64-setup.exe`, { os: 'windows', arch: 'x86_64', kind: 'nsis' }],
    [`brick_${version}_x86_64.AppImage`, { os: 'linux', arch: 'x86_64', kind: 'appimage' }],
    [`brick_${version}_amd64.deb`, { os: 'linux', arch: 'x86_64', kind: 'deb' }],
    [`brick_${version}_x86_64.tar.gz`, { os: 'linux', arch: 'x86_64', kind: 'pacman' }],
    ['PKGBUILD', { os: 'linux', arch: 'x86_64', kind: 'pkgbuild' }],
  ]);
  const found = new Map();
  function visit(dir, depth = 0) {
    if (depth > 2) throw new Error('Unexpected release artifact directory.');
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const name = entry.name;
      const path = join(dir, name);
      if (entry.isSymbolicLink()) throw new Error('Release artifacts cannot be links.');
      if (entry.isDirectory()) { visit(path, depth + 1); continue; }
      if (!entry.isFile() || !expected.has(name) || found.has(name)) throw new Error(`Unexpected or duplicate release artifact: ${name}`);
      const bytes = readRegularFile(path, MAX_ARTIFACT_BYTES);
      // All subsequent attestation, publisher, signature and upload operations
      // use this private snapshot, independent of the downloaded input tree.
      const checkedPath = snapshotDirectory ? join(snapshotDirectory, name) : path;
      if (snapshotDirectory) writeFileSync(checkedPath, bytes, { flag: 'wx', mode: 0o400 });
      found.set(name, { path: checkedPath, ...expected.get(name), fileName: name,
        url: `https://github.com/${feedRepo}/releases/download/v${version}/${encodeURIComponent(name)}`,
        sha256: createHash('sha256').update(bytes).digest('hex'), size: bytes.length });
    }
  }
  visit(directory);
  if (found.size !== expected.size) throw new Error('Both desktop installers and all three Linux package files are required.');
  return [...found.values()].sort((a, b) => a.fileName.localeCompare(b.fileName));
}

export function buildManifest({ version, commit, runId, artifacts, now = new Date() }) {
  releaseVersion(version);
  if (!/^[a-f0-9]{40}$/.test(commit) || !/^[0-9]+$/.test(String(runId))) throw new Error('Exact source commit and release run are required.');
  return { schema: 1, packageId: 'Brick', version, commit, builtAt: now.toISOString(),
    expiresAt: new Date(now.getTime() + 90 * 86400_000).toISOString(),
    source: { provider: 'github-actions', repo: sourceRepo, workflow: 'Release', runId: String(runId), releaseTag: `v${version}` },
    artifacts: artifacts.map(({ path, ...artifact }) => artifact) };
}

export function signManifest(manifest, privateKey, expected = publicKey) {
  if (privateKey.asymmetricKeyType !== 'ed25519'
      || !createPublicKey(privateKey).export({ format: 'der', type: 'spki' }).equals(expected.export({ format: 'der', type: 'spki' }))) {
    throw new Error('This is not the dedicated Brick app-update key.');
  }
  const bytes = Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`);
  const signature = sign(null, bytes, privateKey);
  if (!verify(null, bytes, expected, signature)) throw new Error('Local signature verification failed.');
  return { bytes, signature };
}

function command(name, args, { capture = false } = {}) {
  return execFileSync(name, args, { cwd: root, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024,
    stdio: capture ? ['ignore', 'pipe', 'pipe'] : ['ignore', 'inherit', 'inherit'] })?.trim();
}
const gh = (args, options) => command('gh', args, options);
const api = endpoint => JSON.parse(gh(['api', endpoint], { capture: true }));

async function main() {
  if (process.env.CI || process.env.GITHUB_ACTIONS) throw new Error('App-update signing and publication must run locally, never in CI.');
  const { values } = parseArgs({ options: {
    version: { type: 'string' }, assets: { type: 'string' }, key: { type: 'string' }, run: { type: 'string' },
    source: { type: 'string' }, 'reviewed-commit': { type: 'string' },
    output: { type: 'string' }, publish: { type: 'boolean', default: false },
  } });
  root = resolve(values.source || root);
  const version = releaseVersion(values.version);
  if (!/^[a-f0-9]{40}$/.test(values['reviewed-commit'] || '')) throw new Error('Provide --reviewed-commit with the full source SHA you independently reviewed for this release.');
  if (!values.assets || !values.key || !/^[0-9]+$/.test(values.run || '')) throw new Error('Provide --assets, --key and --run. Without --publish this only verifies and prepares local files.');
  command('git', ['diff', '--exit-code']);
  command('git', ['diff', '--cached', '--exit-code']);
  const commit = command('git', ['rev-parse', 'HEAD'], { capture: true });
  if (values['reviewed-commit'] !== commit) throw new Error('Source differs from your reviewed commit.');
  const configured = readFileSync(join(root, 'Cargo.toml'), 'utf8').match(/^version = "([^"]+)"/m)?.[1];
  if (configured !== version) throw new Error('Release version does not match the reviewed source.');
  const run = api(`repos/${sourceRepo}/actions/runs/${values.run}`);
  if (run.conclusion !== 'success' || run.head_sha !== commit || run.path !== '.github/workflows/release.yml'
      || run.head_repository?.full_name !== sourceRepo || !['push', 'workflow_dispatch'].includes(run.event)) {
    throw new Error('Artifacts must come from a successful Release run for this exact source commit.');
  }
  const mainBranch = api(`repos/${sourceRepo}/branches/main`);
  if (!mainBranch.protected || mainBranch.commit.sha !== commit) throw new Error('Only the current protected main commit may be released.');
  const checks = api(`repos/${sourceRepo}/commits/${commit}/check-runs?per_page=100`).check_runs;
  for (const name of ['ubuntu-22.04', 'windows-latest', 'Rust security advisories', 'Container security']) {
    const latest = checks.filter(check => check.name === name && check.app?.id === 15368).sort((a, b) => b.id - a.id)[0];
    if (latest?.conclusion !== 'success') throw new Error(`Required source check is missing: ${name}`);
  }
  const outputRoot = resolve(values.output || join(root, '.release', `signed-${version}`));
  mkdirSync(outputRoot, { recursive: true, mode: 0o700 });
  const outputFd = openSync(outputRoot, constants.O_RDONLY | constants.O_DIRECTORY | constants.O_NOFOLLOW);
  try {
    const stat = fstatSync(outputFd);
    if (!stat.isDirectory() || (stat.mode & 0o077) || stat.uid !== process.getuid()) throw new Error('The signing output directory must be private and owned by you.');
  } finally { closeSync(outputFd); }
  const output = mkdtempSync(join(outputRoot, 'release-'));
  const artifacts = findReleaseArtifacts(resolve(values.assets), version, { snapshotDirectory: output });
  for (const artifact of artifacts) {
    gh(['attestation', 'verify', artifact.path, '--repo', sourceRepo,
      '--signer-workflow', `${sourceRepo}/.github/workflows/release.yml`, '--source-digest', commit,
      '--signer-digest', commit, '--deny-self-hosted-runners']);
    if (artifact.kind === 'nsis') {
      const publishers = JSON.parse(readFileSync(join(trustedRoot, 'security/windows-publishers.json'), 'utf8'));
      let accepted = false;
      for (const publisher of publishers) {
        try {
          command(process.env.BRICK_OSSLSIGNCODE || 'osslsigncode', ['verify', '-in', artifact.path, '-require-leaf-hash', `sha256:${publisher}`]);
          accepted = true; break;
        } catch { /* Only a listed publisher with a valid signature is accepted. */ }
      }
      if (!accepted) throw new Error('Windows installer publisher verification failed.');
    }
  }
  // Read the private key only after all downloaded input verification is complete.
  const privateKey = createPrivateKey({ key: readRegularFile(resolve(values.key), 4096, { privateFile: true }), format: 'der', type: 'pkcs8' });
  const manifest = buildManifest({ version, commit, runId: values.run, artifacts });
  const { bytes, signature } = signManifest(manifest, privateKey);
  const manifestPath = join(output, 'app-manifest.json');
  const signaturePath = `${manifestPath}.sig`;
  writeFileSync(manifestPath, bytes, { flag: 'wx', mode: 0o400 });
  writeFileSync(signaturePath, `${signature.toString('base64')}\n`, { flag: 'wx', mode: 0o400 });
  const detached = [];
  for (const artifact of artifacts) {
    const path = join(output, `${artifact.fileName}.sig`);
    const artifactBytes = readRegularFile(artifact.path, MAX_ARTIFACT_BYTES);
    if (createHash('sha256').update(artifactBytes).digest('hex') !== artifact.sha256) throw new Error('Verified artifact changed before signing.');
    writeFileSync(path, `${sign(null, artifactBytes, privateKey).toString('base64')}\n`, { flag: 'wx', mode: 0o400 });
    detached.push(path);
  }
  console.log(`Verified and locally signed Brick ${version} from ${commit}. Manifest: ${manifestPath}`);
  if (!values.publish) return;
  if (!api(`repos/${feedRepo}/immutable-releases`).enabled) throw new Error('Enable immutable version releases before publication.');
  const feed = api(`repos/${feedRepo}/releases/tags/${feedTag}`);
  if (feed.immutable || feed.draft) throw new Error('The mutable app-feed-v2 channel must already exist.');
  // Never overwrite an existing published version. Everything is attached while draft.
  let release;
  try { release = api(`repos/${feedRepo}/releases/tags/v${version}`); } catch { /* create below */ }
  if (release && !release.draft) throw new Error('This version is already published; use a new version.');
  if (!release) gh(['release', 'create', `v${version}`, '--repo', feedRepo, '--draft', '--title', `Brick v${version}`, '--notes', '']);
  gh(['release', 'upload', `v${version}`, ...artifacts.map(artifact => artifact.path), manifestPath, signaturePath, ...detached, '--repo', feedRepo]);
  // Confirm GitHub has the exact signed artifact digests before making the release public.
  release = api(`repos/${feedRepo}/releases/tags/v${version}`);
  for (const artifact of artifacts) {
    if (!release.assets.some(asset => asset.name === artifact.fileName && asset.digest === `sha256:${artifact.sha256}` && asset.size === artifact.size)) throw new Error('Uploaded artifact did not match its signed digest.');
  }
  gh(['release', 'edit', `v${version}`, '--repo', feedRepo, '--draft=false', '--latest']);
  if (!api(`repos/${feedRepo}/releases/tags/v${version}`).immutable) throw new Error('Version publication did not become immutable; the update feed was not changed.');
  gh(['release', 'upload', feedTag, manifestPath, signaturePath, '--repo', feedRepo, '--clobber']);
  console.log(`Published immutable Brick ${version} and its locally signed update feed.`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
