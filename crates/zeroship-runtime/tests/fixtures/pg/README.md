# pg fixture

`pg-8.16.3.bundle.mjs` is generated from unmodified `pg@8.16.3` with esbuild.
The bundle preserves runtime-owned Node builtins as `node:*` imports and uses
fixture-only shims for unused Node surfaces such as `.pgpass` filesystem lookup.

The headline test is `crates/zeroship-runtime/tests/node_pg_e2e.rs`. It runs against
the PostgreSQL server `zeroship_testkit::postgres::server` starts in Docker, and
fails loudly when that server cannot start.
