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
// Stage 6 of the @zeroship/db refactor replaces the legacy
// `_installSchema(schema, { installOnEnvDb: true })` shape with
// `installSchema(schema, env) → { collections, ready }`. The `ready`
// promise is captured directly from the return value and awaited
// before any request is dispatched — no `globalThis.__zeroshipPlatformReady`
// indirection.
//
// Top-level-await pattern: V8's module evaluation runs the dynamic
// `import("@zeroship/db")` synchronously through the microtask
// checkpoint that `load_modules` invokes after `module.evaluate()`,
// because `@zeroship/db` is bundle-resident (statically imported by
// user code that uses it). `installSchema` returns synchronously
// after planting collections + extensions on `env.db`; awaiting the
// returned `ready` promise gates module evaluation on DDL settling.
//
// Guards:
//   - `__zsBeginAutoTx` undefined → no DbPlugin registered on this
//     runtime. `installSchema` would throw "env.db not available";
//     skip silently to support dev runs without DATABASE_URL.
//   - `user.default.schema` not a plain object → skip; covers
//     RPC-only / fetch-only apps and the dev-bootstrap (whose own
//     `default` carries `{ fetch, rpc }` only — schema installs lazily
//     on first request via `maybeRegisterSchema`).
//
// Errors thrown by `installSchema` (validation, naming collisions)
// re-raise — module evaluation rejects, the runtime surfaces it as an
// init failure, and the worker refuses to serve until the bundle is
// re-deployed.
if (typeof globalThis.__zsBeginAutoTx === "function") {
  const schema = (user && user.default && typeof user.default === "object")
    ? user.default.schema
    : undefined;
  if (schema && typeof schema === "object") {
    const sdk = await import("@zeroship/db");
    if (typeof sdk.installSchema === "function") {
      // Resolve the live env.db handle off the runtime's composite
      // env object. `__zs_env()` is the bootstrap-visible helper
      // (`crates/runtime/src/core/init.rs::zs_env_callback`) that
      // returns the same v8::Global the request-path passes as the
      // second arg of `fetch(req, env, ctx)`. Pulling the native db
      // through it (instead of having the SDK look up `env.db`
      // itself) keeps the data-flow explicit: the bootstrap owns
      // the env, the SDK plants on whatever it's handed.
      //
      // The `__zsBeginAutoTx` gate already guarantees the DbPlugin
      // registered, which means env.db should be present. We pass
      // it through verbatim — the SDK throws a clear error if it
      // somehow received an undefined handle.
      const envObj = (typeof globalThis.__zs_env === "function")
        ? globalThis.__zs_env()
        : undefined;
      const envDb = envObj && envObj.db;
      const { ready } = sdk.installSchema(schema, envDb);
      // Await the DDL chain so the bootstrap module's top-level
      // promise doesn't resolve until registerModel has settled —
      // otherwise the worker accepts requests against an
      // unregistered schema and the auto-tx dispatcher's
      // defensive await is the only thing standing between user
      // code and a missing-table error.
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
