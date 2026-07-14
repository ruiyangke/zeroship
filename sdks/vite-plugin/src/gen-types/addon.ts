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

const require = createRequire(import.meta.url);

/**
 * The napi `genArtifacts` surface the orchestrator consumes. Kept to exactly the
 * two verbs gen-types needs so the addon's larger apply-side surface is not
 * accidentally reached here.
 */
export interface GenArtifactsSource {
  /** The GENERATED source: IR envelopes `{ ir_version, name, ops }`, version-ordered. */
  envelopes?: unknown[];
  /** The MANUAL source: declared `CollectionDescriptorDto`s. */
  descriptors?: CollectionDescriptorDto[];
  /** The project schema FK definitions thread; defaults to `"public"`. */
  projectSchema?: string;
}

/** The two co-emitted artifact strings (or a soft error). */
export interface GenArtifactsReply {
  ok: boolean;
  envDbTs?: string;
  runtimeJson?: string;
  error?: string;
}

/** One declared collection — the MANUAL-source `CollectionDescriptor` mirror. */
export interface CollectionDescriptorDto {
  name: string;
  ownerApp: string;
  fields: FieldDescriptorDto[];
  indexes?: IndexDescriptorDto[];
  runtimeOptions?: RuntimeOptionsDto;
}

/** One declared field — the MANUAL-source `FieldDescriptor` mirror. */
export interface FieldDescriptorDto {
  name: string;
  type: string;
  required?: boolean;
  unique?: boolean;
  references?: string;
  onDelete?: string;
  onUpdate?: string;
  deferrable?: boolean;
  default?: unknown;
  min?: number;
  max?: number;
  enum?: unknown[];
  idPrefix?: string;
  vectorDims?: number;
  vectorMetric?: string;
  caseSensitive?: boolean;
  encrypted?: unknown;
  mask?: unknown;
  fts?: boolean;
  ftsLanguage?: string;
  generated?: unknown;
  identity?: unknown;
}

/** One declared named index. */
export interface IndexDescriptorDto {
  name: string;
  columns: string[];
  unique?: boolean;
}

/** Per-collection runtime options. */
export interface RuntimeOptionsDto {
  softDelete?: boolean;
  versioning?: boolean;
  strictness?: string;
}

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
 * `zero-migrate-node.<platform>-<arch>[-<abi>].node` next to the addon's
 * `index.js`. We resolve the addon package dir via its `package.json` and scan
 * for the platform binary. Returns `null` when none is found (published install,
 * where the bare require already succeeded).
 */
function findStandaloneBinding(): string | null {
  let pkgJson: string;
  try {
    pkgJson = require.resolve("zero-migrate-node/package.json");
  } catch {
    return null;
  }
  const pkgDir = dirname(pkgJson);
  const candidates = readdirSync(pkgDir).filter(
    (f) => f.startsWith("zero-migrate-node.") && f.endsWith(".node"),
  );
  if (candidates.length > 0) return join(pkgDir, candidates[0]);
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
