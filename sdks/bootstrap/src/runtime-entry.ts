// Runtime-owned DB schema auto-discovery (production path).
//
// Compiled to `dist/runtime-entry.js` and `include_str!`d by the
// runtime crate's `crates/runtime/src/core/init.rs`, spliced into the
// bootstrap module so it runs INSIDE the module's top-level evaluation
// — between `import * as user from "./__user__.js"` and the
// `default.fetch` / `default.rpc` resolution. The post-build step
// (`scripts/post-build.mjs`) strips the `export {};` line so the file
// content is pure top-level JS suitable for splicing.
//
// Stage 7 of the @zeroship/db refactor moved `installSchema` into the
// `@zeroship/bootstrap` package. This entry awaits the dynamic import
// of that package (bundle-resident; the bundle includes it because the
// Vite plugin's synthetic SSR entry side-effect-imports it).
//
// Top-level-await pattern: V8's module evaluation runs the dynamic
// `import("@zeroship/bootstrap")` synchronously through the microtask
// checkpoint that `load_modules` invokes after `module.evaluate()`,
// because the package is bundle-resident.
//
// Guards:
//   - `__zsBeginAutoTx` undefined → no DbPlugin registered on this
//     runtime. `installSchema` would throw "env.db not available";
//     skip silently to support dev runs without DATABASE_URL.
//   - `user.default.schema` not a plain object → skip; covers
//     RPC-only / fetch-only apps and the dev-bootstrap (whose own
//     `default` carries `{ fetch, rpc }` only — schema installs lazily
//     on first request via the dev-entry's path).
//
// Errors thrown by `installSchema` (validation, naming collisions)
// re-raise — module evaluation rejects, the runtime surfaces it as an
// init failure, and the worker refuses to serve until the bundle is
// re-deployed.

// Module marker — stripped by the post-build script. See dispatcher.ts
// for the same pattern.
export {};

declare const user: { default?: { schema?: unknown } };
declare const globalThis: {
  __zsBeginAutoTx?: unknown;
  __zs_env?: () => { db?: unknown } | undefined;
  [key: string]: unknown;
};

if (typeof globalThis.__zsBeginAutoTx === "function") {
  const schema = (user && user.default && typeof user.default === "object")
    ? (user.default as { schema?: unknown }).schema
    : undefined;
  if (schema && typeof schema === "object") {
    const sdk = await import("@zeroship/bootstrap/install-schema") as {
      installSchema?: (
        schema: unknown,
        env: unknown,
      ) => { collections: unknown; ready: Promise<void> };
    };
    if (typeof sdk.installSchema === "function") {
      // Resolve the live env.db handle off the runtime's composite env
      // object. `__zs_env()` is the bootstrap-visible helper
      // (`crates/runtime/src/core/init.rs::zs_env_callback`) that
      // returns the same v8::Global the request-path passes as the
      // second arg of `fetch(req, env, ctx)`. Pulling the native db
      // through it keeps the data-flow explicit.
      const envObj = (typeof globalThis.__zs_env === "function")
        ? globalThis.__zs_env()
        : undefined;
      const envDb = envObj && envObj.db;
      const { ready } = sdk.installSchema(schema, envDb);
      // Await the DDL chain so the bootstrap module's top-level
      // promise doesn't resolve until registerModel has settled.
      try {
        await ready;
      } catch (e) {
        const err = e as { message?: string };
        console.error(
          "[zeroship] schema DDL failed:",
          (err && err.message) ? err.message : String(e),
        );
        throw e;
      }

      // **P5.5 PR 5** — flush the pending mask policy (declared via
      // `defineMaskPolicy()` at app top-level) through the native
      // `setMaskPolicy` op. Single shot at boot — re-declares after
      // this point do not propagate to the platform until the next
      // worker cold start. A failure here surfaces as a rejected
      // module evaluation (same shape as the schema DDL failure
      // above) so a creator's typo in `defineMaskPolicy({...})` is
      // loud, not silent.
      try {
        const policyMod = await import("@zeroship/db/internal") as {
          _flushPendingMaskPolicy?: () => Record<string, readonly string[]> | null;
        };
        const pending = typeof policyMod._flushPendingMaskPolicy === "function"
          ? policyMod._flushPendingMaskPolicy()
          : null;
        if (pending) {
          const setMaskPolicy = (envDb as { setMaskPolicy?: unknown } | undefined)?.setMaskPolicy;
          if (typeof setMaskPolicy === "function") {
            // Call via `.call(envDb, ...)` so the v8_class brand
            // check sees the right receiver (mirrors the
            // `registerModel` pattern in `installSchema`).
            await (setMaskPolicy as (
              this: typeof envDb,
              p: Record<string, readonly string[]>,
            ) => Promise<unknown>).call(envDb, pending);
          }
        }
      } catch (e) {
        const err = e as { message?: string };
        console.error(
          "[zeroship] mask policy flush failed:",
          (err && err.message) ? err.message : String(e),
        );
        throw e;
      }
    }
  }
}
