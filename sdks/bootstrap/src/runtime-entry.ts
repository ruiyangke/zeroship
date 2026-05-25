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
  // **P9 §8** — the capability-handle resolver the runtime installs.
  // `runtime-entry` is the sole legitimate caller: it resolves the
  // `__platform` handle once here, hands it to `installSchema`, then
  // DELETES this global so no creator handler (which runs only after
  // module evaluation completes) can reach it.
  __zsDbPlatform?: (db: unknown) => unknown;
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
        options?: { platform?: unknown },
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

      // **P9 §8** — resolve the platform capability handle via the
      // runtime resolver, BEFORE we delete the global below. The handle
      // is the carrier for `registerModel` / `setMaskPolicy` (those
      // moved off `env.db`). Resolving once and passing it through
      // `installSchema` + the mask flush means the rest of this entry
      // works after the resolver is gone.
      const plat = (typeof globalThis.__zsDbPlatform === "function" && envDb)
        ? globalThis.__zsDbPlatform(envDb)
        : undefined;

      const { ready } = sdk.installSchema(schema, envDb, { platform: plat });
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
      //
      // **P9 §8** — `setMaskPolicy` moved off `env.db` to the
      // `__platform` handle. Call it on `plat` (resolved above), not on
      // `envDb`.
      try {
        const policyMod = await import("@zeroship/db/internal") as {
          _flushPendingMaskPolicy?: () => Record<string, readonly string[]> | null;
        };
        const pending = typeof policyMod._flushPendingMaskPolicy === "function"
          ? policyMod._flushPendingMaskPolicy()
          : null;
        if (pending) {
          const setMaskPolicy = (plat as { setMaskPolicy?: unknown } | undefined)?.setMaskPolicy;
          if (typeof setMaskPolicy === "function") {
            // Call via `.call(plat, ...)` so the v8_class brand check
            // sees the right receiver (mirrors the `registerModel`
            // pattern in `installSchema`).
            await (setMaskPolicy as (
              this: typeof plat,
              p: Record<string, readonly string[]>,
            ) => Promise<unknown>).call(plat, pending);
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

// **P9 §8** — capability boundary close-out. The platform handle has
// been handed to `installSchema` (and used for the mask flush); the
// resolver global is no longer needed. Delete it so no creator `fetch` /
// `rpc` handler — which runs only AFTER this module evaluation
// completes — can call `globalThis.__zsDbPlatform(env.db)` to fish the
// handle out of the private slot. The handle itself remains live (held
// by `env.db`'s private symbol); only the JS-reachable resolver is
// removed.
//
// Runs UNCONDITIONALLY (outside the `__zsBeginAutoTx` / schema guards):
// the runtime installs `__zsDbPlatform` on every isolate, so it must be
// cleared even on RPC-only / fetch-only apps that skipped the schema
// install above. Idempotent: a no-op if the runtime never installed it
// or a re-evaluation already cleared it.
try {
  delete globalThis.__zsDbPlatform;
} catch {
  // A non-configurable global (shouldn't happen — the runtime installs
  // it as a plain property) would throw in strict mode; swallow so the
  // boot doesn't fail on the cleanup step.
}
