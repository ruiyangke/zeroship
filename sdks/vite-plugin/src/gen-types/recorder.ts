/**
 * Migration recorder — the generated-source front-end.
 *
 * Records a committed `.ts` migration into a pure-JS IR envelope
 * (`{ ir_version, name, ops }`) using the standalone `zero-migrate` recorder
 * (`zero-migrate/internal/recorder`). No CLI subprocess, no `zeroship-runtime`
 * authoring vector — plain builder calls in the plugin's own Node engine.
 *
 * A migration `.ts` cannot be `import()`ed directly on plain Node (TS syntax, and
 * its `zero-migrate` / `@zeroship/migrate` DSL import must resolve to the SAME
 * module instance the recorder drains from — a duplicated DSL module would drain
 * an empty op list). So we esbuild-bundle each migration to a temp `.mjs`,
 * marking the DSL package **external** (Node then resolves the single installed
 * `zero-migrate` at import time) and **aliasing** the legacy `@zeroship/migrate`
 * specifier onto `zero-migrate` so both authoring spellings converge on one
 * recorder singleton. The bundled module is then imported and drained through
 * `buildEnvelope`.
 */

import { promises as fs } from "node:fs";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import { pathToFileURL } from "node:url";
import { build } from "esbuild";

import {
  buildEnvelope,
  deriveNameFromPath,
  type IrEnvelope,
  type MigrationModule,
} from "zero-migrate/internal/recorder";

import { loadMigrateAddon } from "./addon.js";
import { bundleNodePaths, bundleTmpDir } from "./tmp.js";

/** The `<14-digit>_<desc>.ts` migration filename grammar (desc = [A-Za-z0-9_]+). */
const MIGRATION_TS_RE = /^(\d{14})_([A-Za-z0-9_]+)\.ts$/;

/** One discovered migration source, version-ordered. */
export interface DiscoveredMigration {
  /** The 14-digit version prefix. */
  version: string;
  /** The filename stem (without `.ts`). */
  stem: string;
  /** The absolute path to the `.ts` source. */
  path: string;
}

/**
 * Discover `.ts` migrations under `migrationsDir`, sorted by their 14-digit
 * version prefix (ties broken by stem). A non-`.ts` file is ignored; a `.ts` file
 * that violates the filename grammar is a hard error (an author typo must not be
 * silently skipped). A missing dir yields `[]`.
 */
export async function discoverMigrations(
  migrationsDir: string,
): Promise<DiscoveredMigration[]> {
  let names: string[];
  try {
    names = await fs.readdir(migrationsDir);
  } catch {
    return [];
  }
  const found: DiscoveredMigration[] = [];
  for (const name of names) {
    const m = MIGRATION_TS_RE.exec(name);
    if (!m) {
      if (name.endsWith(".ts")) {
        throw new Error(
          `gen-types: ${name} violates the <14-digit>_<desc>.ts migration filename grammar`,
        );
      }
      continue;
    }
    found.push({
      version: m[1],
      stem: name.slice(0, -3),
      path: join(migrationsDir, name),
    });
  }
  found.sort((a, b) =>
    a.version < b.version
      ? -1
      : a.version > b.version
        ? 1
        : a.stem < b.stem
          ? -1
          : 1,
  );
  return found;
}

/**
 * Record one migration `.ts` into an IR envelope. Bundles the source to a temp
 * `.mjs` (DSL external + `@zeroship/migrate` aliased onto `zero-migrate`),
 * imports it, and drains the recorded ops. The temp file is always cleaned up.
 */
export async function recordMigration(tsPath: string): Promise<IrEnvelope> {
  const irVersion = loadMigrateAddon().irVersion();
  // Emit the bundle UNDER the monorepo `node_modules` tree so Node's resolution
  // of the EXTERNAL `zero-migrate` at import time walks up to the one installed
  // instance (a `/tmp` output would have no node_modules chain).
  const outFile = join(await bundleTmpDir(), `zs-mig-${randomUUID()}.mjs`);
  try {
    await build({
      entryPoints: [tsPath],
      outfile: outFile,
      bundle: true,
      format: "esm",
      platform: "node",
      target: "node20",
      // Keep the DSL external so the bundled migration and the recorder resolve
      // to ONE installed `zero-migrate` instance (shared op-recorder singleton).
      external: ["zero-migrate", "zero-migrate/*"],
      // The legacy authoring spelling `@zeroship/migrate` converges on the same
      // installed `zero-migrate` package.
      alias: { "@zeroship/migrate": "zero-migrate" },
      nodePaths: bundleNodePaths(),
      logLevel: "silent",
    });
    const mod = (await import(pathToFileURL(outFile).href)) as MigrationModule;
    return buildEnvelope(mod, {
      irVersion,
      nameFallback: deriveNameFromPath(tsPath),
    });
  } finally {
    await fs.rm(outFile, { force: true });
  }
}

/**
 * Discover + record every migration under `migrationsDir`, in version order.
 * Returns the ordered envelope list the `genArtifacts` GENERATED source consumes.
 */
export async function recordMigrationsDir(
  migrationsDir: string,
): Promise<IrEnvelope[]> {
  const migrations = await discoverMigrations(migrationsDir);
  const envelopes: IrEnvelope[] = [];
  for (const m of migrations) {
    envelopes.push(await recordMigration(m.path));
  }
  return envelopes;
}
