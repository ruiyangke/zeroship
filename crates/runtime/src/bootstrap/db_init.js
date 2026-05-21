// Runtime-owned DB schema auto-discovery.
//
// Inlined into BOOTSTRAP_JS (`crates/runtime/src/core/init.rs`) so it
// runs INSIDE the bootstrap module's top-level evaluation — before the
// runtime resolves `default.fetch` / `default.rpc` off the namespace.
//
// Stage 5c of the ZS-standard refactor removes the
// `manifest.exports.schema` reliance. Schema is read directly off the
// loaded entry's `default.schema`. The Vite plugin's synthetic entry
// re-exports `_zsUserDefault.schema` on the bootstrap-visible
// `user.default.schema`; raw `.js` deploys with `export default
// { schema: {...} }` work without any tooling. No manifest field, no
// dynamic-import indirection — the schema lives on the module the
// bootstrap already imported as `user`.
//
// Top-level-await pattern: V8's module evaluation runs the dynamic
// `import("@zeroship/db")` synchronously through the microtask
// checkpoint that `load_modules` invokes after `module.evaluate()`,
// because `@zeroship/db` is bundle-resident (statically imported by
// user code that uses it). `_installSchema` returns synchronously
// after publishing the `__zeroshipPlatformReady` promise (the
// register-model DDL chain), so the auto-tx dispatcher in the synthetic
// entry can await that promise without racing.
//
// Guards:
//   - `__zsBeginAutoTx` undefined → no DbPlugin registered on this
//     runtime. `_installSchema` would throw "env.db not available";
//     skip silently to support dev runs without DATABASE_URL.
//   - `user.default.schema` not a plain object → skip; covers
//     RPC-only / fetch-only apps and the dev-bootstrap (whose own
//     `default` carries `{ fetch, rpc }` only — schema installs lazily
//     on first request via `maybeRegisterSchema`).
//
// Errors thrown by `_installSchema` (validation, naming collisions)
// re-raise — module evaluation rejects, the runtime surfaces it as an
// init failure, and the worker refuses to serve until the bundle is
// re-deployed.
if (typeof globalThis.__zsBeginAutoTx === "function") {
  const schema = (user && user.default && typeof user.default === "object")
    ? user.default.schema
    : undefined;
  if (schema && typeof schema === "object") {
    const sdk = await import("@zeroship/db");
    if (typeof sdk._installSchema === "function") {
      sdk._installSchema(schema, { installOnEnvDb: true });
      // `_installSchema` publishes `__zeroshipPlatformReady` synchronously.
      // Await it so the bootstrap module's top-level promise doesn't
      // resolve until DDL has settled — otherwise the worker accepts
      // requests against an unregistered schema and the auto-tx
      // dispatcher's defensive await is the only thing standing between
      // user code and a missing-table error.
      const ready = globalThis.__zeroshipPlatformReady;
      if (ready && typeof ready.then === "function") {
        try {
          await ready;
        } catch (e) {
          console.error(
            "[zeroship] schema DDL failed:",
            (e && e.message) ? e.message : String(e),
          );
          throw e;
        }
      }
    }
  }
}
