/**
 * Temp-bundle location helper.
 *
 * The gen-types front-ends esbuild-bundle author `.ts` (migrations / `schema.ts`)
 * to a temp `.mjs` they then dynamically `import()`. When a bundle leaves a
 * package EXTERNAL (the recorder keeps `@zeroship/migrate` external so it shares the
 * one op-recorder singleton), Node resolves that external at import time by
 * walking up from the temp file's directory. A `/tmp` output has no
 * `node_modules` chain, so the external would fail to resolve. Emitting under
 * THIS package's `node_modules/.cache` guarantees the resolution walk reaches the
 * monorepo's installed packages.
 */

import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

let cached: string | undefined;

/** This package root (two dirs up from `dist|src/gen-types`). */
function packageRoot(): string {
  const here = dirname(fileURLToPath(import.meta.url));
  return join(here, "..", "..");
}

/**
 * A writable temp dir under this package's `node_modules/.cache`, created on
 * first use. Emitting here guarantees the bundled temp file's parent chain
 * includes the monorepo's installed packages (needed for the recorder's
 * EXTERNAL `@zeroship/migrate` to resolve at import time).
 */
export async function bundleTmpDir(): Promise<string> {
  if (cached) return cached;
  const dir = join(packageRoot(), "node_modules", ".cache", "zs-gen-types");
  await fs.mkdir(dir, { recursive: true });
  cached = dir;
  return dir;
}

/**
 * The `node_modules` dirs esbuild should add to its resolution search path, so a
 * `schema.ts` / migration `.ts` OUTSIDE the monorepo tree (a test fixture, an
 * app in an arbitrary location) still resolves `@zeroship/db` / `@zeroship/migrate`
 * against the installed packages. Both the package-local and the monorepo-root
 * `node_modules` are included.
 */
export function bundleNodePaths(): string[] {
  const root = packageRoot();
  return [
    join(root, "node_modules"),
    // Monorepo root node_modules (pnpm hoists here); harmless if absent.
    join(root, "..", "..", "node_modules"),
  ];
}
