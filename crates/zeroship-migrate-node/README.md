# zero-migrate-node

The native N-API addon for zero-migrate: the V8-free Rust core that applies
portable migrations over a host-driven (pg / mysql2) database session. It ships a
small loader (`index.js`) that resolves the correct prebuilt binary for your
platform from one of the `zero-migrate-node-<triple>` optional dependencies.

You usually do not depend on this package directly. Install
[`zero-migrate-cli`](https://www.npmjs.com/package/zero-migrate-cli), which
depends on it and exposes the friendly `apply` / `plan` / `status` API and the
`zero-migrate` command.

## Supported platforms

Prebuilt binaries are published for:

- linux-x64-gnu, linux-arm64-gnu
- darwin-x64, darwin-arm64
- win32-x64-msvc

The right one is installed automatically as an optional dependency. To point at a
locally built `.node` (for development), set `ZERO_MIGRATE_ADDON_PATH` to its
path.

## Docs

See the [zero-migrate documentation](https://github.com/ruiyangke/zero-migrate/tree/main/docs).

## License

MIT
