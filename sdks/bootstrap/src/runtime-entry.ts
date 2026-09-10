// Runtime-owned DB schema descriptor install (production path).
//
// Compiled to `dist/runtime-entry.js` and `include_str!`d by the
// runtime crate's `crates/zeroship-runtime/src/core/init.rs`, spliced into the
// bootstrap module so it runs INSIDE the module's top-level evaluation
// — between `import * as user from "./__user__.js"` and the
// `default.fetch` / `default.rpc` resolution. The post-build step
// (`scripts/post-build.mjs`) strips the `export {};` line so the file
// content is pure top-level JS suitable for splicing.
//
// Stage 7 of the @zeroship/db refactor moved `installSchema` into the
// `@zeroship/bootstrap` package. This entry dynamic-imports that package
// (runtime-provided — `crates/zeroship-runtime/src/core/bootstrap_modules.rs`
// satisfies the specifier, so the import resolves synchronously through
// the microtask checkpoint `load_modules` invokes after
// `module.evaluate()`).
//
// Schema install MUST NOT block module evaluation. `installSchema`
// plants the typed `Collection` wrappers on `env.db` SYNCHRONOUSLY (so
// `default.{fetch,rpc}` and `env.db.<collection>.find(...)` are live the
// instant evaluation completes). The async mask-policy flush resolves
// later. We stash that promise on `globalThis.__zsSchemaReady` and the shared dispatcher
// (`dispatcher.ts`) AWAITS it before running any procedure — mirroring
// the dev path (`dev-entry.ts`, which gates on `schemaReady` per request).
//
// Why not `await` here: the mask-policy flush does real async database I/O.
// A top-level `await` on it can leave the bootstrap module's evaluation
// PENDING after `load_modules`' single microtask checkpoint (which cannot
// drive the compio event loop). `default.fetch` / `default.rpc` would
// then be unread (exports unpopulated) and every dispatch 404s with
// "No default.fetch handler exported". Deferring readiness to the
// dispatch path keeps init synchronous and exports available. (ISS-66)
//
// Guards:
//   - `__zs_env()?.db` missing → no DbPlugin registered on this runtime.
//     `installSchema` would throw "env.db not available"; skip silently
//     to support dev runs without DATABASE_URL.
//   - `globalThis.__zsRuntimeDescriptor` absent → schema-less app; skip
//     silently and install no env.db collections.
//
// Errors from the synchronous `installSchema` call (validation, naming
// collisions) re-raise — module evaluation rejects, the runtime surfaces
// it as an init failure. Mask-policy errors surface on the first dispatch
// (the dispatcher awaits `__zsSchemaReady` and lets the rejection through
// to the RPC error envelope) — same as the dev path.

// Module marker — stripped by the post-build script. See dispatcher.ts
// for the same pattern.
export {};

declare const globalThis: {
  __zs_env?: () => { db?: unknown } | undefined;
  // **P9 §8** — the capability-handle resolver the runtime installs.
  // `runtime-entry` is the sole legitimate caller: it resolves the
  // `__platform` handle for the mask-policy flush, then DELETES this
  // global so no creator handler can reach it.
  __zsDbPlatform?: (db: unknown) => unknown;
  // Schema-readiness promise, set here for the mask-policy flush and
  // awaited by the shared dispatcher (`dispatcher.ts`) before running any
  // procedure. Keeps the policy flush off the module-eval critical path.
  __zsSchemaReady?: Promise<unknown>;
  // **Migration-first cutover (P5 S3)** — the bundled RuntimeSchemaDescriptor
  // v2 `{ version, collections }`, resolved from `manifest.runtime_descriptor`
  // and injected by the runtime (`crates/zeroship-runtime/src/core/init.rs::setup_globals`).
  // Present → the source of truth for schema install; absent → schema-less app.
  __zsRuntimeDescriptor?: Record<string, unknown>;
  [key: string]: unknown;
};

// **Migration-first cutover (P5 S3)** — use only the bundled
// RuntimeSchemaDescriptor the runtime injected as `globalThis.__zsRuntimeDescriptor`.
// v2 carries per-collection fields/options/indexes, each field additionally naming
// the physical columns it occupies (`storage`) and its read-surface capabilities.
const descriptor = globalThis.__zsRuntimeDescriptor;
const hasDescriptor =
  descriptor != null &&
  typeof descriptor === "object" &&
  Object.keys(descriptor).length > 0;
function runtimeDescriptorFields(value: Record<string, unknown> | undefined): Record<string, unknown> {
  if (
    value != null &&
    typeof value === "object" &&
    (value as { version?: unknown }).version === 2 &&
    (value as { collections?: unknown }).collections != null &&
    typeof (value as { collections?: unknown }).collections === "object" &&
    !Array.isArray((value as { collections?: unknown }).collections)
  ) {
    const out: Record<string, unknown> = {};
    for (const [name, collection] of Object.entries(
      (value as { collections: Record<string, unknown> }).collections,
    )) {
      if (collection == null || typeof collection !== "object" || Array.isArray(collection)) {
        throw new Error(`@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: collection ${JSON.stringify(name)} must be an object`);
      }
      const c = collection as Record<string, unknown>;
      if (c.fields == null || typeof c.fields !== "object" || Array.isArray(c.fields)) {
        throw new Error(`@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: collection ${JSON.stringify(name)} requires object field "fields"`);
      }
      if (c.options == null || typeof c.options !== "object" || Array.isArray(c.options)) {
        throw new Error(`@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: collection ${JSON.stringify(name)} requires object field "options"`);
      }
      const options = c.options as Record<string, unknown>;
      if (typeof options.softDelete !== "boolean" || typeof options.versioning !== "boolean") {
        throw new Error(`@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: collection ${JSON.stringify(name)} options requires boolean "softDelete" and "versioning"`);
      }
      if (!Array.isArray(c.indexes)) {
        throw new Error(`@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: collection ${JSON.stringify(name)} requires array field "indexes"`);
      }
      out[name] = c.fields;
    }
    return out;
  }
  throw new Error(
    "@zeroship/bootstrap: invalid RuntimeSchemaDescriptor: expected v2 object with { version: 2, collections }",
  );
}
// The object passed as installSchema's first arg is only the descriptor's field
// map. If the descriptor is present but not v2-shaped, throw: corrupt
// descriptors must never degrade to schema-less boots.
const schema = hasDescriptor ? runtimeDescriptorFields(descriptor) : undefined;
if (hasDescriptor && schema && typeof schema === "object") {
  // Resolve the live env.db handle off the runtime's composite env
  // object. `__zs_env()` is the bootstrap-visible helper
  // (`crates/zeroship-runtime/src/core/init.rs::zs_env_callback`) that returns
  // the same v8::Global the request-path passes as the second arg of
  // `fetch(req, env, ctx)`. DbPlugin registration is observable here as
  // the presence of `env.db`; `__zsDbPlatform` is intentionally not a
  // sentinel because the runtime installs that resolver on every isolate.
  const envObj = (typeof globalThis.__zs_env === "function")
    ? globalThis.__zs_env()
    : undefined;
  const envDb = envObj && envObj.db;

  if (envDb != null) {
    const sdk = await import("@zeroship/bootstrap/install-schema") as {
      installSchema?: (
        schema: unknown,
        env: unknown,
        options?: { descriptor?: unknown },
      ) => { collections: unknown };
    };
    if (typeof sdk.installSchema === "function") {
      // **P9 §8** — resolve the platform capability handle via the
      // runtime resolver, BEFORE we delete the global below. The handle
      // carries `setMaskPolicy`, which moved off `env.db`; the local
      // reference keeps the flush working after the resolver is gone.
      const plat = (typeof globalThis.__zsDbPlatform === "function" && envDb)
        ? globalThis.__zsDbPlatform(envDb)
        : undefined;

      // `installSchema` plants the Collection wrappers synchronously.
      sdk.installSchema(schema, envDb, {
        // **P5 S3** — the descriptor is the source of truth; _installSchemaInner
        // reads it and ignores the first arg for options.
        descriptor: hasDescriptor ? descriptor : undefined,
      });

      // Keep only the asynchronous mask-policy flush off the module-eval
      // critical path. The shared dispatcher awaits it before the first
      // procedure runs.
      globalThis.__zsSchemaReady = (async () => {
        // Seal the app declaration before dispatch. Install an empty policy
        // when none was declared so runtime code cannot add one later.
        const policyMod = await import("@zeroship/db/internal") as {
          _flushPendingMaskPolicy?: () => Record<string, readonly string[]> | null;
        };
        const pending = typeof policyMod._flushPendingMaskPolicy === "function"
          ? policyMod._flushPendingMaskPolicy()
          : null;
        const setMaskPolicy = (plat as { setMaskPolicy?: unknown } | undefined)?.setMaskPolicy;
        if (typeof setMaskPolicy === "function") {
          // Call via `.call(plat, ...)` so the v8_class brand check
          // sees the right receiver.
          await (setMaskPolicy as (
            this: typeof plat,
            p: Record<string, readonly string[]>,
          ) => Promise<unknown>).call(plat, pending ?? {});
        }
      })();
    }
  }
}

// **P9 §8** — capability boundary close-out. The platform handle has
// been used to start the mask flush; the resolver global is no longer
// needed. Delete it so no creator `fetch` /
// `rpc` handler — which runs only AFTER this module evaluation
// completes — can call `globalThis.__zsDbPlatform(env.db)` to fish the
// handle out of the private slot. The handle itself remains live (held
// by `env.db`'s private symbol); only the JS-reachable resolver is
// removed.
//
// Runs UNCONDITIONALLY (outside the `env.db` / schema guards):
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
