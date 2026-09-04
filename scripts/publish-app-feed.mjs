import { createHash, createPrivateKey, sign } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';

const feedRepo = process.env.BRICK_APP_FEED_REPO || 'IsogiE/Brick-Releases';
const feedTag = process.env.BRICK_APP_FEED_TAG || 'app-feed';
const releaseAssetsDir = resolve(process.env.BRICK_RELEASE_ASSETS_DIR || 'release-assets');
const packageId = 'Brick';
const manifestName = 'app-manifest.json';
const signatureName = `${manifestName}.sig`;
const outputDir = resolve('.release/brick-app-feed');

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});

async function main() {
  const version = releaseVersion();
  const releaseTag = `v${version}`;
  const artifacts = findAppArtifacts(releaseAssetsDir, releaseTag, version);

  if (process.argv.includes('--list-artifacts')) {
    console.log(JSON.stringify(artifacts, null, 2));
    return;
  }

  const privateKeyB64 = requiredEnv('BRICK_ADDON_PRIVATE_KEY_B64');
  const ghToken = requiredEnvAny(['GH_TOKEN', 'GITHUB_TOKEN']);

  if (artifacts.length === 0) {
    throw new Error(`No Brick ${version} NSIS installer or AppImage found under ${releaseAssetsDir}.`);
  }

  const manifest = {
    schema: 1,
    packageId,
    version,
    commit: process.env.GITHUB_SHA || git(['rev-parse', 'HEAD']),
    builtAt: new Date().toISOString(),
    source: {
      provider: 'github-actions',
      repo: process.env.GITHUB_REPOSITORY || 'IsogiE/Brick',
      workflow: process.env.GITHUB_WORKFLOW || 'Release',
      runId: process.env.GITHUB_RUN_ID || '',
      releaseTag
    },
    artifacts
  };

  mkdirSync(outputDir, { recursive: true });
  const manifestPath = join(outputDir, manifestName);
  const signaturePath = join(outputDir, signatureName);
  const manifestJson = `${JSON.stringify(manifest, null, 2)}\n`;

  writeFileSync(manifestPath, manifestJson);
  writeFileSync(signaturePath, `${signManifest(manifestJson, privateKeyB64)}\n`);

  ensureFeedRelease(ghToken);
  gh(['release', 'upload', feedTag, manifestPath, signaturePath, '--repo', feedRepo, '--clobber'], ghToken);

  console.log(`Published Brick app update feed ${version} to ${feedRepo}@${feedTag}.`);
}

function releaseVersion() {
  const versionInput =
    process.env.BRICK_APP_VERSION
    || process.env.GITHUB_REF_NAME?.replace(/^v/, '')
    || null;
  const version = versionInput?.replace(/^v/, '') || null;

  if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error('BRICK_APP_VERSION or a v* SemVer tag is required.');
  }

  return version;
}

function findAppArtifacts(dir, releaseTag, version) {
  return findFiles(dir)
    .filter((path) => /\.(exe|appimage)$/i.test(path) && fileBelongsToVersion(basename(path), version))
    .map((path) => appArtifact(path, releaseTag))
    .sort((a, b) => a.fileName.localeCompare(b.fileName));
}

function fileBelongsToVersion(fileName, version) {
  const escaped = version.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`(^|[\\s._-])v?${escaped}($|[\\s._-])`, 'i').test(fileName);
}

function appArtifact(path, releaseTag) {
  const file = readFileSync(path);
  const fileName = basename(path);
  const kind = inferKind(fileName);

  return {
    os: kind === 'appimage' ? 'linux' : 'windows',
    arch: inferArch(fileName),
    kind,
    fileName,
    url: `https://github.com/${feedRepo}/releases/download/${encodeURIComponent(releaseTag)}/${encodeURIComponent(fileName)}`,
    sha256: createHash('sha256').update(file).digest('hex'),
    size: file.length
  };
}

function inferKind(fileName) {
  if (/\.appimage$/i.test(fileName)) {
    return 'appimage';
  }
  return 'nsis';
}

function inferArch(fileName) {
  const lower = fileName.toLowerCase();
  if (/(arm64|aarch64)/.test(lower)) {
    return 'aarch64';
  }
  if (/(x86|i686|win32)/.test(lower) && !/(x86_64|x64)/.test(lower)) {
    return 'x86';
  }
  return 'x86_64';
}

function findFiles(dir) {
  const files = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) {
      files.push(...findFiles(path));
    } else {
      files.push(path);
    }
  }
  return files;
}

function signManifest(manifestJson, privateKeyB64) {
  const privateKey = createPrivateKey({
    key: Buffer.from(privateKeyB64, 'base64'),
    format: 'der',
    type: 'pkcs8'
  });

  return sign(null, Buffer.from(manifestJson), privateKey).toString('base64');
}

function ensureFeedRelease(ghToken) {
  let releaseExists = false;
  try {
    gh(['release', 'view', feedTag, '--repo', feedRepo], ghToken, 'ignore');
    releaseExists = true;
  } catch {
    // The app feed release is addressed by a fixed tag so clients have a stable URL.
  }

  const title = 'Brick App Feed';
  const notes = 'Signed machine-readable app update feed used by Brick clients.';
  if (!releaseExists) {
    const createArgs = [
      'release',
      'create',
      feedTag,
      '--repo',
      feedRepo,
      '--title',
      title,
      '--notes',
      notes,
      '--prerelease',
      '--latest=false'
    ];

    try {
      gh(createArgs, ghToken);
    } catch {
      gh(createArgs.filter((arg) => arg !== '--latest=false'), ghToken);
    }
  }

  const editArgs = [
    'release',
    'edit',
    feedTag,
    '--repo',
    feedRepo,
    '--title',
    title,
    '--notes',
    notes,
    '--prerelease',
    '--latest=false'
  ];

  try {
    gh(editArgs, ghToken, 'ignore');
  } catch {
    gh(editArgs.filter((arg) => arg !== '--latest=false'), ghToken, 'ignore');
  }
}

function git(args) {
  return execFileSync('git', args, {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe']
  }).trim();
}

function gh(args, ghToken, stdio = 'inherit') {
  execFileSync('gh', args, {
    env: {
      ...process.env,
      GH_TOKEN: ghToken
    },
    stdio
  });
}

function requiredEnv(name) {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} is required.`);
  }
  return value;
}

function requiredEnvAny(names) {
  for (const name of names) {
    if (process.env[name]) {
      return process.env[name];
    }
  }
  throw new Error(`${names.join(' or ')} is required.`);
}
