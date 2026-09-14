import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const directory = import.meta.dirname;
const { packageManager } = JSON.parse(readFileSync(join(directory, 'package.json'), 'utf8'));
const expected = packageManager.replace('@', '/');
const actual = (process.env.npm_config_user_agent || '').split(' ')[0];
if (actual !== expected) {
  console.error(`Use ${packageManager} on local machines and servers: pnpm install --frozen-lockfile; pnpm run check.`);
  process.exit(1);
}
for (const base of [directory, join(directory, '..')]) {
  for (const name of ['package-lock.json', 'npm-shrinkwrap.json', 'yarn.lock', 'bun.lock', 'bun.lockb']) {
    if (existsSync(join(base, name))) {
      console.error(`Remove ${name}; pnpm-lock.yaml is the only supported JavaScript lockfile.`);
      process.exit(1);
    }
  }
}
