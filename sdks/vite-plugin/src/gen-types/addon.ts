/**
 * `zero-migrate-node` addon loader — the single place the gen-types orchestrator
 * requires the napi `genArtifacts` verb.
 *
 * The addon is a napi-rs package whose `index.js` locates its prebuilt
 * `*.node` binary next to itself (or via `NAPI_RS_NATIVE_LIBRARY_PATH`). In a
 * *published* install the binary ships in the package tarball, so a bare
 * `require("zero-migrate-node")` resolves it. In *dev* (this monorepo linking the
 * standalone repo via a `file:` dep), the compiled `*.node` is gitignored in the
 * standalone tree, so pnpm's tarball-pack omits it and the bare require fails
 * with "Cannot find native binding".
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
 */
export type GenArtifactsReply = Pick<AddonGenArtifactsReply, "ok" | "runtimeJson" | "error">;

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
 * found, or `null` when none is found (published install, where the bare require
 * already succeeded).
 *
 * One candidate dir is scanned: the RESOLVED addon package dir
 * (`require.resolve("zero-migrate-node/…")`). In a published install that is
 * where the binary ships, next to `index.js`; in this monorepo the
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
