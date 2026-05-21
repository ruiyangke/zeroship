/**
 * Typed accessors for the two SDK-internal globals shared between
 * `@zeroship/db`, the vite-plugin synthetic SSR entry, and the
 * dev-bootstrap. Centralising them here avoids three modules drifting
 * on stringly-named `(globalThis as any).…` casts.
 *
 * Both names are platform-internal and SHOULD NOT be read by user code.
 *
 * - `__zeroshipPlatformReady` — Promise published by `_installSchema`
 *   that resolves once the chained `registerModel` DDL has settled.
 *   The auto-tx dispatcher awaits it before opening BEGIN so a per-
 *   connection-in-tx serialised pglite proxy doesn't deadlock against
 *   the orchestrator's advisory lock.
 *
 * - `__zsSchemaInit` — Promise the synthetic SSR entry's IIFE anchors
 *   so rolldown's static-folding step can't eliminate the side effect.
 *   The auto-tx dispatcher awaits it first (cold-start race: under
 *   auto-discovery `__zeroshipPlatformReady` is set INSIDE
 *   `_installSchema`, which itself runs inside the IIFE).
 *
 * The exported name sentinels (`INSTALL_SCHEMA_NAME`) let consumers
 * who must reference the runtime symbol by string (rolldown-emitted
 * source, dynamic-import resolution) pin a single source of truth.
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

/** @internal — typed handle on the two SDK-internal globals. */
declare global {
  // eslint-disable-next-line no-var
  var __zeroshipPlatformReady: Promise<unknown> | undefined;
  // eslint-disable-next-line no-var
  var __zsSchemaInit: Promise<unknown> | undefined;
}

/** Read the platform-ready promise. `undefined` until first `_installSchema`. */
export function getPlatformReady(): Promise<unknown> | undefined {
  return globalThis.__zeroshipPlatformReady;
}

/** Replace the platform-ready promise (chained by `_installSchema`). */
export function setPlatformReady(p: Promise<unknown>): void {
  globalThis.__zeroshipPlatformReady = p;
}

/** Read the schema-init IIFE handle. Set by the synthetic SSR entry. */
export function getSchemaInit(): Promise<unknown> | undefined {
  return globalThis.__zsSchemaInit;
}
