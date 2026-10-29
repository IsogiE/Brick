import { appendFileSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';

const sourceRepo = 'IsogiE/Brick';
const feedRepo = 'IsogiE/Brick-Releases';
export const REQUIRED_CHECKS = ['ubuntu-22.04', 'windows-latest', 'Rust security advisories', 'Analyze (rust)',
  'Analyze (c-cpp)', 'Analyze (javascript-typescript)', 'Analyze (python)', 'Analyze (actions)'];

// Decide whether this commit should be released. A release starts once every
// required check on current main has passed and its Cargo version is not yet
// published; each finishing source workflow re-asks, so the last one starts it.
export function planRelease({ event, trigger, commit, mainSha, cargoToml, checks, published }) {
  if (event === 'workflow_run') {
    if (trigger?.event !== 'push' || trigger?.head_branch !== 'main'
        || trigger?.head_repository?.full_name !== sourceRepo) return { release: false, reason: 'Not a push to main of Brick.' };
    if (trigger.conclusion !== 'success') return { release: false, reason: `${trigger.name} did not succeed.` };
  } else if (event !== 'workflow_dispatch') {
    return { release: false, reason: `Unsupported event ${event}.` };
  }
  if (!/^[a-f0-9]{40}$/.test(commit || '') || commit !== mainSha) return { release: false, reason: 'Main has moved on; the newer commit releases itself.' };
  const version = cargoToml.match(/^version = "((?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*))"$/m)?.[1];
  if (!version) return { release: false, reason: 'Cargo.toml has no stable release version.' };
  if (published) return { release: false, version, reason: `Brick ${version} is already published.` };
  for (const name of REQUIRED_CHECKS) {
    const latest = checks.filter(check => check.name === name && check.app?.id === 15368 && check.head_sha === commit)
      .sort((a, b) => b.id - a.id)[0];
    if (latest?.status !== 'completed') return { release: false, version, reason: `Waiting for ${name}.` };
    if (latest.conclusion !== 'success') return { release: false, version, reason: `${name} did not pass.` };
  }
  return { release: true, version, reason: `Releasing Brick ${version} from ${commit}.` };
}

function api(endpoint, token) {
  return JSON.parse(execFileSync('gh', ['api', endpoint], { encoding: 'utf8', env: { ...process.env, GH_TOKEN: token } }));
}

function main() {
  const event = process.env.GITHUB_EVENT_NAME;
  const payload = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
  const trigger = payload.workflow_run;
  const commit = event === 'workflow_run' ? trigger?.head_sha : process.env.GITHUB_SHA;
  const token = process.env.GH_TOKEN;
  const mainSha = api(`repos/${sourceRepo}/branches/main`, token).commit.sha;
  const cargoToml = Buffer.from(api(`repos/${sourceRepo}/contents/Cargo.toml?ref=${commit}`, token).content, 'base64').toString('utf8');
  const checks = api(`repos/${sourceRepo}/commits/${commit}/check-runs?per_page=100`, token).check_runs;
  const version = cargoToml.match(/^version = "([^"]+)"$/m)?.[1];
  let published = false;
  if (version) {
    try {
      const release = api(`repos/${feedRepo}/releases/tags/v${version}`, process.env.BRICK_FEED_TOKEN);
      published = !release.draft;
    } catch (error) {
      if (!/HTTP 404/.test(`${error.stderr || ''}${error.message}`)) throw error;
    }
  }
  const plan = planRelease({ event, trigger, commit, mainSha, cargoToml, checks, published });
  console.log(plan.reason);
  appendFileSync(process.env.GITHUB_OUTPUT, `release=${plan.release}\nversion=${plan.version || ''}\ncommit=${commit}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) main();
