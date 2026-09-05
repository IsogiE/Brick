import { createHash, createPrivateKey, sign } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const feedRepo = process.env.BRICK_FEED_REPO || 'IsogiE/Brick-Releases';
const feedTag = process.env.BRICK_FEED_TAG || 'addon-feed';
const sourceRepo = process.env.BRICK_ADDON_GITHUB_REPO || 'IsogiE/AdvanceRaidTools';
const sourceDir = resolve(process.env.BRICK_ADDON_SOURCE_DIR || '../AdvanceRaidTools');
const releaseDir = resolve(process.env.BRICK_ADDON_RELEASE_DIR || join(sourceDir, '.release'));
const packagerCommit = process.env.BRICK_PACKAGER_COMMIT || '';
const packagerScriptSha256 = process.env.BRICK_PACKAGER_SCRIPT_SHA256 || '';
const packageId = 'AdvanceRaidTools';
const artifactName = 'AdvanceRaidTools-current.zip';
const manifestName = 'addon-manifest.json';
const signatureName = `${manifestName}.sig`;
const outputDir = resolve('.release/brick-addon-feed');
const allowedFolders = [
  'AdvanceRaidTools',
  'AdvanceRaidTools_Libraries',
  'AdvanceRaidTools_Options'
];
const supportedFlavors = [
  'retail',
  'ptr',
  'xptr',
  'beta',
  'classic',
  'classic-era',
  'classic-ptr',
  'classic-beta'
];

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}

async function main() {
  const source = readSourceMetadata();
  const packagePath = findAddonPackage();
  const version = packagePath ? versionFromPackageName(packagePath) : (source.exactTag || source.describe);
  const releaseType = inferReleaseType(source.exactTag, version);

  if (process.argv.includes('--latest')) {
    const packagePart = packagePath ? `, package ${basename(packagePath)}` : ', package not built yet';
    console.log(`${version} (${releaseType}, ${source.shortHash}${packagePart})`);
    return;
  }

  const sourceInfo = {
    provider: 'github-packager',
    repo: sourceRepo,
    commit: source.commit,
    shortCommit: source.shortHash,
    version,
    releaseType,
    packageFile: packagePath ? basename(packagePath) : `${packageId}-${version}.zip`,
    packagerCommit,
    packagerScriptSha256
  };
  const existingManifest = await fetchExistingManifest();
  const current = isSameSource(existingManifest, sourceInfo);
  if (process.argv.includes('--check-current')) {
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `current=${current}\n`);
    console.log(current ? `Addon feed already current at ${version} (${source.shortHash}).` : `Addon feed needs ${version} (${source.shortHash}).`);
    return;
  }
  if (current) {
    console.log(`Addon feed already current at ${version} (${source.shortHash}).`);
    return;
  }
  if (!packagePath) {
    throw new Error(`No ${packageId} package zip found under ${releaseDir}. Run the packager first.`);
  }

  const privateKeyB64 = requiredEnv('BRICK_ADDON_PRIVATE_KEY_B64');
  const ghToken = requiredEnvAny(['GH_TOKEN', 'GITHUB_TOKEN']);
  const zip = readFileSync(packagePath);
  const sha256 = createHash('sha256').update(zip).digest('hex');
  const artifactUrl = `https://github.com/${feedRepo}/releases/download/${feedTag}/${artifactName}?brickSource=${source.commit}`;
  const manifest = {
    schema: 1,
    packageId,
    version,
    commit: source.commit,
    builtAt: source.commitDate,
    source: sourceInfo,
    artifact: {
      url: artifactUrl,
      sha256,
      size: zip.length,
      folders: allowedFolders,
      flavors: supportedFlavors
    }
  };

  mkdirSync(outputDir, { recursive: true });
  const artifactPath = join(outputDir, artifactName);
  const manifestPath = join(outputDir, manifestName);
  const signaturePath = join(outputDir, signatureName);
  const manifestJson = `${JSON.stringify(manifest, null, 2)}\n`;

  writeFileSync(artifactPath, zip);
  writeFileSync(manifestPath, manifestJson);
  writeFileSync(signaturePath, `${signManifest(manifestJson, privateKeyB64)}\n`);

  ensureFeedRelease(ghToken);
  gh(['release', 'upload', feedTag, artifactPath, manifestPath, signaturePath, '--repo', feedRepo, '--clobber'], ghToken);

  console.log(`Published ${version} from ${sourceRepo}@${source.shortHash} to ${feedRepo}@${feedTag}.`);
}

function readSourceMetadata() {
  if (!existsSync(sourceDir)) {
    throw new Error(`Addon source directory does not exist: ${sourceDir}`);
  }

  const commit = git(['rev-parse', 'HEAD']);
  const shortHash = git(['rev-parse', '--short=7', 'HEAD']);
  const commitDate = git(['show', '-s', '--format=%cI', 'HEAD']);
  const describe = git(['describe', '--tags', '--match', 'v[0-9]*', '--long', '--always']);
  const exactTag = gitMaybe(['describe', '--tags', '--match', 'v[0-9]*', '--exact-match', 'HEAD']);

  return {
    commit,
    shortHash,
    commitDate,
    describe,
    exactTag
  };
}

function findAddonPackage() {
  if (!existsSync(releaseDir)) {
    return undefined;
  }

  return findFiles(releaseDir)
    .filter((path) => {
      const name = basename(path);
      return name.startsWith(`${packageId}-`) && /\.zip$/i.test(name) && !/-nolib\.zip$/i.test(name);
    })
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
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

function versionFromPackageName(packagePath) {
  return basename(packagePath)
    .replace(new RegExp(`^${packageId}-`), '')
    .replace(/\.zip$/i, '');
}

function inferReleaseType(exactTag, version) {
  const marker = String(exactTag || version).toLowerCase();
  if (!exactTag || /-\d+-g[0-9a-f]+$/i.test(version)) {
    return 'alpha';
  }
  if (marker.includes('alpha')) {
    return 'alpha';
  }
  if (marker.includes('beta')) {
    return 'beta';
  }
  return 'release';
}

async function fetchExistingManifest() {
  const url = `https://github.com/${feedRepo}/releases/download/${feedTag}/${manifestName}`;
  const response = await fetch(url, {
    headers: {
      accept: 'application/json',
      'user-agent': 'Brick addon feed publisher'
    }
  });

  if (response.status === 404) {
    return null;
  }
  if (!response.ok) {
    throw new Error(`Existing manifest request failed: HTTP ${response.status} ${response.statusText}.`);
  }

  return response.json();
}

export function isSameSource(manifest, sourceInfo) {
  // A published source revision is immutable. ZIP timestamps and freshly
  // checked-out externals change archive bytes on otherwise identical builds;
  // comparing those hashes republishes the same version and reinstalls it on
  // every client. New addon revisions or packaging changes still publish.
  return Boolean(
    manifest
      && manifest.schema === 1
      && manifest.packageId === packageId
      && manifest.commit === sourceInfo.commit
      && manifest.version === sourceInfo.version
      && manifest.source
      && manifest.source.provider === sourceInfo.provider
      && manifest.source.repo === sourceInfo.repo
      && manifest.source.commit === sourceInfo.commit
      && manifest.source.packageFile === sourceInfo.packageFile
      && manifest.source.releaseType === sourceInfo.releaseType
      && manifest.source.packagerCommit === sourceInfo.packagerCommit
      && manifest.source.packagerScriptSha256 === sourceInfo.packagerScriptSha256
      && manifest.artifact
      && manifest.artifact.size > 0
      && /^[a-f0-9]{64}$/.test(manifest.artifact.sha256)
  );
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
    // The feed release is addressed by a fixed tag so clients have a stable URL.
  }

  if (!releaseExists) {
    const createArgs = [
      'release',
      'create',
      feedTag,
      '--repo',
      feedRepo,
      '--title',
      'Brick Addon Feed',
      '--notes',
      'Signed machine-readable addon feed used by Brick clients.',
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
    'Brick Addon Feed',
    '--notes',
    'Signed machine-readable addon feed used by Brick clients.',
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
  return execFileSync('git', ['-C', sourceDir, ...args], {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe']
  }).trim();
}

function gitMaybe(args) {
  try {
    return git(args);
  } catch {
    return '';
  }
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
