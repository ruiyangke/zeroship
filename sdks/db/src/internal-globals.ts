/**
 * Typed accessors for the SDK-internal globals shared between
 * `@zeroship/db`, the vite-plugin synthetic SSR entry, and the
 * dev-bootstrap. Centralising them here avoids three modules drifting
 * on stringly-named `(globalThis as any).…` casts.
 *
 * The names below are platform-internal and SHOULD NOT be read by
 * user code.
 *
 * - `__zeroshipPlatformReady` — Promise published by `_installSchema`
 *   that resolves once the chained `registerModel` DDL has settled.
 *   The auto-tx dispatcher awaits it before opening BEGIN so a per-
 *   connection-in-tx serialised pglite proxy doesn't deadlock against
 *   the orchestrator's advisory lock.
 *
 * Stage 4 of the schema auto-discovery refactor moved schema
 * registration into the runtime bootstrap. The synthetic SSR entry's
 * IIFE — which used to publish `__zsSchemaInit` so rolldown wouldn't
 * tree-shake the side effect — is gone, and so are this module's
 * `__zsSchemaInit` accessors and `SCHEMA_INIT_NAME` sentinel. The
 * `__zeroshipPlatformReady` chain still serializes the registerModel
 * DDL, but it's set synchronously during bootstrap evaluation now
 * (top-level await on `_installSchema` inside the runtime's
 * `db_init.js`), so the synthetic entry's auto-tx dispatcher's await
 * is a warm-path no-op.
 *
 * The exported name sentinels (`INSTALL_SCHEMA_NAME`,
 * `PLATFORM_READY_NAME`) let consumers who must reference the runtime
 * symbol by string (rolldown-emitted source, dynamic-import
 * resolution) pin a single source of truth.
 */

/**
 * The export name `_installSchema` carries through three boundaries:
 *
 *   1. `sdks/db/src/db.ts` — declares the function under this name.
 *   2. `sdks/db/src/index.ts` — re-exports it on the package main entry.
 *   3. `sdks/vite-plugin/src/{rpc-registry,dev-bootstrap}` — calls
 *      `dbSdk._installSchema` after a dynamic `import("@zeroship/db")`.
 *
 * The third site cannot statically link against the symbol (the
 * dev-bootstrap and the generated synthetic SSR entry both have to
 * tolerate a missing SDK at runtime), so the string lives in the
 * generated source. Future refactors that rename the function must
 * update this sentinel too — a CI grep on `INSTALL_SCHEMA_NAME`
 * surfaces both ends.
 */
export const INSTALL_SCHEMA_NAME = "_installSchema";

/**
 * Global-property name for the platform-ready promise. Consumers that
 * must reference the runtime symbol by string (the rolldown-emitted
 * synthetic SSR entry source — see `sdks/vite-plugin/src/rpc-registry.ts`)
 * interpolate this via `JSON.stringify(...)` so a rename surfaces at TS
 * compile time on both ends — dev-bootstrap reads through
 * `getPlatformReady` (typed); the generated worker source reads through
 * `globalThis[<sentinel>]` (stringly, but the string lives here).
 */
export const PLATFORM_READY_NAME = "__zeroshipPlatformReady";

/** @internal — typed handle on the SDK-internal platform-ready global. */
declare global {
  // eslint-disable-next-line no-var
  var __zeroshipPlatformReady: Promise<unknown> | undefined;
}

/** Read the platform-ready promise. `undefined` until first `_installSchema`. */
export function getPlatformReady(): Promise<unknown> | undefined {
  return globalThis.__zeroshipPlatformReady;
}

/** Replace the platform-ready promise (chained by `_installSchema`). */
export function setPlatformReady(p: Promise<unknown>): void {
  globalThis.__zeroshipPlatformReady = p;
}
