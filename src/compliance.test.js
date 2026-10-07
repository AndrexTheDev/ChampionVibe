import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const checker = path.join(repositoryRoot, 'scripts', 'verify-cleared-model-policy.sh');

function runChecker() {
  return execFileSync('bash', [checker], {
    cwd: repositoryRoot,
    encoding: 'utf8',
    env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null' },
  });
}

test('the cleared-model compliance gate passes on the shipped tree', () => {
  const output = runChecker();
  assert.match(output, /All \d+ compliance checks passed\./u);
  assert.doesNotMatch(output, /^FAIL/u);
});

test('the compliance gate covers the required policy areas', () => {
  const output = runChecker();
  const required = [
    'registry declares championvibe-model-registry-v2',
    'registry policy is cleared-tier-only',
    'is not hosted at a forbidden source',
    'is in the cleared tier',
    'carries a 64-hex SHA-256 pin',
    'no separator weights or model archives are tracked in the repository',
    'the separator download path enforces the forbidden-source scan',
    'the separator download path enforces the cleared-tier-only policy',
    'the separator download path requires a verified SHA-256 pin',
    'the drop sampleboard ships the 10 committed CC0 WAVs',
    'names the TTS engine as a pronunciation approximation',
    'states that the voice is not authentic Patois',
    'Cargo.lock is checked into Git',
    'CI checks the workspace against the locked dependency graph',
    'CI fails on compiler warnings',
  ];
  for (const description of required) {
    assert.ok(
      output.includes(description),
      `compliance gate is missing the check: ${description}`,
    );
  }
});
