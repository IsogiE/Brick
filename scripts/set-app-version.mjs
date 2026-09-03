import { readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

const versionInput =
  process.env.BRICK_APP_VERSION
  || process.env.GITHUB_REF_NAME?.replace(/^v/, '')
  || null;

const rawVersion = versionInput?.replace(/^v/, '') || null;

if (!rawVersion) {
  if (process.env.CI) {
    throw new Error('BRICK_APP_VERSION or a v* SemVer tag is required in CI.');
  }
  console.warn('No app version provided; leaving local version unchanged.');
  process.exit(0);
}

if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(rawVersion)) {
  throw new Error(`Invalid app version "${rawVersion}". Use SemVer like 0.1.0 or 0.1.0-beta.1.`);
}

updateJson(resolve('package.json'), (config) => {
  config.version = rawVersion;
});
updateCargoVersion(resolve('Cargo.toml'), rawVersion);
updateCargoVersion(resolve('Packager.toml'), rawVersion);

console.log(`Configured Brick app version ${rawVersion}.`);

function updateJson(path, update) {
  const config = JSON.parse(readFileSync(path, 'utf8'));
  update(config);
  writeFileSync(path, `${JSON.stringify(config, null, 2)}\n`);
}

function updateCargoVersion(path, version) {
  const cargo = readFileSync(path, 'utf8');
  const next = cargo.replace(
    /^version = ".+"$/m,
    `version = "${version}"`
  );
  writeFileSync(path, next);
}
