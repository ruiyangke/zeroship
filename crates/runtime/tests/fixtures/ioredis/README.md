# ioredis fixture

`ioredis-5.6.1.bundle.mjs` is generated from unmodified `ioredis@5.6.1`
with esbuild, using the package's normal built entrypoint.

The bundle preserves runtime-owned Node builtins as `node:*` imports and uses
fixture-only shims for ambient Node modules that are not part of the runtime
surface (`stream`, `process`, `assert`, `tty`, etc.). The driver package source
is not patched.

The e2e test is `crates/runtime/tests/node_ioredis_e2e.rs`. It brings up
`redis:7` on `127.0.0.1:6391` when no compatible server is already reachable.

