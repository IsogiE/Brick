import { test } from 'node:test';
import assert from 'node:assert/strict';
import { REQUIRED_CHECKS, planRelease } from './plan-release.mjs';

const commit = 'a'.repeat(40);
const passing = REQUIRED_CHECKS.map((name, id) => ({ id, name, app: { id: 15368 }, head_sha: commit, status: 'completed', conclusion: 'success' }));
const trigger = { name: 'CI', event: 'push', head_branch: 'main', head_repository: { full_name: 'IsogiE/Brick' }, conclusion: 'success' };
const base = { event: 'workflow_run', trigger, commit, mainSha: commit, cargoToml: '[package]\nversion = "0.8.1"\n', checks: passing, published: false };

test('an unpublished version on green main releases', () => {
  assert.deepEqual(planRelease(base), { release: true, version: '0.8.1', reason: `Releasing Brick 0.8.1 from ${commit}.` });
  assert.equal(planRelease({ ...base, event: 'workflow_dispatch', trigger: undefined }).release, true);
});

test('nothing releases twice, early, from forks or from stale commits', () => {
  const cases = {
    published: { published: true },
    stale: { mainSha: 'b'.repeat(40) },
    prerelease: { cargoToml: 'version = "0.8.1-rc1"\n' },
    failedTrigger: { trigger: { ...trigger, conclusion: 'failure' } },
    pullRequest: { trigger: { ...trigger, event: 'pull_request' } },
    fork: { trigger: { ...trigger, head_repository: { full_name: 'someone/Brick' } } },
    otherEvent: { event: 'push' },
    pending: { checks: passing.map(check => check.name === 'windows-latest' ? { ...check, status: 'in_progress', conclusion: null } : check) },
    failed: { checks: passing.map(check => check.name === 'Analyze (rust)' ? { ...check, conclusion: 'failure' } : check) },
    missing: { checks: passing.slice(1) },
    otherCommit: { checks: passing.map(check => ({ ...check, head_sha: 'c'.repeat(40) })) },
    otherApp: { checks: passing.map(check => ({ ...check, app: { id: 1 } })) },
  };
  for (const [name, change] of Object.entries(cases)) {
    assert.equal(planRelease({ ...base, ...change }).release, false, name);
  }
});
