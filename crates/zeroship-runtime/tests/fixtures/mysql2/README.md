# mysql2 fixture

`mysql2-3.14.1.bundle.mjs` is generated from unmodified `mysql2@3.14.1`
with esbuild, using the package's promise entrypoint.

The bundle preserves runtime-owned Node builtins as `node:*` imports and uses
fixture-only shims for ambient Node modules that are not part of the runtime
surface (`stream`, `process`, `dns`, `fs`, etc.). The driver package source is
not patched.

The e2e test is `crates/runtime/tests/node_mysql2_e2e.rs`. It brings up
`mysql:8` on `127.0.0.1:3307` when no compatible server is already reachable.

