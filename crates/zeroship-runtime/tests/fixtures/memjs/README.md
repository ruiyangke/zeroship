# memjs fixture

`memjs-1.3.2.bundle.mjs` is generated from unmodified `memjs@1.3.2`
with esbuild, using the package's normal built entrypoint.

The bundle preserves runtime-owned Node builtins as `node:*` imports via
fixture-only shims for `net`, `events`, and `util`. The driver package source
is not patched.

The e2e test is `crates/runtime/tests/node_memjs_e2e.rs`. It brings up
`memcached:1.6` on `127.0.0.1:11212` when no compatible server is already
reachable.
