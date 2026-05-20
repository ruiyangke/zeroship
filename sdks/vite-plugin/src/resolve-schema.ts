// sdks/vite-plugin/src/resolve-schema.ts
//
// Build-time DB schema resolver — shared between the Vite plugin and
// any future raw `.zship` builder. Stage 1 of the schema auto-discovery
// refactor (see `docs/proposals/db-schema-auto-discovery.md`): the
// resolver picks the user's schema module by convention (with an
// optional plugin-supplied override), and the build adapter writes
// the resolved path into `manifest.exports.schema`. Stage 2 wires the
// runtime read.
//
// This module is intentionally tiny — no parsing, no module
// evaluation, just file-existence checks against a fixed convention
// chain. Resolution must be deterministic and side-effect-free so
// CLIs (Vite, the eventual Rust CLI's JS sidecar, tests) all agree
// on the same path.

import { existsSync } from "node:fs";
import { isAbsolute, resolve as pathResolve } from "node:path";

/** Origin of the resolved path. Surfaced for build-time diagnostics. */
export type SchemaResolutionSource =
  | "option"
  | "src/schema.ts"
  | "src/schema/index.ts"
  | "entry-fallback";

export interface SchemaResolution {
  /**
   * Absolute path to the schema module, or `null` meaning "look at the
   * entry module's `default.schema` at runtime" (i.e. the legacy
   * convention where the schema lives next to the server handlers).
   */
  path: string | null;
  /** For build diagnostics — tells the user how the path was chosen. */
  source: SchemaResolutionSource;
}

/** File extensions tried for the convention chain, in priority order. */
const CONVENTION_EXTS = [".ts", ".js", ".mjs"] as const;

/**
 * Resolve the user's DB schema module.
 *
 * Resolution order:
 *   1. `opt` — caller-supplied path (e.g. plugin option). Must exist.
 *   2. `src/schema.{ts,js,mjs}` — single-file schema convention.
 *   3. `src/schema/index.{ts,js,mjs}` — directory-form convention.
 *   4. Fallback to runtime probe — return `{ path: null }`. The Stage 2
 *      bootstrap reads `default.schema` off the entry module.
 *
 * File-existence checks only. We intentionally never `import()` or
 * parse the schema file here — that's the runtime's job, and doing it
 * at build time would force the build to be able to load every
 * polyfill the runtime ships.
 *
 * @throws if `opt` is set but the file does not exist.
 */
export function resolveSchemaPath(root: string, opt?: string): SchemaResolution {
  if (opt !== undefined && opt !== "") {
    const abs = isAbsolute(opt) ? opt : pathResolve(root, opt);
    if (!existsSync(abs)) {
      throw new Error(
        `[zeroship] schema option "${opt}" resolved to ${abs} but no file exists there`,
      );
    }
    return { path: abs, source: "option" };
  }

  for (const ext of CONVENTION_EXTS) {
    const abs = pathResolve(root, `src/schema${ext}`);
    if (existsSync(abs)) {
      return { path: abs, source: "src/schema.ts" };
    }
  }

  for (const ext of CONVENTION_EXTS) {
    const abs = pathResolve(root, "src/schema", `index${ext}`);
    if (existsSync(abs)) {
      return { path: abs, source: "src/schema/index.ts" };
    }
  }

  return { path: null, source: "entry-fallback" };
}
