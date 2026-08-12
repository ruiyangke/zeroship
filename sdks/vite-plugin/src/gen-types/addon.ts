/**
 * `zero-migrate-node` addon loader — the single place the gen-types orchestrator
 * requires the napi `genArtifacts` verb.
 *
 * The addon is a napi-rs package whose `index.js` locates its prebuilt
 * `*.node` binary next to itself (or via `NAPI_RS_NATIVE_LIBRARY_PATH`). In a
 * *published* install the binary ships in the package tarball, so a bare
 * `require("zero-migrate-node")` resolves it.
 *
 * The paragraph that stood here described dev as "this monorepo linking the
 * standalone repo via a `file:` dep", whose tarball-pack omitted the gitignored
 * `*.node` and so broke the bare require. That mechanism is GONE: e4ad10373
 * moved both engine deps to `workspace:*` (verified at package.json:64-65, and
 * no `file:` spec for them survives anywhere in the tree). pnpm resolves a
 * workspace member by SYMLINK into third_party/zero-migrate/, not by packing a
 * tarball, so dev now points at the standalone tree in place - binary included.
 *
 * The fallback below is kept for the cases the symlink does not cover, not for
 * the one it replaced. Do not reason about dev resolution from the old premise.
 *
 * This loader makes both cases work with no build-graph coupling: it first tries
 * the ordinary require, and on failure re-points napi's own
 * `NAPI_RS_NATIVE_LIBRARY_PATH` env hook at the `*.node` the standalone repo built
 * in place, then retries. When the addon is eventually extracted/published this
 * file's fallback simply never fires — a move, not an untangle.
 */

import { createRequire } from "node:module";
import { existsSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";

// The addon's own generated declarations, not a copy of them.
//
// These used to be hand-mirrored here, and that turned every REQUIRED field the
// engine adds into a runtime failure at a creator's build instead of a red
// build in CI: our TypeScript never touched the engine's types, so a source we
// no longer satisfied still compiled. That has happened - `charterLayers`
// became required, this file still passed `policyCeilingToml`, and every
// gen-types call failed at run time.
//
// The runtime error is worse than "it fails later", which is why importing
// matters more than it looks. Measured by the engine's authors against the
// built addon: a source missing BOTH `dialect` and `charterLayers` reports
//
//     Missing field `dialect`
//
// and stops. No function name, no argument position, and no mention of the
// second missing field - the deserializer names the first and returns. Fix what
// it told you, rebuild, and you meet the next one. An N-field contract change
// becomes N builds, each looking like the last.
//
// tsc reports every missing property in one error instead, and does so
// regardless of `strict` - it is ordinary assignability, not a strictness
// feature - so a consumer who has never turned strict on still gets the whole
// list. The engine exports these types (`types: index.d.ts`, and the file is in
// `files`), so the mirror was a choice rather than a constraint.
import type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  GenArtifactsReply as AddonGenArtifactsReply,
  GenArtifactsSource,
  IndexDescriptorDto,
  RuntimeOptionsDto,
  ApplyIrSqliteRequest,
  ApplyReply,
} from "zero-migrate-node";

export type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  GenArtifactsSource,
  IndexDescriptorDto,
  RuntimeOptionsDto,
  ApplyIrSqliteRequest,
  ApplyReply,
};

const require = createRequire(import.meta.url);

/**
 * The reply gen-types consumes (or a soft error).
 *
 * The addon also returns an `envDbTs`, deliberately NOT declared here: it
 * re-authors the folded IR in the *migration* DSL (`from "zero-migrate"`,
 * `t.text().primaryKey()`), which is the engine's own artifact and neither
 * resolvable nor type-bearing in a creator app. gen-types renders the typed
 * `env.db` surface itself from `runtimeJson` (see `render-env-db.ts`).
 *
 * WHAT THIS DOES NOT CATCH, because `Pick` enumerates: a field the engine ADDS
 * to `GenArtifactsReply` is invisible here until someone edits this line. That
 * is the one protection the type-only import does NOT buy for this type - the
 * seven types re-exported verbatim above do get it. Stated because the engine
 * told us to expect exactly that (`hasDialectalOps`, absent at pin cb1bcb59)
 * and predicted it would "appear without anyone editing anything"; it will not.
 *
 * What the narrowing DOES buy, and the reason it stays: if a picked field
 * changes shape or is removed, that is a compile error rather than a runtime
 * `undefined`. Widening to the full reply to catch future additions would
 * re-admit `envDbTs`, which is excluded above for a stated reason - so the
 * deliberate trade is a known one-line edit when a needed field lands.
 */
export type GenArtifactsReply = Pick<AddonGenArtifactsReply, "ok" | "runtimeJson" | "error">;

/**
 * Closes the `Pick` blind spot described above WITHOUT widening the `Pick`.
 *
 * Every key of the engine's reply must be one this file has triaged: either
 * consumed by the `Pick`, or deliberately dropped (`envDbTs`). A key the engine
 * ADDS belongs to neither set, so `UntriagedReplyKeys` stops being `never` and
 * the `AssertNever` instantiation below fails to compile - naming the new key in
 * the error. That is the compile-time signal ticket #68 asked for; the `Pick`
 * alone could not give it, because `Pick` enumerates what it wants rather than
 * reacting to what arrives.
 *
 * Deleting a name from the exclusion list is what proves this load-bearing:
 * drop `"envDbTs"` and the build goes red with TS2344 naming `"envDbTs"`.
 * This is type-only and erases at compile time - it loads no addon binary.
 */
type UntriagedReplyKeys = Exclude<
  keyof AddonGenArtifactsReply,
  "ok" | "runtimeJson" | "error" | "envDbTs"
>;
type AssertNever<T extends never> = T;
export type _NoUntriagedAddonReplyKeys = AssertNever<UntriagedReplyKeys>;

/** The verbs the dev tier needs, so the addon's larger apply-side surface is
 *  not reachable from here.
 *
 *  `applyIrSqlite` is the dev tier's schema authority: the dev server applies
 *  the committed migrations to the dev SQLite file AHEAD of the worker, in
 *  authored order, exactly as `migrated` does for Postgres at deploy. The
 *  worker's `registerModel` then never renders the descriptor into DDL - see
 *  docs/proposals/2026-08-09-dev-sqlite-migration-apply-ahead-of-runtime.md.
 *
 *  Its request/reply types are IMPORTED from `zero-migrate-node`, never
 *  re-declared here. A hand-mirrored copy is how a field the engine adds turns
 *  into a runtime failure instead of a compile error. */
interface MigrateAddon {
  genArtifacts(source: GenArtifactsSource): GenArtifactsReply;
  irVersion(): number;
  applyIrSqlite(
    appPath: string,
    journalPath: string,
    req: ApplyIrSqliteRequest,
  ): Promise<ApplyReply>;
}

let cached: MigrateAddon | undefined;

/**
 * Load the `zero-migrate-node` addon, caching the module. Throws a descriptive
 * error (never a bare napi "Cannot find native binding") when neither the normal
 * resolution nor the standalone-build fallback locate the binary.
 */
export function loadMigrateAddon(): MigrateAddon {
  if (cached) return cached;

  try {
    cached = require("zero-migrate-node") as MigrateAddon;
    return cached;
  } catch (primary) {
    const bindingPath = findStandaloneBinding();
    if (bindingPath) {
      const prev = process.env.NAPI_RS_NATIVE_LIBRARY_PATH;
      process.env.NAPI_RS_NATIVE_LIBRARY_PATH = bindingPath;
      try {
        cached = require("zero-migrate-node") as MigrateAddon;
        return cached;
      } catch (retry) {
        throw addonLoadError(primary, retry, bindingPath);
      } finally {
        if (prev === undefined) delete process.env.NAPI_RS_NATIVE_LIBRARY_PATH;
        else process.env.NAPI_RS_NATIVE_LIBRARY_PATH = prev;
      }
    }
    throw addonLoadError(primary, undefined, undefined);
  }
}

/**
 * Locate the standalone repo's prebuilt `*.node` binary. napi-rs names it
 * `zero-migrate-node.<platform>-<arch>[-<abi>].node`. Returns the first such file
 * found, or `null` when none is found.
 *
 * THE "PUBLISHED INSTALL" ARM DESCRIBED BELOW DOES NOT EXIST YET, and this
 * comment used to assert it as the reason `null` is safe ("published install,
 * where the bare require already succeeded"). Measured 2026-08-12:
 * `zero-migrate-node` is NOT published and is not in `publish_packages` in
 * `deploy/scripts/publish-sdks.sh`, which REFUSES to publish
 * `@zeroship/vite-plugin` for exactly that reason -
 *   "2 dependency(ies) on workspace packages that are not published:
 *    @zeroship/vite-plugin [dependencies] zero-migrate-node@workspace:*"
 * So in a real registry install the bare `require` cannot succeed and this
 * fallback cannot find a binary either; the creator gets `addonLoadError`. The
 * monorepo arm below is the ONLY arm that works today. Tracked as the SDK
 * distribution blocker; do not read this loader as evidence that the published
 * path is covered.
 *
 * One candidate dir is scanned: the RESOLVED addon package dir
 * (`require.resolve("zero-migrate-node/…")`). In a published install that is
 * where the binary WOULD ship, next to `index.js` (unverified - see above); in
 * this monorepo the
 * `workspace:*` dep makes it a symlink straight to
 * `third_party/zero-migrate/crates/zero-migrate-node`, where a dev `napi build`
 * leaves the freshly-built `.node` in place. Verified 2026-08-10: it resolves to
 * that dir and `zero-migrate-node.linux-x64-gnu.node` is present.
 *
 * There used to be a second candidate — the `file:` dep TARGET dir, recovered by
 * walking up for a `package.json` declaring `zero-migrate-node: file:<path>`. It
 * existed because pnpm packs `file:` deps by their `files` list, which omits the
 * gitignored `.node`, so the store copy had no binary. `e4ad10373` replaced that
 * `file:` spec with `workspace:*`, which made the fallback both DEAD (no
 * `package.json` in the tree declares a `file:` spec, so it always returned
 * null) and UNNECESSARY (the symlink points at the live build dir). Deleted
 * rather than left as an unreachable branch.
 */
function findStandaloneBinding(): string | null {
  return scanForBinding(resolvedAddonDir());
}

/** Scan one dir for a `zero-migrate-node.*.node` file; return its path or null. */
function scanForBinding(dir: string | null): string | null {
  if (!dir || !existsSync(dir)) return null;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return null;
  }
  const hit = names.find(
    (f) => f.startsWith("zero-migrate-node.") && f.endsWith(".node"),
  );
  return hit ? join(dir, hit) : null;
}

/** The resolved `zero-migrate-node` package dir (a workspace symlink in dev). */
function resolvedAddonDir(): string | null {
  try {
    return dirname(require.resolve("zero-migrate-node/package.json"));
  } catch {
    return null;
  }
}

function addonLoadError(
  primary: unknown,
  retry: unknown,
  bindingPath: string | undefined,
): Error {
  const reasons = [
    `primary: ${(primary as Error)?.message ?? String(primary)}`,
    bindingPath ? `fallback (${bindingPath}): ${(retry as Error)?.message ?? String(retry)}` : null,
  ]
    .filter(Boolean)
    .join("; ");
  return new Error(
    "gen-types: failed to load the zero-migrate-node addon — the schema-artifact " +
      "emitter cannot run. Ensure the addon's native binary is built/installed. " +
      `(${reasons})`,
  );
}

/** Re-export for tests: whether a standalone binding path is discoverable. */
export { findStandaloneBinding as _findStandaloneBinding };
