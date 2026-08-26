# pg fixture

`pg-8.16.3.bundle.mjs` is generated from unmodified `pg@8.16.3` with esbuild.
The bundle preserves runtime-owned Node builtins as `node:*` imports and uses
fixture-only shims for unused Node surfaces such as `.pgpass` filesystem lookup.

The headline test is `crates/runtime/tests/node_pg_e2e.rs`. It fails loudly when
the live migrate Postgres on `127.0.0.1:5440` is unavailable.
