import assert from 'node:assert/strict';
import { mkdtempSync, copyFileSync, writeFileSync, rmSync, mkdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';

test('package manager policy accepts only pinned pnpm and rejects alternate lockfiles', () => {
  const root = mkdtempSync(join(tmpdir(), 'retrofeel-package-manager-'));
  const site = join(root, 'site');
  mkdirSync(site);
  try {
    for (const file of ['require-pnpm.mjs', 'package.json']) copyFileSync(join(import.meta.dirname, file), join(site, file));
    const run = agent => spawnSync(process.execPath, [join(site, 'require-pnpm.mjs')], {
      env: { ...process.env, npm_config_user_agent: agent }, encoding: 'utf8',
    });
    assert.equal(run('pnpm/11.27.0 npm/? node/v22.23.2 linux x64').status, 0);
    for (const agent of ['', 'npm/11.0.0', 'yarn/1.22.0', 'bun/1.3.0', 'pnpm/11.3.0']) {
      const result = run(agent);
      assert.equal(result.status, 1, agent);
      assert.match(result.stderr, /Use pnpm@11\.27\.0/);
    }
    for (const base of [root, site]) for (const file of ['package-lock.json', 'npm-shrinkwrap.json', 'yarn.lock', 'bun.lock', 'bun.lockb']) {
      const path = join(base, file);
      writeFileSync(path, '{}');
      const result = run('pnpm/11.27.0');
      assert.equal(result.status, 1, path);
      assert.match(result.stderr, /only supported JavaScript lockfile/);
      rmSync(path);
    }
  } finally { rmSync(root, { recursive: true, force: true }); }
});
