# Contributing

Keep changes focused and include reproduction steps and relevant verification.
Use fictional games and identifiers in tests and documentation. Do not commit
recordings, credentials, account mappings, user data, or generated build output.
Preserve copyright notices, upstream licenses, and the core license tracker.
See [development](docs/DEVELOPMENT.md) for commands and architecture.

Use pnpm exclusively for JavaScript tooling on local machines and Ubuntu Servers.
Use the version pinned in `site/package.json`, install with
`pnpm install --frozen-lockfile`, and use `pnpm run` / `pnpm exec` for commands.
Do not add npm, Yarn, or Bun lockfiles or bypass the package manager checks.
See [website tooling](site/README.md) for the dependency policy.
