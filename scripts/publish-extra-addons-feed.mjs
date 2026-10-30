// Publishes the signed manifest for optional third-party addons. The addons
// are not rehosted: each entry pins the author's own download (a GitHub
// release zip, or the files of the newest tag in the author's CurseForge
// repository) by URL, size and SHA-256, and Brick downloads from there. Only
// the manifest and its signature are uploaded to the feed release.
import { createHash, createPrivateKey, sign } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

import { ensureFeedRelease } from './publish-addon-feed.mjs';

const feedRepo = process.env.BRICK_FEED_REPO || 'IsogiE/Brick-Releases';
const feedTag = process.env.BRICK_FEED_TAG || 'addon-feed-v3';
const manifestName = 'extra-addons-manifest.json';
const signatureName = `${manifestName}.sig`;
const outputDir = resolve('.release/brick-extra-addons-feed');
const packageMaxBytes = 16 * 1024 * 1024;
const sourceMaxFiles = 2000;
const curseforgeRepos = 'https://repos.curseforge.com/wow/';
const userAgent = 'Brick extra addon feed publisher';
export const manifestKind = 'brick-extra-addons';

// Keep in sync with CATALOG in src/addon/extras.rs.
export const catalog = [
  { id: 'buggrabber', source: 'curseforge-tag', project: 'bug-grabber', folder: '!BugGrabber' },
  { id: 'bugsack', source: 'github-release', repo: 'funkydude/BugSack', folder: 'BugSack' },
  { id: 'addon-profiler', source: 'github-release', repo: 'NumyAddon/AddonProfiler', folder: '!!AddonProfiler' },
  {
    id: 'bigwigs', source: 'github-release', repo: 'BigWigsMods/BigWigs', folder: 'BigWigs',
    folderPrefix: 'BigWigs_', prereleases: true
  }
];

// BigWigs packager flavors from release.json mapped to Brick client flavors.
const retailFlavors = ['retail', 'ptr', 'xptr', 'beta'];
const classicFlavors = ['classic', 'classic-era', 'classic-ptr', 'classic-beta'];

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}

async function main() {
  const dryRun = process.argv.includes('--check');
  const ghToken = requiredEnvAny(['GH_TOKEN', 'GITHUB_TOKEN']);
  const existing = await fetchExistingManifest();
  const previous = new Map((existing?.kind === manifestKind ? existing.addons : []).map((entry) => [entry.id, entry]));
  const addons = [];
  const failures = [];

  for (const addon of catalog) {
    try {
      addons.push(await resolveEntry(addon, previous.get(addon.id), ghToken));
    } catch (error) {
      // One broken upstream must not hold back updates to the others; keep
      // its last published entry and fail the run so the problem is visible.
      failures.push(`${addon.id}: ${error instanceof Error ? error.message : error}`);
      if (previous.has(addon.id)) addons.push(previous.get(addon.id));
    }
  }

  const manifest = { schema: 1, kind: manifestKind, addons };
  if (sameAddons(existing, manifest)) {
    console.log('Extra addon feed is already current.');
  } else if (dryRun) {
    console.log(JSON.stringify(manifest, null, 2));
  } else {
    publish(manifest, ghToken);
  }
  if (failures.length > 0) {
    throw new Error(`Some extra addons could not be refreshed:\n${failures.join('\n')}`);
  }
}

async function resolveEntry(addon, current, ghToken) {
  if (addon.source === 'github-release') {
    const release = selectRelease(addon, ghReleases(addon, ghToken));
    const asset = selectPackageAsset(release);
    if (isCurrentGitHubEntry(current, addon, release.tag_name, asset.browser_download_url)) return current;
    const entry = await buildGitHubEntry(addon, release, asset);
    console.log(`${addon.id}: ${current?.version ?? 'none'} -> ${entry.version}`);
    return entry;
  }
  if (addon.source === 'curseforge-tag') {
    const base = `${curseforgeRepos}${addon.project}/tags/`;
    const tag = newestVersionTag(listingEntries(await fetchText(base)));
    if (!tag) throw new Error(`No version tags found at ${base}.`);
    if (isCurrentSourceEntry(current, addon, tag)) return current;
    const entry = await buildSourceEntry(addon, tag);
    console.log(`${addon.id}: ${current?.version ?? 'none'} -> ${entry.version}`);
    return entry;
  }
  throw new Error(`Unknown source ${addon.source}.`);
}

function ghReleases(addon, ghToken) {
  const path = addon.prereleases ? `repos/${addon.repo}/releases?per_page=20` : `repos/${addon.repo}/releases/latest`;
  const result = JSON.parse(gh(['api', path], ghToken));
  return Array.isArray(result) ? result : [result];
}

export function selectRelease(addon, releases) {
  const candidates = releases
    .filter((release) => release && !release.draft && (addon.prereleases || !release.prerelease))
    .sort((a, b) => Date.parse(b.published_at || b.created_at) - Date.parse(a.published_at || a.created_at));
  if (candidates.length === 0) throw new Error(`${addon.repo} has no usable release.`);
  return candidates[0];
}

export function selectPackageAsset(release) {
  const zips = (release.assets || []).filter((asset) =>
    /\.zip$/i.test(asset.name) && !/-nolib\.zip$/i.test(asset.name));
  if (zips.length !== 1) {
    throw new Error(`Expected exactly one packaged zip in ${release.tag_name}, found ${zips.length}.`);
  }
  const [asset] = zips;
  if (!(asset.size > 0 && asset.size <= packageMaxBytes)) {
    throw new Error(`${asset.name} has an unsupported size ${asset.size}.`);
  }
  return asset;
}

export function isCurrentGitHubEntry(entry, addon, version, url) {
  return Boolean(
    entry
      && entry.id === addon.id
      && entry.version === version
      && entry.source?.provider === 'github-release'
      && entry.source?.repo === addon.repo
      && entry.artifact?.url === url
      && /^[a-f0-9]{64}$/.test(entry.artifact.sha256)
      && entry.artifact.size > 0
  );
}

export function isCurrentSourceEntry(entry, addon, version) {
  return Boolean(
    entry
      && entry.id === addon.id
      && entry.version === version
      && entry.source?.provider === 'curseforge-tag'
      && entry.source?.project === addon.project
      && Array.isArray(entry.artifact?.files)
      && entry.artifact.files.length > 0
  );
}

export function sameAddons(existing, manifest) {
  return Boolean(existing)
    && existing.schema === manifest.schema
    && existing.kind === manifest.kind
    && JSON.stringify(existing.addons) === JSON.stringify(manifest.addons);
}

export function mapFlavors(releaseJson) {
  const flavors = new Set(
    (releaseJson?.releases || []).flatMap((entry) => (entry.metadata || []).map((meta) => meta.flavor))
  );
  if (flavors.size === 0) return [];
  const mapped = [];
  if (flavors.has('mainline')) mapped.push(...retailFlavors);
  if ([...flavors].some((flavor) => flavor !== 'mainline')) mapped.push(...classicFlavors);
  return mapped;
}

export function folderAllowed(addon, folder) {
  return folder === addon.folder
    || Boolean(addon.folderPrefix && folder.length > addon.folderPrefix.length
      && folder.startsWith(addon.folderPrefix) && /^[A-Za-z0-9_-]+$/.test(folder));
}

// Returns the package's top-level folders; every entry must sit inside one
// the addon owns.
export function packageFolders(names, addon) {
  if (names.length === 0) throw new Error('Package zip is empty.');
  const folders = new Set();
  for (const name of names) {
    const parts = name.split('/');
    if (parts.length < 2 || !folderAllowed(addon, parts[0]) || parts.includes('..') || name.includes('\\')) {
      throw new Error(`Package entry ${name} is outside ${addon.folder}.`);
    }
    folders.add(parts[0]);
  }
  if (!folders.has(addon.folder)) throw new Error(`Package does not contain ${addon.folder}/.`);
  return [addon.folder, ...[...folders].filter((folder) => folder !== addon.folder).sort()];
}

export function tocVersion(toc) {
  for (const line of toc.replace(/^﻿/, '').split(/\r?\n/)) {
    const match = /^##\s*Version\s*:\s*(.*)$/i.exec(line.trim());
    if (match) return match[1].trim();
  }
  return '';
}

export async function buildGitHubEntry(addon, release, asset) {
  const zip = await download(asset.browser_download_url, packageMaxBytes);
  if (zip.length !== asset.size) {
    throw new Error(`${asset.name} size mismatch: release says ${asset.size}, downloaded ${zip.length}.`);
  }
  const dir = mkdtempSync(join(tmpdir(), 'brick-extra-'));
  let folders;
  try {
    const zipPath = join(dir, 'package.zip');
    writeFileSync(zipPath, zip);
    const names = execFileSync('unzip', ['-Z1', zipPath], { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 })
      .split('\n').filter(Boolean);
    folders = packageFolders(names, addon);
    for (const folder of folders) {
      const toc = execFileSync('unzip', ['-p', zipPath, `${folder}/${folder}.toc`], { encoding: 'utf8' });
      const version = tocVersion(toc);
      if (version !== release.tag_name) {
        throw new Error(`${folder} TOC version ${version || '(none)'} does not match ${release.tag_name}.`);
      }
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }

  const releaseJsonAsset = (release.assets || []).find((candidate) => candidate.name === 'release.json');
  const releaseJson = releaseJsonAsset
    ? JSON.parse((await download(releaseJsonAsset.browser_download_url, 64 * 1024)).toString('utf8'))
    : null;

  return {
    id: addon.id,
    version: release.tag_name,
    source: { provider: 'github-release', repo: addon.repo, tag: release.tag_name, prerelease: Boolean(release.prerelease) },
    artifact: {
      url: asset.browser_download_url,
      sha256: createHash('sha256').update(zip).digest('hex'),
      size: zip.length,
      folders,
      flavors: mapFlavors(releaseJson)
    }
  };
}

// Parses an Apache Subversion directory listing into entry names. Directories
// keep their trailing slash.
export function listingEntries(html) {
  return [...html.matchAll(/<li><a href="([^"]+)">/g)]
    .map((match) => decodeURIComponent(match[1].replaceAll('&amp;', '&')))
    .filter((name) => name !== '../');
}

export function newestVersionTag(entries) {
  const tags = entries
    .filter((name) => name.endsWith('/'))
    .map((name) => name.slice(0, -1))
    .filter((name) => /^v?\d+(\.\d+)*$/.test(name));
  const key = (tag) => tag.replace(/^v/, '').split('.').map(Number);
  tags.sort((a, b) => {
    const [x, y] = [key(a), key(b)];
    for (let i = 0; i < Math.max(x.length, y.length); i += 1) {
      if ((x[i] ?? 0) !== (y[i] ?? 0)) return (x[i] ?? 0) - (y[i] ?? 0);
    }
    return 0;
  });
  return tags.at(-1);
}

// The client copies tag files as they are, filling in only the version token,
// so any other packager directive means the files need a real package build.
const packagerMarker = /@(?:end-)?(?:debug|alpha|non-alpha|do-not-package|retail|non-retail|version-[a-z-]+|non-version-[a-z-]+|toc-[a-z-]+|project-(?!version@)[a-z-]+|file-[a-z-]+)@/;

export function checkSourceFile(path, bytes) {
  if (!/\.(toc|lua|xml)$/i.test(path)) return;
  const marker = packagerMarker.exec(bytes.toString('utf8'));
  if (marker) throw new Error(`${path} needs packaging (${marker[0]}).`);
}

// Reads .pkgmeta well enough to refuse projects that need a real package
// build and to honour its ignore list.
export function pkgmetaIgnores(pkgmeta) {
  if (!pkgmeta) return [];
  if (/^(externals|move-folders)\s*:/m.test(pkgmeta)) {
    throw new Error('.pkgmeta uses externals or move-folders, which need a package build.');
  }
  const block = /^ignore\s*:\s*\n((?:[ \t]+-.*\n?)+)/m.exec(pkgmeta);
  return block ? block[1].split('\n').map((line) => line.replace(/^\s*-\s*/, '').trim()).filter(Boolean) : [];
}

export async function buildSourceEntry(addon, tag) {
  const base = `${curseforgeRepos}${addon.project}/tags/${tag}/`;
  const root = listingEntries(await fetchText(base));
  const pkgmeta = root.includes('.pkgmeta') ? await fetchText(`${base}.pkgmeta`) : '';
  const ignores = pkgmetaIgnores(pkgmeta);
  const relativePaths = [];
  const walk = async (prefix, names) => {
    for (const name of names) {
      if (name.startsWith('.')) continue;
      const relative = `${prefix}${name}`;
      if (ignores.some((ignored) => relative === ignored || relative.startsWith(`${ignored.replace(/\/$/, '')}/`))) continue;
      if (name.endsWith('/')) {
        await walk(relative, listingEntries(await fetchText(`${base}${encodePath(relative)}`)));
      } else {
        relativePaths.push(relative);
      }
      if (relativePaths.length > sourceMaxFiles) throw new Error('Too many source files.');
    }
  };
  await walk('', root);
  if (!relativePaths.includes(`${addon.folder}.toc`)) throw new Error(`Tag ${tag} has no ${addon.folder}.toc.`);

  const files = [];
  let total = 0;
  for (const relative of relativePaths.sort()) {
    const url = `${base}${encodePath(relative)}`;
    const bytes = await download(url, packageMaxBytes);
    checkSourceFile(relative, bytes);
    total += bytes.length;
    if (total > packageMaxBytes) throw new Error('Source files exceed the package size limit.');
    files.push({
      path: `${addon.folder}/${relative}`,
      url,
      sha256: createHash('sha256').update(bytes).digest('hex'),
      size: bytes.length
    });
  }

  return {
    id: addon.id,
    version: tag,
    source: { provider: 'curseforge-tag', project: addon.project, tag },
    artifact: { files, folders: [addon.folder], flavors: [] }
  };
}

// Matches how the client resolves each path against the tag URL.
export function encodePath(relative) {
  return new URL(relative, 'https://example.invalid/').pathname.slice(1);
}

function publish(manifest, ghToken) {
  const privateKey = createPrivateKey({
    key: Buffer.from(requiredEnv('BRICK_ADDON_PRIVATE_KEY_B64'), 'base64'),
    format: 'der',
    type: 'pkcs8'
  });
  const manifestJson = `${JSON.stringify(manifest, null, 2)}\n`;
  mkdirSync(outputDir, { recursive: true });
  const manifestPath = join(outputDir, manifestName);
  const signaturePath = join(outputDir, signatureName);
  writeFileSync(manifestPath, manifestJson);
  writeFileSync(signaturePath, `${sign(null, Buffer.from(manifestJson), privateKey).toString('base64')}\n`);

  ensureFeedRelease(ghToken, (args, token, stdio) => gh(args, token, stdio));
  gh(['release', 'upload', feedTag, manifestPath, signaturePath, '--repo', feedRepo, '--clobber'], ghToken, 'inherit');
  console.log(`Published extra addon manifest to ${feedRepo}@${feedTag}.`);
}

async function download(url, maxBytes) {
  const response = await fetch(url, { headers: { 'user-agent': userAgent } });
  if (!response.ok) throw new Error(`Download failed: HTTP ${response.status} for ${url}.`);
  const body = Buffer.from(await response.arrayBuffer());
  if (body.length > maxBytes) throw new Error(`${url} exceeds ${maxBytes} bytes.`);
  return body;
}

async function fetchText(url) {
  return (await download(url, 4 * 1024 * 1024)).toString('utf8');
}

async function fetchExistingManifest() {
  const url = `https://github.com/${feedRepo}/releases/download/${feedTag}/${manifestName}?brickFeedCheck=${Date.now()}`;
  const response = await fetch(url, { headers: { accept: 'application/json', 'user-agent': userAgent } });
  if (response.status === 404) return null;
  if (!response.ok) throw new Error(`Existing manifest request failed: HTTP ${response.status}.`);
  return response.json();
}

function gh(args, ghToken, stdio = 'pipe') {
  return execFileSync('gh', args, { encoding: 'utf8', env: { ...process.env, GH_TOKEN: ghToken }, stdio });
}

function requiredEnv(name) {
  if (!process.env[name]) throw new Error(`${name} is required.`);
  return process.env[name];
}

function requiredEnvAny(names) {
  for (const name of names) if (process.env[name]) return process.env[name];
  throw new Error(`${names.join(' or ')} is required.`);
}
