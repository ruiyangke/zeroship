/**
 * Dev-mode entry — consolidates the dispatch + schema-install
 * coordination the Vite plugin's `dev-bootstrap` used to inline.
 *
 * The caller supplies a `loadUserModule()` thunk that re-imports the
 * user module per request (HMR may have invalidated cached
 * evaluations) and the `envDb` getter (so the dev entry doesn't
 * hard-code the runtime's `__zs_env()` indirection). This module owns:
 *
 *   1. Lazy schema install via `installSchema(schema, env.db)` on first
 *      request. Going top-level-await on the user import would block
 *      dev startup on potentially-failing user code; lazy is the right
 *      tradeoff for dev.
 *   2. Per-call normalization via `normalizeUserModule(mod, registry)`
 *      so HMR replacements land naturally — the registry captures the
 *      transform's `__register` side-effects and merges last
 *      (last-write-wins).
 *   3. Function-shape `default.rpc` for HMR. Dev's namespace may
 *      change per request; the dict-shape captured at module-init
 *      would go stale on every edit. The dispatcher (`__zsDispatch`)
 *      consumes the dict each call.
 *   4. WinterCG `default.fetch` via `createFetchHandler(...)` — routes
 *      `/_zs/v1/<id>` through the dispatcher; falls through to user's
 *      own `default.fetch` for non-RPC paths.
 *
 * The Vite plugin's `dev-bootstrap/index.ts` (post-Stage-7) is a thin
 * shell: it constructs the ModuleRunner + registry, calls `devEntry`,
 * and re-exports the result as `default`.
 *
 * NOTE: this module imports `./dispatcher.js` for its side effect so
 * the same `__zsDispatch` function the runtime splices in production
 * is installed on the dev isolate too. Single implementation; no
 * drift.
 */

import "./dispatcher.js";
import { installSchema as bundledInstallSchema } from "./install-schema.js";
import { createFetchHandler } from "./fetch-handler.js";
import { normalizeUserModule, type NormalizedUserModule } from "./normalize.js";

type InstallSchema = typeof bundledInstallSchema;
type DbInternalModule = {
  _flushPendingMaskPolicy?: () => Record<string, readonly string[]> | null;
};

declare const globalThis: {
  __zsDispatch?: (
    rpc: Record<string, unknown>,
    name: string,
    input: unknown,
    ctx: unknown,
  ) => Promise<unknown>;
  [key: string]: unknown;
};

export interface DevEntryOptions {
  /**
   * Loader for the user module. Called per request in dev so HMR
   * invalidations land naturally. Returns the module namespace
   * (i.e. the result of `runner.import(ENTRY)`).
   */
  loadUserModule: () => Promise<unknown>;
  /**
   * Resolve the live `env.db` handle. The dev-bootstrap previously
   * reached into `globalThis.__zs_env()`; this is a parameter so the
   * dev path doesn't hard-code a global. Returns `undefined` when the
   * DbPlugin isn't registered (skip-schema-install path).
   */
  getEnvDb: () => unknown;
  /**
   * Optional registry that the transform populates via
   * `globalThis.__register(wireId, fn)`. When provided, the
   * normalizer merges its entries last (last-write-wins for HMR).
   */
  registry?: Map<string, (input: unknown, ctx: unknown) => unknown>;
  /**
   * Optional `installSchema` loader. In dev mode the caller MUST
   * provide this so the dev path loads `installSchema` through the
   * same module-loader (e.g. Vite's ModuleRunner) that loads the
   * user's `t.*` builders. Without this, the dev path uses the
   * `@zeroship/bootstrap`-bundled `installSchema` which carries its
   * OWN `TypeBuilder` class — `instanceof TypeBuilder` checks inside
   * `validateRefTargets` / `normalizeSchema` then return `false`
   * for builders the user code constructed, and the schema install
   * silently produces empty Collection wrappers.
   *
   * Production callers (the runtime crate's bootstrap module) don't
   * use this entry — they invoke `installSchema` from a dynamic
   * import that goes through the bundle resolver, so identity is
   * preserved automatically.
   */
  getInstallSchema?: () => Promise<InstallSchema>;
  /**
   * Optional loader for `@zeroship/db/internal`. In dev mode the caller
   * should provide this through the same ModuleRunner that loaded the
   * user module so `defineMaskPolicy()` and `_flushPendingMaskPolicy()`
   * observe the same module instance.
   */
  getDbInternal?: () => Promise<DbInternalModule>;
  /**
   * Logger for dev-only diagnostics. Defaults to `console.log` /
   * `console.error`. Pass a no-op pair to silence the dev path.
   */
  logger?: { log: (msg: string) => void; error: (msg: string) => void };
}

export interface DevEntry {
  fetch: (request: Request, env: unknown, ctx: unknown) => Promise<Response>;
  rpc: (name: string, input: unknown, ctx: unknown) => unknown;
  /**
   * Test/runtime hook: reset the lazy schema-install latch. Called by
   * the dev-bootstrap when Vite's dep optimizer regenerated pre-
   * bundled files (schema must be re-registered against the fresh
   * runner because the new runtime instance carries a separate
   * `@zeroship/bootstrap` copy — see `instanceof` rationale below).
   */
  resetSchemaInstalled: () => void;
}

/**
 * Build the dev-mode `default` export. Returns the same `{ fetch, rpc }`
 * shape the runtime expects from a user module; the Vite plugin's
 * dev-bootstrap re-exports this verbatim.
 *
 * Per the ZS standard (`docs/reference/zs-standard.md`), `default.rpc`
 * is function-shape here — dev's namespace may change per request so
 * the dict is freshly resolved on every call. Production uses dict-
 * shape because the bundle is frozen.
 */
export function devEntry(options: DevEntryOptions): DevEntry {
  const log = options.logger?.log ?? ((m) => console.log(m));
  const logError = options.logger?.error ?? ((m) => console.error(m));

  // **P9 §8** — capture the platform-handle resolver NOW, at devEntry()
  // call time (dev-bootstrap module init). The production `runtime-entry`
  // that wraps the dev-bootstrap deletes `globalThis.__zsDbPlatform`
  // during ITS module evaluation — which runs AFTER this module's
  // top-level (ESM import hoisting) but BEFORE dev's lazy schema install
  // (first request). Capturing the reference here, module-locally (and
  // therefore invisible to user code, which lives in a separate module),
  // lets the lazy install still resolve the `__platform` handle after the
  // global is gone. `registerModel` / `setMaskPolicy` moved onto that
  // handle in P9 PR 4; without this capture, dev registration would
  // silently no-op.
  const platformResolver = (globalThis as unknown as {
    __zsDbPlatform?: (db: unknown) => unknown;
  }).__zsDbPlatform;

  // Module-local handle on the most recent install's `ready` promise.
  // Stage 6 of the @zeroship/db refactor replaced the cross-module
  // `globalThis.__zeroshipPlatformReady` with a per-isolate (per-
  // module-load) variable. HMR re-runs of `maybeRegisterSchema`
  // overwrite the handle in place so the second request awaits the
  // FRESH chain.
  let schemaReady: Promise<unknown> | undefined;

  // Set once per ModuleRunner lifetime — schema auto-discovery is
  // idempotent on the SDK side, but re-running registerModel for every
  // RPC dispatch is wasted work. The caller resets it via
  // `resetSchemaInstalled()` after a deps re-optimize (which forces
  // the runner to rebuild and re-imports the SDK copy).
  let schemaInstalled = false;

  async function loadNormalized(): Promise<NormalizedUserModule> {
    const mod = await options.loadUserModule();
    await maybeRegisterSchema(mod);
    return normalizeUserModule(mod, options.registry);
  }

  async function maybeRegisterSchema(mod: unknown): Promise<void> {
    if (schemaInstalled) return;
    const defaultExport =
      (mod && typeof mod === "object" && (mod as { default?: unknown }).default) || null;
    const schema =
      (defaultExport &&
        typeof defaultExport === "object" &&
        (defaultExport as { schema?: unknown }).schema &&
        typeof (defaultExport as { schema?: unknown }).schema === "object")
        ? (defaultExport as { schema: unknown }).schema
        : undefined;

    if (!schema) {
      schemaInstalled = true;
      return;
    }

    try {
      const envDb = options.getEnvDb();
      if (!envDb) {
        logError(
          `[zeroship:dev] schema registration skipped: env.db not available — ` +
            `is the DbPlugin registered on this runtime?`,
        );
        schemaInstalled = true;
        return;
      }
      // Use the caller-supplied loader when available so the install
      // path runs through the SAME module-loader (Vite's ModuleRunner
      // in dev) that loaded the user's `t.*` builders. Identity-
      // matched TypeBuilder is required for `instanceof` checks in
      // validateRefTargets / normalizeSchema to recognise user-side
      // type builders. Falls back to the bundled `installSchema` when
      // no loader is provided (e.g. unit tests).
      const installSchema = options.getInstallSchema
        ? await options.getInstallSchema()
        : bundledInstallSchema;
      // **P9 §8** — resolve the `__platform` handle via the captured
      // resolver (the global may already be deleted by the production
      // runtime-entry; the module-local capture survives). Hand it to
      // `installSchema` so `registerModel` routes through `__platform`.
      const platform =
        typeof platformResolver === "function" ? platformResolver(envDb) : undefined;
      const { ready } = installSchema(
        schema as Parameters<typeof installSchema>[0],
        envDb as Parameters<typeof installSchema>[1],
        { platform } as Parameters<typeof installSchema>[2],
      );
      schemaReady = (async () => {
        await ready;
        const policyMod = options.getDbInternal
          ? await options.getDbInternal()
          : await import("@zeroship/db/internal") as DbInternalModule;
        const pending = typeof policyMod._flushPendingMaskPolicy === "function"
          ? policyMod._flushPendingMaskPolicy()
          : null;
        if (pending) {
          const setMaskPolicy = (platform as { setMaskPolicy?: unknown } | undefined)?.setMaskPolicy;
          if (typeof setMaskPolicy === "function") {
            await (setMaskPolicy as (
              this: typeof platform,
              p: Record<string, readonly string[]>,
            ) => Promise<unknown>).call(platform, pending);
          }
        }
      })();
      log(`[zeroship:dev] registered schema from default-export`);
    } catch (e) {
      const err = e as { message?: string };
      logError(`[zeroship:dev] schema registration failed: ${err?.message ?? e}`);
    } finally {
      schemaInstalled = true;
    }
  }

  async function dispatchRpcAsync(name: string, input: unknown, ctx: unknown): Promise<unknown> {
    // Re-import per call so HMR invalidations land naturally. On the
    // FIRST call this also triggers schema registration — schemaReady
    // gets populated here.
    const normalized = await loadNormalized();

    // Await schema-readiness AFTER `loadNormalized` had a chance to
    // populate `schemaReady`. Reading it BEFORE the import would
    // observe `undefined` on the first call. On the cold path,
    // `registerModel` may still be installing schema state when the
    // first RPC tries to begin a transaction, so gate here. No-op on
    // the warm path.
    if (schemaReady && typeof schemaReady.then === "function") {
      try { await schemaReady; } catch { /* surfaces via the handler */ }
    }

    const dispatch = globalThis.__zsDispatch;
    if (typeof dispatch !== "function") {
      // Should never happen — `import "./dispatcher.js"` above
      // installs `__zsDispatch` at module-init.
      throw Object.assign(new Error("__zsDispatch is not installed"), {
        status: 500,
        code: "INTERNAL",
      });
    }
    return dispatch(normalized.rpc, name, input, ctx);
  }

  // Non-async RPC dispatcher — must NOT be declared `async`. For stream /
  // subscription procedures the kernel expects to receive the AsyncIterator
  // synchronously so its FallThrough path fires and routes the request to
  // `default.fetch` (which owns the SSE encoding via `createFetchHandler`).
  // An `async function` always wraps returns in a Promise; the kernel's
  // promise-settle path would then see `Promise<AsyncIterator>` and surface
  // "AsyncIterator from a Promise — unsupported" instead.
  //
  // Fast-sync path: look up the handler in the transform-populated registry.
  // `__register(wireId, fn)` is appended to every server module by the Vite
  // transform, so the registry is populated after the first module evaluation.
  // Any call that arrives before the first evaluation (registry empty) falls
  // back to the async path, which also fails for streams on first call — the
  // smoke always makes several non-stream RPC calls first, so the module is
  // already evaluated by the time subscribeTodos is called in practice.
  function dispatchRpc(name: string, input: unknown, ctx: unknown): unknown {
    const regFn = options.registry?.get(name) as ((input: unknown, ctx: unknown) => unknown) | undefined;
    if (regFn) {
      const regFnMeta = regFn as { kind?: string; config?: { kind?: string } };
      // Legacy `fn.config = { id: "..." }` assignments replace the
      // wrapper-attached config object, which can drop `config.kind`
      // for stream/subscription procedures. The SSR-hook patch also
      // plants a stable top-level `.kind`; honor it first so the dev
      // stream fast-path survives config replacement.
      const kind = regFnMeta.kind ?? regFnMeta.config?.kind;
      if (kind === "stream" || kind === "subscription") {
        // Return the AsyncIterator synchronously — the kernel's FallThrough
        // path routes to default.fetch (createFetchHandler) which handles
        // SSE encoding. The handler is re-invoked there via __zsDispatch;
        // calling it here (discarded) is benign for `async function*`.
        return regFn(input, ctx);
      }
    }
    // Non-streaming procedures: use the async path (returns a Promise).
    return dispatchRpcAsync(name, input, ctx);
  }

  const fetchHandler = createFetchHandler(loadNormalized);

  return {
    fetch: fetchHandler,
    rpc: dispatchRpc,
    resetSchemaInstalled: () => {
      schemaInstalled = false;
      schemaReady = undefined;
    },
  };
}
