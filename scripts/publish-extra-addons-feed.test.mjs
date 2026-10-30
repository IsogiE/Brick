import assert from 'node:assert/strict';
import test from 'node:test';
import { deflateRawSync } from 'node:zlib';
import {
  catalog, checkSourceFile, encodePath, isCurrentGitHubEntry, isCurrentSourceEntry, listingEntries,
  mapFlavors, newestVersionTag, packageFolders, pkgmetaIgnores, readZipEntry, sameAddons,
  selectPackageAsset, selectRelease, tocVersion, uploadReleaseAsset, zipEntries
} from './publish-extra-addons-feed.mjs';

// Builds a small zip with one stored and the rest deflated entries.
function makeZip(files) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  files.forEach(([name, text], index) => {
    const raw = Buffer.from(text);
    const method = index === 0 ? 0 : 8;
    const data = method === 0 ? raw : deflateRawSync(raw);
    const nameBytes = Buffer.from(name);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(raw.length, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(raw.length, 24);
    central.writeUInt16LE(nameBytes.length, 28);
    central.writeUInt32LE(offset, 42);
    locals.push(local, nameBytes, data);
    centrals.push(central, nameBytes);
    offset += 30 + nameBytes.length + data.length;
  });
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(files.length, 8);
  end.writeUInt16LE(files.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

const byId = (id) => catalog.find((addon) => addon.id === id);
const asset = (name, size = 1000) => ({ name, size, browser_download_url: `https://github.com/x/y/releases/download/v1/${name}` });

test('picks the single packaged zip and ignores nolib and metadata assets', () => {
  const release = { tag_name: 'v1', assets: [asset('BugSack-v1.zip'), asset('BugSack-v1-nolib.zip'), asset('release.json')] };
  assert.equal(selectPackageAsset(release).name, 'BugSack-v1.zip');
  assert.throws(() => selectPackageAsset({ tag_name: 'v1', assets: [asset('a.zip'), asset('b.zip')] }));
  assert.throws(() => selectPackageAsset({ tag_name: 'v1', assets: [asset('a.zip', 64 * 1024 * 1024)] }));
});

test('prereleases are followed only for addons that opt in, newest first', () => {
  const releases = [
    { tag_name: 'v2-alpha', prerelease: true, published_at: '2026-10-29T00:00:00Z' },
    { tag_name: 'v1', prerelease: false, published_at: '2026-10-20T00:00:00Z' },
    { tag_name: 'v3', draft: true, published_at: '2026-10-30T00:00:00Z' }
  ];
  assert.equal(selectRelease(byId('bigwigs'), releases).tag_name, 'v2-alpha');
  assert.equal(selectRelease(byId('bugsack'), releases).tag_name, 'v1');
  assert.throws(() => selectRelease(byId('bugsack'), [releases[0], releases[2]]));
});

test('package folders stay inside the addon and its module prefix', () => {
  assert.deepEqual(packageFolders(['BugSack/', 'BugSack/BugSack.toc'], byId('bugsack')), ['BugSack']);
  assert.deepEqual(
    packageFolders(['BigWigs_Core/a.lua', 'BigWigs/BigWigs.toc', 'BigWigs_Options/b.lua'], byId('bigwigs')),
    ['BigWigs', 'BigWigs_Core', 'BigWigs_Options']
  );
  for (const names of [['BugSack/x', 'Other/x'], ['BugSack/../x'], ['x.lua'], ['BugSack_Extra/x'], []]) {
    assert.throws(() => packageFolders(names, byId('bugsack')), String(names));
  }
  assert.throws(() => packageFolders(['BigWigs_Core/a.lua'], byId('bigwigs')));
});

test('reads TOC versions and maps packager flavors', () => {
  assert.equal(tocVersion('﻿## Title: X\r\n## Version: v12.1.9\r\n'), 'v12.1.9');
  assert.equal(tocVersion('## Title: X\n'), '');
  assert.deepEqual(mapFlavors({ releases: [{ metadata: [{ flavor: 'mainline' }] }] }), ['retail', 'ptr', 'xptr', 'beta']);
  assert.equal(mapFlavors({ releases: [{ metadata: [{ flavor: 'mainline' }, { flavor: 'mists' }] }] }).length, 8);
  assert.deepEqual(mapFlavors(null), []);
});

test('parses Subversion listings and picks the newest numeric tag', () => {
  const html = '<ul>\n<li><a href="../">..</a></li>\n<li><a href="%21BugGrabber.toc">!BugGrabber.toc</a></li>\n'
    + '<li><a href="v12.0.21/">v12.0.21/</a></li>\n<li><a href="v12.1.1/">v12.1.1/</a></li>\n'
    + '<li><a href="v12.1.0/">v12.1.0/</a></li>\n<li><a href="r223-release/">r223-release/</a></li>\n'
    + '<li><a href="v9.9.9/">v9.9.9/</a></li>\n<li><a href="3.0-beta/">3.0-beta/</a></li>\n</ul>';
  const entries = listingEntries(html);
  assert.ok(entries.includes('!BugGrabber.toc'));
  assert.ok(!entries.includes('../'));
  assert.equal(newestVersionTag(entries), 'v12.1.1');
  assert.equal(newestVersionTag(['BugGrabber.lua']), undefined);
  assert.equal(encodePath('!BugGrabber.toc'), '!BugGrabber.toc');
  assert.equal(encodePath('Locales/de DE.lua'), 'Locales/de%20DE.lua');
});

test('source tags that need a real package build are refused', () => {
  checkSourceFile('!BugGrabber/!BugGrabber.toc', Buffer.from('## Version: @project-version@\n'));
  checkSourceFile('icon.tga', Buffer.from('--@debug@'));
  for (const text of ['--@debug@\nprint(1)\n--@end-debug@', '## Interface: @toc-version-retail@', '--@non-retail@']) {
    assert.throws(() => checkSourceFile('x.lua', Buffer.from(text)), text);
  }
  assert.deepEqual(pkgmetaIgnores('package-as: !BugGrabber\nignore:\n  - README.md\n  - Docs\n'), ['README.md', 'Docs']);
  assert.deepEqual(pkgmetaIgnores(''), []);
  assert.throws(() => pkgmetaIgnores('externals:\n  Libs/LibStub: https://example\n'));
  assert.throws(() => pkgmetaIgnores('move-folders:\n  A/B: B\n'));
});

test('unchanged upstream versions reuse the published entry without downloading', () => {
  const bugsack = byId('bugsack');
  const url = 'https://github.com/funkydude/BugSack/releases/download/v12.1.9/BugSack-v12.1.9.zip';
  const entry = {
    id: 'bugsack', version: 'v12.1.9', source: { provider: 'github-release', repo: bugsack.repo },
    artifact: { url, sha256: 'a'.repeat(64), size: 10 }
  };
  assert.equal(isCurrentGitHubEntry(entry, bugsack, 'v12.1.9', url), true);
  assert.equal(isCurrentGitHubEntry(entry, bugsack, 'v12.2.0', url), false);
  assert.equal(isCurrentGitHubEntry(entry, byId('bigwigs'), 'v12.1.9', url), false);

  const grabber = byId('buggrabber');
  const source = {
    id: 'buggrabber', version: 'v12.1.1', source: { provider: 'curseforge-tag', project: 'bug-grabber' },
    artifact: { files: [{}] }
  };
  assert.equal(isCurrentSourceEntry(source, grabber, 'v12.1.1'), true);
  assert.equal(isCurrentSourceEntry(source, grabber, 'v12.1.2'), false);
  assert.equal(isCurrentSourceEntry({ ...source, artifact: { files: [] } }, grabber, 'v12.1.1'), false);

  const manifest = { schema: 1, kind: 'brick-extra-addons', addons: [entry] };
  assert.equal(sameAddons(structuredClone(manifest), manifest), true);
  assert.equal(sameAddons(null, manifest), false);
  assert.equal(sameAddons({ ...manifest, addons: [] }, manifest), false);
});

test('packages are listed and their TOCs read in memory', () => {
  const zip = makeZip([
    ['BigWigs/BigWigs.toc', '## Version: v427.8\n'],
    ['BigWigs_Core/BigWigs_Core.toc', '## Title: Core\n## Version: v427.8\n'],
    ['BigWigs_Core/Core.lua', 'print("x")'.repeat(50)]
  ]);
  const entries = zipEntries(zip);
  assert.deepEqual(packageFolders(entries.map((entry) => entry.name), byId('bigwigs')), ['BigWigs', 'BigWigs_Core']);
  for (const entry of entries.filter((candidate) => candidate.name.endsWith('.toc'))) {
    assert.equal(tocVersion(readZipEntry(zip, entry).toString('utf8')), 'v427.8');
  }
  assert.equal(readZipEntry(zip, entries[2]).toString(), 'print("x")'.repeat(50));
  assert.throws(() => zipEntries(Buffer.from('not a zip at all, definitely not')));
  assert.throws(() => zipEntries(zip.subarray(0, zip.length - 30)));
});

test('feed assets are replaced from memory without touching other assets', () => {
  const calls = [];
  const invoke = (args, token, stdio, input) => {
    calls.push({ args, input });
    if (args[1] === 'repos/IsogiE/Brick-Releases/releases/tags/addon-feed-v3') {
      return JSON.stringify({ id: 7, assets: [
        { id: 1, name: 'extra-addons-manifest.json' }, { id: 2, name: 'addon-manifest.json' }
      ] });
    }
    return '{}';
  };
  uploadReleaseAsset('extra-addons-manifest.json', '{"x":1}\n', 'token', invoke);
  assert.deepEqual(calls.map((call) => call.args.slice(0, 3)), [
    ['api', 'repos/IsogiE/Brick-Releases/releases/tags/addon-feed-v3'],
    ['api', '-X', 'DELETE'],
    ['api', '-X', 'POST']
  ]);
  assert.equal(calls[1].args[3], 'repos/IsogiE/Brick-Releases/releases/assets/1');
  assert.equal(calls[2].args[3], 'https://uploads.github.com/repos/IsogiE/Brick-Releases/releases/7/assets?name=extra-addons-manifest.json');
  assert.equal(calls[2].input, '{"x":1}\n');
});
