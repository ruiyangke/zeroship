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
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// The addon's own generated declarations, not a copy of them.
//
// These used to be hand-mirrored here, and that turned every REQUIRED field the
// engine adds into a runtime failure at a creator's build instead of a red
// build in CI: our TypeScript never touched the engine's types, so a source we
// no longer satisfied still compiled and napi rejected it with "Missing field
// `<name>`". That has happened - `charterLayers` became required, this file
// still passed `policyCeilingToml`, and every gen-types call failed at run time.
// The engine exports these (`types: index.d.ts`, and the file is in `files`), so
// the mirror was a choice rather than a constraint.
import type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  GenArtifactsReply as AddonGenArtifactsReply,
  GenArtifactsSource,
  IndexDescriptorDto,
  RuntimeOptionsDto,
} from "zero-migrate-node";

export type {
  CollectionDescriptorDto,
  FieldDescriptorDto,
  GenArtifactsSource,
  IndexDescriptorDto,
  RuntimeOptionsDto,
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

/** The two verbs gen-types needs, so the addon's larger apply-side surface is
 *  not reachable from here. */
interface MigrateAddon {
  genArtifacts(source: GenArtifactsSource): GenArtifactsReply;
  irVersion(): number;
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
 * Two candidate dirs are scanned, in order:
 *  1. The RESOLVED addon package dir (`require.resolve("zero-migrate-node/…")`) —
 *     where a published/packed install ships the binary next to `index.js`.
 *  2. The `file:` dep TARGET dir — the standalone repo's
 *     `crates/zero-migrate-node`, where a dev `napi build` leaves the freshly-built
 *     `.node` **in place**. pnpm packs `file:` deps by their `files` list and the
 *     `.node` is gitignored/absent from that pack, so the store copy (dir 1) has no
 *     binary; the real one lives here. We recover this path by reading the
 *     consuming package's `package.json` `zero-migrate-node` `file:` spec (an
 *     absolute path in this monorepo's dev setup).
 */
function findStandaloneBinding(): string | null {
  for (const dir of standaloneBindingDirs()) {
    const hit = scanForBinding(dir);
    if (hit) return hit;
  }
  return null;
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

/** The ordered candidate dirs {@link findStandaloneBinding} scans (see its doc). */
function standaloneBindingDirs(): (string | null)[] {
  return [resolvedAddonDir(), fileDepTargetDir()];
}

/** Dir 1: the resolved `zero-migrate-node` package dir (the store copy in dev). */
function resolvedAddonDir(): string | null {
  try {
    return dirname(require.resolve("zero-migrate-node/package.json"));
  } catch {
    return null;
  }
}

/**
 * Dir 2: the `file:` dep target. Walk up from this module to the first
 * `package.json` that declares a `zero-migrate-node` dependency spelled
 * `file:<path>`, and return that `<path>` (resolved absolute). This is the
 * standalone repo's `crates/zero-migrate-node`, where the dev `napi build` leaves
 * the fresh `.node` in place.
 */
function fileDepTargetDir(): string | null {
  let dir = dirname(fileURLToPath(import.meta.url));
  for (let i = 0; i < 8; i++) {
    const pkgPath = join(dir, "package.json");
    if (existsSync(pkgPath)) {
      const spec = readFileDepSpec(pkgPath);
      if (spec) {
        return isAbsolute(spec) ? spec : resolve(dir, spec);
      }
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return null;
}

/** Read a `zero-migrate-node: file:<path>` dep spec from a package.json, or null. */
function readFileDepSpec(pkgPath: string): string | null {
  let json: { dependencies?: Record<string, string>; devDependencies?: Record<string, string> };
  try {
    json = JSON.parse(readFileSync(pkgPath, "utf8"));
  } catch {
    return null;
  }
  const dep = json.dependencies?.["zero-migrate-node"] ?? json.devDependencies?.["zero-migrate-node"];
  if (dep && dep.startsWith("file:")) return dep.slice("file:".length);
  return null;
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
