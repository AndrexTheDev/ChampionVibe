import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const cleaner = path.join(repositoryRoot, 'scripts', 'clean-untracked.sh');

function fixedEnvironment() {
  return {
    ...process.env,
    GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_CONFIG_SYSTEM: '/dev/null',
    GIT_AUTHOR_NAME: 'ChampionVibe test',
    GIT_AUTHOR_EMAIL: 'test@championvibe.invalid',
    GIT_COMMITTER_NAME: 'ChampionVibe test',
    GIT_COMMITTER_EMAIL: 'test@championvibe.invalid',
  };
}

function runGit(cwd, ...args) {
  return execFileSync('git', args, { cwd, encoding: 'utf8', env: fixedEnvironment() });
}

function runCleaner(cwd, ...args) {
  return execFileSync('bash', [cleaner, ...args], { cwd, encoding: 'utf8', env: fixedEnvironment() });
}

// Tracked files and one ignored directory: the cleaner must respect both.
function makeRepository() {
  const directory = mkdtempSync(path.join(tmpdir(), 'championvibe-clean-'));
  runGit(directory, 'init', '--quiet');
  writeFileSync(path.join(directory, 'tracked.txt'), 'tracked\n');
  writeFileSync(path.join(directory, '.gitignore'), 'ignored-dir/\n');
  runGit(directory, 'add', 'tracked.txt', '.gitignore');
  runGit(directory, 'commit', '--quiet', '-m', 'initial commit');
  return directory;
}

function withRepository(body) {
  const directory = makeRepository();
  try {
    body(directory);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

function writeIgnoredOutput(directory) {
  mkdirSync(path.join(directory, 'ignored-dir'));
  writeFileSync(path.join(directory, 'ignored-dir', 'cache.bin'), 'cache\n');
}

test('clean-untracked lists untracked paths without deleting anything by default', () => {
  withRepository((directory) => {
    writeFileSync(path.join(directory, 'scratch.txt'), 'scratch\n');
    const output = runCleaner(directory);
    assert.match(output, /scratch\.txt/u);
    assert.match(output, /Dry run/u);
    assert.ok(existsSync(path.join(directory, 'scratch.txt')));
  });
});

test('clean-untracked --force removes untracked paths and keeps tracked and ignored ones', () => {
  withRepository((directory) => {
    writeFileSync(path.join(directory, 'scratch.txt'), 'scratch\n');
    mkdirSync(path.join(directory, 'untracked-dir'));
    writeFileSync(path.join(directory, 'untracked-dir', 'leftover.bin'), 'leftover\n');
    writeIgnoredOutput(directory);
    const output = runCleaner(directory, '--force');
    assert.match(output, /Removed 2 untracked path/u);
    assert.equal(existsSync(path.join(directory, 'scratch.txt')), false);
    assert.equal(existsSync(path.join(directory, 'untracked-dir')), false);
    assert.equal(existsSync(path.join(directory, 'tracked.txt')), true);
    assert.equal(existsSync(path.join(directory, 'ignored-dir', 'cache.bin')), true);
    assert.equal(runGit(directory, 'status', '--porcelain').trim(), '');
  });
});

test('clean-untracked --include-ignored also removes ignored build output', () => {
  withRepository((directory) => {
    writeIgnoredOutput(directory);
    const output = runCleaner(directory, '--force', '--include-ignored');
    assert.match(output, /ignored-dir/u);
    assert.equal(existsSync(path.join(directory, 'ignored-dir')), false);
    assert.equal(existsSync(path.join(directory, 'tracked.txt')), true);
  });
});

test('clean-untracked refuses to run outside a Git work tree', () => {
  const directory = mkdtempSync(path.join(tmpdir(), 'championvibe-outside-'));
  try {
    assert.throws(() => runCleaner(directory, '--force'));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
