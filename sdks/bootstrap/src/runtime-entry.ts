// Production mask-policy handoff after creator evaluation.
//
// The runtime embeds this entry while the native policy finalization cutover
// is in progress. DbPlugin prepares SDK collections before creator evaluation;
// this entry still seals the JavaScript policy declaration and starts the
// existing asynchronous native policy handoff. Dispatchers await its readiness
// promise. Vite's deferred development entry owns its separate policy handoff.
// Compiled to `dist/runtime-entry.js` and embedded by the runtime's
// `crates/zeroship-runtime/src/core/init.rs` inside the host bootstrap module.
// The post-build step strips `export {};` for top-level splicing.
//
// The runtime supplies zeroship:db/internal independently of creator artifacts.

// Module marker — stripped by the post-build script. See dispatcher.ts
// for the same pattern.
export {};

// Private bootstrap-module binding emitted by the Rust host. Creator globals
// cannot turn a production runtime into a deferred dev entry.
declare const __zsAllowDeferredSchemaInstall: boolean;
declare const __zsInstallDbMaskPolicy: (
  policy: Record<string, readonly string[]>,
) => Promise<void>;

declare const globalThis: {
  __zs_env?: () => { db?: unknown } | undefined;
  // The internal runtime bridge consumes this before creator evaluation.
  // It remains declared here only for unconditional defensive cleanup.
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
// Vite's dev entry captured the private platform resolver during evaluation
// and owns installation after importing the creator module. Sealing here would
// freeze the default policy before the app could declare its startup policy.
const deferredInstall = __zsAllowDeferredSchemaInstall && globalThis.__zsDeferSchemaInstall === true;
delete globalThis.__zsDeferSchemaInstall;
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
    const out = Object.create(null) as Record<string, unknown>;
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
// Validate the descriptor before resolving its native handle. A corrupt
// descriptor must never degrade to a schema-less boot.
const schema = hasDescriptor ? runtimeDescriptorFields(descriptor) : undefined;
if (!deferredInstall && hasDescriptor && schema && typeof schema === "object") {
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
    // The native DB plugin installs SDK collections before creator evaluation.
    // Policy declaration sealing still follows creator evaluation here until
    // the native policy finalization hook owns this remaining handoff.
    globalThis.__zsSchemaReady = (async () => {
      const policyMod = await import("zeroship:db/internal") as {
        _flushPendingMaskPolicy?: () => Record<string, readonly string[]> | null;
      };
      const pending = typeof policyMod._flushPendingMaskPolicy === "function"
        ? policyMod._flushPendingMaskPolicy()
        : null;
      await __zsInstallDbMaskPolicy(pending ?? {});
    })();
  }
}

// The internal bridge already removes the resolver before creator evaluation
// in production. Keep this unconditional cleanup for schema-less and dev boots.
try {
  delete globalThis.__zsDbPlatform;
} catch {
  // The runtime installs a configurable property; cleanup stays best-effort.
}
